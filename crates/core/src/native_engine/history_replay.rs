//! Rebuild provider input from the same session view used on resume.
use super::*;

const MID_TURN_CONTINUATION_FOCUS: &str = "This automatic compaction runs mid-turn while the assistant is actively executing a task. Preserve the user's original request and constraints, all files modified or inspected, key tool outputs and test/verification results so far, unresolved errors, and the exact next concrete step so the assistant can continue executing immediately without waiting for another user message.";

const NATIVE_MID_TURN_CONTINUATION_PROMPT: &str = "Context was automatically compacted mid-turn. Continue executing the current task immediately from where you left off without waiting for a new user prompt.";

/// A selection from the executor's canonical model history, before wire rendering.
pub(in crate::native_engine) struct CompactionHistory(Vec<Value>);

// Host-provided history can be absent from the shard. Keep any later live
// messages outside that snapshot and reattach them after in-flight compaction.
struct ExplicitHistory {
    source: CompactionHistory,
    rendered_len: usize,
}

pub(super) enum MidTurnCompact {
    /// Mid-turn auto-compact is active and checked; context is below threshold.
    Managed,
    /// Mid-turn auto-compact is disabled, unavailable, or in cooldown.
    Unmanaged,
    /// Lightweight microcompact cleared older tool results and brought context below threshold.
    Synced,
    /// Full mid-turn compaction committed a durable boundary and rolled over the assistant segment.
    Compacted,
    /// The host closed the event channel during compaction; abort the turn cleanly.
    Cancelled,
}

fn format_compact_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs > 0 {
        format!("{secs}s")
    } else {
        "<1s".into()
    }
}

impl ToolLoopExecutor {
    pub(super) fn acknowledge_model_inputs(&mut self) -> Result<()> {
        if let Some(ExecutorHistory::Session(shard)) = &self.executor_history {
            shard.acknowledge_model_inputs(&self.pending_inputs)?;
        }
        self.pending_inputs.clear();
        Ok(())
    }

    pub(super) fn boundary_changed(&self) -> Result<bool> {
        match &self.executor_history {
            Some(ExecutorHistory::Session(shard)) => {
                Ok(shard.read(|r| r.boundary_id())? != self.context_boundary_id)
            }
            _ => Ok(false),
        }
    }

    pub(super) fn retain_replay_tool(
        &mut self,
        call: &PendingToolCall,
        input: &Value,
        output: &str,
        is_error: bool,
        request_id: Option<&str>,
    ) {
        let (Some(ExecutorHistory::Session(_)), Some(segment)) =
            (&self.executor_history, &self.parent_message_id)
        else {
            return;
        };
        let row = crate::db::message_repo::ToolCallRow {
            id: call.id.clone(),
            message_id: segment.clone(),
            name: call.name.clone(),
            input: Some(input.to_string()),
            output: Some(output.into()),
            is_error,
            sort_order: self.tool_order,
        };
        self.tool_order += 1;
        self.replay_tools.push(row);
        if let Some(request_id) = request_id {
            self.replay_request_links.insert(
                call.id.clone(),
                (request_id.to_owned(), Some(call.provider_id.clone())),
            );
        }
    }

    fn flush_replay_segment(
        &mut self,
        shard: &crate::db::session_store::SessionDb,
        text: Option<&str>,
    ) -> Result<()> {
        let Some(segment) = &self.parent_message_id else {
            return Ok(());
        };
        let text = text.filter(|value| !value.is_empty());
        if self.replay_tools.is_empty() && text.is_none() {
            return Ok(());
        }
        ExecutorHistory::Session(shard.clone()).save_segment(
            segment,
            crate::db::session_store::SegmentUpdate {
                text,
                tools: &self.replay_tools,
                request_links: Some(&self.replay_request_links),
                ..Default::default()
            },
        )?;
        self.replay_tools.clear();
        self.replay_request_links.clear();
        Ok(())
    }

    // The live loop appends complete tool exchanges. Keep its new rows with the
    // host boundary so subsequent admissions or pressure checks see that view.
    pub(super) fn retain_in_flight_additions<M: serde::Serialize>(
        &mut self,
        window: &[M],
    ) -> Result<()> {
        if self.in_flight_window.is_some() {
            for message in window
                .get(self.messages.len()..)
                .context("Live conversation lost its compacted prefix")?
            {
                self.messages.push(serde_json::to_value(message)?);
            }
        }
        Ok(())
    }

    pub(super) fn reload_history(
        &mut self,
        shard: &crate::db::session_store::SessionDb,
    ) -> Result<()> {
        self.reload_with_cut(shard, false).map(drop)
    }

    fn reload_with_cut(
        &mut self,
        shard: &crate::db::session_store::SessionDb,
        capture_cut: bool,
    ) -> Result<(
        Option<crate::db::session_store::CompactionWindow>,
        bool,
        Option<ExplicitHistory>,
    )> {
        self.flush_replay_segment(shard, None)?;
        let (mut messages, boundary, (index, entries), (protected, pending), cut) =
            shard.read(|r| {
                let (mut messages, boundary) =
                    r.model_history(&self.provider.provider.api_format)?;
                if let Some(window) = &mut self.in_flight_window {
                    let mut live = self.messages.clone();
                    for mut row in messages {
                        if row["role"] == "user"
                            && row["id"].as_str() != boundary.as_deref()
                            && row["id"]
                                .as_str()
                                .is_some_and(|id| window.shard_users.insert(id.to_owned()))
                        {
                            row["content"] = match self.provider.provider.api_format {
                                ApiFormat::OpenAI => serde_json::to_value(
                                    multimodal::openai_content(&row["content"])?,
                                )?,
                                ApiFormat::Anthropic => serde_json::to_value(
                                    multimodal::anthropic_content(&row["content"])?,
                                )?,
                            };
                            live.push(row);
                        }
                    }
                    messages = live;
                }
                let context = crate::services::turn_context::replay_at(
                    shard,
                    r,
                    boundary.as_deref(),
                    &messages,
                )?;
                let protection = r.protected_inputs(&messages)?;
                let cut = (capture_cut && self.in_flight_window.is_none())
                    .then(|| {
                        r.compaction_window(
                            &messages,
                            protection.0,
                            self.parent_message_id.as_deref(),
                            &self.provider.provider.api_format,
                        )
                    })
                    .transpose()?;
                Ok((messages, boundary, context, protection, cut))
            })?;
        let explicit_len = if capture_cut
            && crate::services::compaction::native::supports(&self.provider)
            && !messages.starts_with(&self.messages)
        {
            Some(self.build_openai_messages()?.len())
        } else {
            None
        };
        let microcompact_changed =
            if self.standalone_frontend && self.in_flight_window.is_none() {
                crate::services::compaction::microcompact(
                    shard,
                    boundary.as_deref(),
                    &mut messages,
                    self.provider.model.context_window,
                )?
            } else {
                false
            };
        self.protected_input = protected;
        self.pending_inputs = pending;
        if boundary.is_some() {
            self.session_reminders.git = None;
        }
        self.session_reminders.resident_indexes = index;
        self.session_reminders.skills =
            crate::services::turn_context::invoked_skills_reminder(&entries, &messages);
        self.context_reminders =
            crate::prompt::first_message_reminders(self.session_reminders.clone());
        self.turn_context = entries;
        let previous = std::mem::replace(&mut self.messages, messages);
        let explicit = explicit_len.map(|rendered_len| ExplicitHistory {
            source: CompactionHistory(previous),
            rendered_len,
        });
        self.applied_compaction = boundary.is_some();
        if boundary.is_some() {
            self.shell_cwd_needs_report
                .store(true, std::sync::atomic::Ordering::Release);
            self.shell_redundant_cd_cooldown
                .store(0, std::sync::atomic::Ordering::Release);
        }
        self.context_boundary_id = boundary;
        Ok((cut, microcompact_changed, explicit))
    }

    fn build_mid_turn_summary_wire<M: serde::Serialize>(
        &self,
        window: &[M],
    ) -> Result<super::super::followup::WireRequest> {
        let raw = window
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<Vec<_>>>()?;
        let prepared = self.prepare_request()?;
        match self.provider.provider.api_format {
            ApiFormat::Anthropic => {
                let messages: Vec<AnthropicMessage> = raw
                    .into_iter()
                    .map(serde_json::from_value)
                    .collect::<serde_json::Result<_>>()?;
                let body = self.anthropic_client.request_body(
                    &self.provider,
                    messages,
                    prepared.system_prompt.as_deref(),
                    prepared.tools,
                    self.max_tokens,
                    matches!(self.reasoning_mode, ReasoningMode::Deep),
                )?;
                Ok(super::super::followup::WireRequest {
                    api: "anthropic-messages".into(),
                    body,
                    builtin_vision: None,
                })
            }
            ApiFormat::OpenAI => {
                let messages: Vec<OpenAIMessage> = raw
                    .into_iter()
                    .map(serde_json::from_value)
                    .collect::<serde_json::Result<_>>()?;
                self.openai_client.request_body(
                    &self.provider,
                    messages,
                    prepared.system_prompt.as_deref(),
                    prepared.tools,
                    self.max_tokens,
                    self.conv_id.as_deref(),
                )
            }
        }
    }

    fn opaque_responses_window(
        &self,
        history: &[Value],
        system: &str,
        session: &str,
    ) -> Result<Option<super::super::followup::WireRequest>> {
        history
            .first()
            .filter(|m| m["provider_content"]["compaction"] == true)
            .map(|boundary| -> Result<_> {
                let mut body = super::super::provider_runtime::messages::request(
                    "openai-responses",
                    &self.provider,
                    &Value::Null,
                    vec![serde_json::from_value(boundary.clone())?],
                    Some(system),
                    vec![],
                    0,
                )?;
                anyhow::ensure!(
                    body["input"] == boundary["provider_content"]["items"]
                        && body["input"]
                            .as_array()
                            .is_some_and(|items| items.iter().any(|i| i["type"] == "compaction"
                                && i["encrypted_content"]
                                    .as_str()
                                    .is_some_and(|v| !v.is_empty()))),
                    "Cannot replay the saved opaque Responses window; original history retained"
                );
                if let Some(key) = super::super::request_identity::prompt_cache_key(Some(session)) {
                    body["prompt_cache_key"] = key.into();
                }
                Ok(super::super::followup::WireRequest {
                    api: "openai-responses".into(),
                    body,
                    builtin_vision: Some(false),
                })
            })
            .transpose()
    }

    fn estimated_input_tokens(&self, estimated_messages: u64) -> u64 {
        let reported_plus_delta = self
            .last_reported_input_tokens
            .map(|reported| {
                let delta = self
                    .last_request_message_tokens
                    .map(|prev| estimated_messages.saturating_sub(prev))
                    .unwrap_or(0);
                reported.saturating_add(delta)
            })
            .unwrap_or(0);
        reported_plus_delta
            .max(estimated_messages.saturating_add(self.last_request_overhead_tokens))
    }

    async fn check_uncompressed_request<M: serde::Serialize>(
        &mut self,
        cause: &str,
        window: &[M],
    ) -> Result<()> {
        if self.event_tx.is_closed() {
            anyhow::bail!("Compaction cancelled");
        }
        let wire = self.build_mid_turn_summary_wire(window)?;
        let model = &self.provider.model;
        let usage = super::super::request_context::ContextUsage::from_body(
            &wire.api,
            &wire.body,
            model.context_window,
            self.max_tokens,
        );
        let rows = window
            .iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<Vec<_>>>()?;
        let estimated = self
            .estimated_input_tokens(TokenCounter::estimate(&rows) as u64)
            .max(usage.text_subtotal());
        let limit = model
            .context_window
            .map(u64::from)
            .unwrap_or_else(|| TokenCounter::get_model_context_limit(&model.id) as u64);
        let capacity = limit.saturating_sub(u64::from(self.max_tokens));
        if estimated > capacity {
            anyhow::bail!("Automatic compaction failed. The next request needs about {estimated} input tokens and only {capacity} fit. Resume and run /compact or select a model with a larger context window. Cause: {cause}");
        }
        if !self.auto_compaction_warned {
            let _ = self
                .event_tx
                .send(EngineEvent::CompactionNotice(format!(
                    "Automatic compaction failed: {cause}. Continuing with the existing context."
                )))
                .await;
            self.auto_compaction_warned = true;
        }
        Ok(())
    }

    /// Check the session's `/autocompact` threshold between tool iterations.
    /// When crossed, try lightweight `microcompact` first; if context still
    /// meets the threshold, run a real durable compaction (native or LLM) and
    /// roll over the active assistant segment so the turn continues seamlessly.
    pub(super) async fn maybe_mid_turn_auto_compact<
        M: serde::Serialize + serde::de::DeserializeOwned + Clone,
    >(
        &mut self,
        gate: &AutoCompactionGate,
        current_text: &str,
        window: &mut Vec<M>,
        durable_len: usize,
        render: fn(&Self, &[(Vec<String>, Vec<String>)]) -> Result<Vec<M>>,
    ) -> Result<MidTurnCompact> {
        // Pressure on a host window is handled by replay_window, which can
        // compact in memory without committing a boundary over unrelated rows.
        if self.in_flight_window.is_some() {
            return Ok(MidTurnCompact::Unmanaged);
        }
        let (Some(ExecutorHistory::Session(shard)), Some(_previous_assistant), Some(conv_id)) = (
            self.executor_history.clone(),
            self.parent_message_id.clone(),
            self.conv_id.clone(),
        ) else {
            return Ok(MidTurnCompact::Unmanaged);
        };
        if gate.blocking_failure().is_some() {
            self.flush_replay_segment(&shard, Some(current_text))?;
            return Ok(MidTurnCompact::Managed);
        }
        let prefs = shard
            .read(|r| r.setting(crate::db::session_store::settings::PREFERENCES))?
            .unwrap_or_default();
        let Some(context_window) = self
            .provider
            .model
            .context_window
            .filter(|value| *value > 0)
            .map(u64::from)
        else {
            return Ok(MidTurnCompact::Unmanaged);
        };
        let Some(threshold) = prefs.autocompact_tokens(context_window) else {
            return Ok(MidTurnCompact::Unmanaged);
        };
        let values = |messages: &[M]| {
            messages
                .iter()
                .map(serde_json::to_value)
                .collect::<serde_json::Result<Vec<_>>>()
        };
        let rows = values(window)?;
        let estimated = self.estimated_input_tokens(TokenCounter::estimate(&rows) as u64);
        if estimated < threshold {
            return Ok(MidTurnCompact::Managed);
        }

        // Flush the active segment's tool calls and text before reloading so
        // both microcompact and full summarization see the latest progress.
        self.flush_replay_segment(&shard, Some(current_text))?;
        let additions = window.split_off(durable_len.min(window.len()));
        let (_, microcompact_changed, _) = self.reload_with_cut(&shard, false)?;
        *window = render(self, &self.message_additions())?;
        window.extend(additions.clone());

        if microcompact_changed {
            let post_micro_rows = values(window)?;
            let post_micro = (TokenCounter::estimate(&post_micro_rows) as u64)
                .saturating_add(self.last_request_overhead_tokens);
            self.last_reported_input_tokens = None;
            self.last_request_message_tokens = None;
            if post_micro < threshold {
                let _ = self
                    .event_tx
                    .send(EngineEvent::CompactionNotice(
                        "Cleared older tool results to save context.".into(),
                    ))
                    .await;
                return Ok(MidTurnCompact::Synced);
            }
        }

        if !gate.may_start() || self.messages.is_empty() {
            return Ok(if microcompact_changed {
                MidTurnCompact::Synced
            } else {
                MidTurnCompact::Unmanaged
            });
        }

        let started = std::time::Instant::now();
        let _ = self
            .event_tx
            .send(EngineEvent::CompactionNotice(format!(
                "Auto-compacting conversation (estimated {estimated} tokens, threshold {threshold})…"
            )))
            .await;

        let (expected_last, expected_boundary, summarized_count) =
            shard.read(|r| r.mid_turn_compaction_fence())?;
        let mut warnings = Vec::new();
        let mut focus = MID_TURN_CONTINUATION_FOCUS.to_owned();
        if let Some(hooks) = &self.hooks {
            match hooks
                .run(
                    crate::services::tool_hooks::HookEvent::PreCompact,
                    &conv_id,
                    json!({"trigger": "auto", "custom_instructions": Value::Null}),
                )
                .await
            {
                Ok(pre) => {
                    warnings.extend(pre.warnings);
                    for text in pre.additional_context {
                        focus.push_str("\n\n");
                        focus.push_str(&text);
                    }
                }
                Err(error) => {
                    let _ = self
                        .event_tx
                        .send(EngineEvent::CompactionNotice(format!(
                            "Mid-turn auto-compact skipped ({error:#}); continuing with existing context."
                        )))
                        .await;
                    gate.defer_threshold();
                    return Ok(if microcompact_changed {
                        MidTurnCompact::Synced
                    } else {
                        MidTurnCompact::Unmanaged
                    });
                }
            }
        }

        let data_root = self
            .search_config_path
            .as_deref()
            .and_then(crate::data_layout::data_root_of);
        let generation = async {
            let input = self.compaction_history(..);
            self.generate_auto_compaction(gate, &shard, input, window, &focus)
                .await
        };

        let outcome = tokio::select! {
            biased;
            _ = self.event_tx.closed() => None,
            result = Box::pin(generation) => Some(result),
        };
        let Some(result) = outcome else {
            return Ok(MidTurnCompact::Cancelled);
        };
        let (generated, mut record) = match result {
            Ok(pair) => pair,
            Err(_) => {
                // replay_window checks capacity on this and each skipped iteration.
                return Ok(MidTurnCompact::Synced);
            }
        };

        if self.event_tx.is_closed() {
            return Ok(MidTurnCompact::Cancelled);
        }

        let boundary_id = match shard.commit_mid_turn_compaction(
            expected_last.as_deref(),
            expected_boundary.as_deref(),
            &generated,
        ) {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(
                    module = "HistoryReplay",
                    "Mid-turn auto-compact commit skipped: {error:#}"
                );
                let _ = self
                    .event_tx
                    .send(EngineEvent::CompactionNotice(format!(
                        "Mid-turn auto-compact skipped ({error:#}); continuing with existing context."
                    )))
                    .await;
                gate.defer_threshold();
                return Ok(if microcompact_changed {
                    MidTurnCompact::Synced
                } else {
                    MidTurnCompact::Unmanaged
                });
            }
        };
        gate.committed_mid_turn();
        generated.log_boundary(&boundary_id);
        if let Some(record) = &mut record {
            record.finish(&generated, &boundary_id);
        }

        let mut summary_path = None;
        let mut export_error = None;
        if let Some(root) = data_root {
            match crate::services::compaction::export_summary(
                root,
                &conv_id,
                summarized_count,
                &generated.text,
            ) {
                Ok(path) => summary_path = Some(path.display().to_string()),
                Err(error) => export_error = Some(format!("{error:#}")),
            }
        }
        if let Some(hooks) = &self.hooks {
            match hooks
                .run(
                    crate::services::tool_hooks::HookEvent::PostCompact,
                    &conv_id,
                    json!({"trigger": "auto", "compact_summary": generated.text}),
                )
                .await
            {
                Ok(post) => warnings.extend(post.warnings),
                Err(error) => warnings.push(format!("PostCompact hook: {error:#}")),
            }
        }
        for warning in &warnings {
            let _ = self
                .event_tx
                .send(EngineEvent::CompactionNotice(format!("Hook: {warning}")))
                .await;
        }
        let duration = format_compact_duration(started.elapsed().as_millis() as u64);
        let mut lines = vec![format!(
            "Compacted in {duration}: {summarized_count} messages summarized; the original transcript remains in /history."
        )];
        if let Some(path) = summary_path {
            lines.push("Summary saved to:".into());
            lines.push(path);
        }
        if let Some(error) = export_error {
            lines.push(format!("Summary export failed: {error}"));
        }
        let notice = lines.join("\n");

        self.publish_context_reset(boundary_id, ContextResetReason::AutoCompact { notice })
            .await?;
        self.tool_order = 0;
        self.replay_tools.clear();
        self.replay_request_links.clear();
        self.stop_hook_active = false;
        self.last_reported_input_tokens = None;
        self.last_request_message_tokens = None;

        self.reload_history(&shard)?;
        self.mid_turn_continuation_cue = true;
        *window = render(self, &self.message_additions())?;
        self.mid_turn_continuation_cue = false;
        let is_native_boundary = self
            .messages
            .first()
            .and_then(|m| m.get("provider_content"))
            .is_some_and(|v| v["compaction"] == true);
        if is_native_boundary {
            window.push(serde_json::from_value(json!({
                "role": "user",
                "content": NATIVE_MID_TURN_CONTINUATION_PROMPT,
            }))?);
        }
        window.extend(additions);

        let post_compact_rows = values(window)?;
        let post_compact = (TokenCounter::estimate(&post_compact_rows) as u64)
            .saturating_add(self.last_request_overhead_tokens);
        if post_compact >= threshold * 90 / 100 {
            gate.defer_threshold();
        }
        Ok(MidTurnCompact::Compacted)
    }

    pub(super) fn reload_session_history(&mut self) -> Result<()> {
        let Some(ExecutorHistory::Session(shard)) = self.executor_history.clone() else {
            anyhow::bail!("Admitted input requires durable session history");
        };
        self.reload_history(&shard)
    }

    pub(in crate::native_engine) fn compaction_history(
        &self,
        cut: impl std::slice::SliceIndex<[Value], Output = [Value]>,
    ) -> CompactionHistory {
        CompactionHistory(self.messages[cut].to_vec())
    }

    /// The native endpoint and its fallback serialize the same frozen rows.
    /// Text-only providers retain their normal turn wire for cache-prefix replay.
    pub(in crate::native_engine) async fn generate_compaction_snapshot(
        &self,
        shard: &crate::db::session_store::SessionDb,
        history: CompactionHistory,
        summary_only_wire: Option<super::super::followup::WireRequest>,
        system: &str,
        focus: &str,
        has_main_turn: bool,
        mut options: super::super::request_options::RequestOptions,
    ) -> Result<crate::services::compaction::GeneratedSummary> {
        use crate::services::compaction::{checkpoint::Checkpoint, native};
        let mut rows = multimodal::normalize_messages(&history.0)?;
        rows.retain(|row| row["origin"] != "memory_change");
        let selected = shard.scoped_stores();
        options.compaction_metrics = Some(super::super::compaction_metrics::MetricsStore(
            shard.clone(),
        ));
        let (mut wire, native_result) = match summary_only_wire {
            Some(wire) => (wire, None),
            None => {
                // Durable rows lack the user-role host prefix used on main requests.
                // In-flight rows are already rendered, including this prefix.
                if self.in_flight_window.is_none() {
                    if let Some(first) = rows.iter().position(|row| row["role"] == "user") {
                        if let Some((offset, reminders)) = project_memory_context::boundary_reminders(
                            &self.context_reminders,
                            &rows[first]["provider_content"],
                        ) {
                            rows.insert(first + offset, serde_json::to_value(reminders)?);
                        }
                    }
                }
                self.remove_compaction_indexes(&mut rows);
                let wire = self.build_mid_turn_summary_wire(&rows)?;
                let result = native::generate(
                    &rows,
                    shard.id(),
                    &self.provider,
                    focus,
                    Some(selected.data_dir()),
                    options.clone(),
                )
                .await;
                (wire, Some(result))
            }
        };
        let summary = async {
            wire.allow_text_summary();
            let opaque = self.opaque_responses_window(&rows, system, shard.id())?;
            if opaque.is_some() {
                rows.remove(0);
            }
            let mut checkpoint = Checkpoint::open(&selected, shard.id(), &self.provider, system)?;
            checkpoint.options = options;
            super::super::generation::replay::summarize_replay(
                self.provider.clone(),
                system,
                rows,
                focus,
                has_main_turn,
                wire,
                opaque,
                &mut checkpoint,
            )
            .await
            .map(Into::into)
        };
        match native_result {
            Some(result) => native::with_summary_fallback(result, summary).await,
            None => summary.await,
        }
    }

    async fn generate_auto_compaction<M: serde::Serialize>(
        &self,
        gate: &AutoCompactionGate,
        shard: &crate::db::session_store::SessionDb,
        input: CompactionHistory,
        window: &[M],
        focus: &str,
    ) -> Result<(
        crate::services::compaction::GeneratedSummary,
        Option<super::super::request_recording::CompactionRecord>,
    )> {
        anyhow::ensure!(gate.may_start(), "Automatic compaction is in cooldown");
        let generation = async {
            let mut options = match self.provider.provider.api_format {
                ApiFormat::Anthropic => self.anthropic_client.request_options.clone(),
                ApiFormat::OpenAI => self.openai_client.request_options.clone(),
            }
            .compaction("auto");
            let record = super::super::request_recording::CompactionRecord::start(
                &mut options,
                "auto",
                &serde_json::to_string(&input.0)?,
            );
            let summary_only_wire = if crate::services::compaction::native::supports(&self.provider)
            {
                None
            } else {
                Some(self.build_mid_turn_summary_wire(window)?)
            };
            let generated = self
                .generate_compaction_snapshot(
                    shard,
                    input,
                    summary_only_wire,
                    crate::services::compaction::PROMPT,
                    focus,
                    true,
                    options,
                )
                .await?;
            Ok((generated, record))
        };
        tokio::select! {
            biased;
            _ = self.event_tx.closed() => anyhow::bail!("Interrupted during automatic compaction"),
            result = generation => result.inspect_err(|error| gate.record_failure(format!("{error:#}"))),
        }
    }

    // A live window can refresh its durable prefix. A summary already owns
    // a snapshot, possibly cut before a queued input or at a partial boundary.
    // Return true whenever the live conversation must adopt the refreshed window.
    pub(super) async fn replay_window<M: serde::Serialize + serde::de::DeserializeOwned>(
        &mut self,
        gate: &AutoCompactionGate,
        window: &mut Vec<M>,
        durable_len: Option<usize>,
        mid_turn_managed: bool,
        render: fn(&Self, &[(Vec<String>, Vec<String>)]) -> Result<Vec<M>>,
    ) -> Result<bool> {
        if let Some(cause) = gate.blocking_failure() {
            self.check_uncompressed_request(&cause, window).await?;
            return Ok(false);
        }
        // Respect threshold deferrals here too. A previous generation failure
        // already took the stricter uncompressed-capacity path above.
        let mid_turn_managed = mid_turn_managed || !gate.may_start();
        let model = &self.provider.model;
        let limit = model
            .context_window
            .map(|n| (n as f64 * 0.90) as usize)
            .unwrap_or_else(|| TokenCounter::get_safe_limit(&model.id, 0.90));
        let values = |messages: &[M]| {
            messages
                .iter()
                .map(serde_json::to_value)
                .collect::<serde_json::Result<Vec<_>>>()
        };
        let mut rows = values(window)?;
        let native = crate::services::compaction::native::supports(&self.provider);
        let threshold = crate::config::OrchestrationConfig::token_threshold();
        let capacity = model
            .context_window
            .map(|n| n as usize)
            .unwrap_or_else(|| TokenCounter::get_model_context_limit(&model.id))
            .saturating_sub(self.max_tokens as usize);
        let autocompact_off = match &self.executor_history {
            Some(ExecutorHistory::Session(shard)) => shard
                .read(|r| r.setting(crate::db::session_store::settings::PREFERENCES))?
                .is_some_and(|prefs| prefs.autocompact_off && prefs.autocompact_percent.is_none()),
            _ => false,
        };
        let estimated = TokenCounter::estimate(&rows);
        if autocompact_off || mid_turn_managed {
            if estimated <= capacity {
                return Ok(false);
            }
        } else if estimated <= (limit * 70 / 100).min(capacity)
            && (native || estimated <= threshold)
        {
            return Ok(false);
        }
        let mut physical_cut = None;
        let mut microcompact_changed = false;
        let mut persisted_len = window.len();
        let mut explicit_history = None;
        if let Some(live) = &self.in_flight_window {
            // Protect the newest complete exchange. If only the preceding
            // boundary could be summarized, keep sending it while it fits.
            let end = self
                .messages
                .iter()
                .rposition(|row| row["role"] == "assistant")
                .filter(|at| TokenCounter::estimate(&rows[*at..]) <= capacity)
                .unwrap_or(self.messages.len());
            if end <= live.boundary_len && estimated <= capacity {
                return Ok(false);
            }
            explicit_history = Some(ExplicitHistory {
                source: self.compaction_history(..end),
                rendered_len: end,
            });
        } else if let (Some(durable_len), Some(ExecutorHistory::Session(shard))) =
            (durable_len, self.executor_history.clone())
        {
            let additions = window.split_off(durable_len);
            let (cut, changed, explicit) = self.reload_with_cut(&shard, true)?;
            physical_cut = cut;
            microcompact_changed = changed;
            *window = render(self, &self.message_additions())?;
            persisted_len = window.len();
            // An explicit request can carry a window that the durable shard does
            // not contain (for example a replayed conversation). Compact that
            // in-flight window instead of rewriting the stored history.
            if let Some(history) = explicit {
                if rows[..durable_len] != values(window)? {
                    explicit_history = Some(history);
                }
            }
            window.extend(additions);
            if explicit_history.is_none() {
                rows = values(window)?;
            }
        }
        let compressor = ContextCompressor::new(None).with_overrides(&self.projected_results);
        let estimated = TokenCounter::estimate(&rows);
        if (autocompact_off || mid_turn_managed) && estimated <= capacity {
            return Ok(microcompact_changed);
        }
        let level = compressor.determine_compression_level(&ContextWindow {
            model_id: self.provider.model.id.clone(),
            total_tokens: estimated as u64,
            reserved_for_completion: (self.max_tokens as u64) * 2,
            context_window: limit as u64,
        });
        let tiered = !native && estimated > threshold;
        if level == CompressionLevel::None && !tiered && estimated <= capacity {
            return Ok(microcompact_changed);
        }

        if native || explicit_history.is_some() {
            if durable_len.is_none() {
                return Ok(false);
            }
            let Some(ExecutorHistory::Session(shard)) = self.executor_history.clone() else {
                anyhow::bail!("Native Responses compression requires durable session history");
            };
            if let Some(explicit) = explicit_history {
                let result = self
                    .generate_auto_compaction(
                        gate,
                        &shard,
                        explicit.source,
                        rows.get(..explicit.rendered_len)
                            .context("Explicit compaction prefix exceeds the live conversation")?,
                        MID_TURN_CONTINUATION_FOCUS,
                    )
                    .await;
                let (mut generated, mut record) = match result {
                    Ok(pair) => pair,
                    Err(error) => {
                        let cause = format!("{error:#}");
                        self.check_uncompressed_request(&cause, &rows).await?;
                        *window = rows
                            .into_iter()
                            .map(serde_json::from_value)
                            .collect::<serde_json::Result<_>>()?;
                        return Ok(true);
                    }
                };
                if let Some(record) = &mut record {
                    record.finish(&generated, "in-flight-native-window");
                }
                if let Some(native) = &mut generated.native {
                    prepare_native_window(
                        native,
                        self.search_config_path
                            .as_deref()
                            .and_then(crate::data_layout::data_root_of),
                        shard.id(),
                    );
                }
                // This host-only boundary is frozen in the live window, not the shard.
                if let Some(startup) = self.session_reminders.git.take() {
                    self.context_reminders.retain(|block| block != &startup);
                }
                let mut live = vec![json!({
                    "role":"user", "content":generated.text, "provider_content":generated.native,
                })];
                if let Some(git) = crate::services::git_snapshot::capture_boundary(
                    std::path::Path::new(&self.workspace_cwd),
                ) {
                    live.push(crate::services::compaction::git_row(git));
                }
                if let Some((at, reminders)) = project_memory_context::boundary_reminders(
                    &self.context_reminders,
                    &live[0]["provider_content"],
                ) {
                    live.insert(if at == 0 { 0 } else { live.len() }, serde_json::to_value(reminders)?);
                }
                let boundary_len = live.len();
                live.extend(rows.into_iter().skip(explicit.rendered_len));
                *window = live
                    .iter()
                    .cloned()
                    .map(serde_json::from_value)
                    .collect::<serde_json::Result<_>>()?;
                let state = self.in_flight_window.get_or_insert_with(|| InFlightWindow {
                    shard_users: self
                        .messages
                        .iter()
                        .filter(|row| row["role"] == "user")
                        .filter_map(|row| row["id"].as_str().map(str::to_owned))
                        .collect(),
                    boundary_len,
                });
                state.boundary_len = boundary_len;
                self.messages = live;
                gate.committed();
                return Ok(true);
            }
            let cut = physical_cut.context("Missing native compaction snapshot")?;
            let mut committed = false;
            if cut.prefix_len > 0 {
                // The Responses renderer can insert one standalone index after
                // an opaque boundary when no later receipt supersedes it.
                let extra_index = persisted_len.saturating_sub(self.messages.len());
                let end = cut.prefix_len + extra_index;
                anyhow::ensure!(end <= persisted_len, "Invalid native compaction cut");
                let mut suffix = rows.split_off(end);
                if TokenCounter::estimate(&suffix) > capacity {
                    crate::services::compaction::clear_tool_results(&mut suffix);
                    anyhow::ensure!(TokenCounter::estimate(&suffix) <= capacity,
                        "Context overflow: the current turn's admitted inputs exceed the model window");
                }
                let result = self
                    .generate_auto_compaction(
                        gate,
                        &shard,
                        self.compaction_history(..cut.prefix_len),
                        &window[..end],
                        self.system_prompt.as_deref().unwrap_or(""),
                    )
                    .await;
                let (generated, mut record) = match result {
                    Ok(pair) => pair,
                    Err(error) => {
                        let cause = format!("{error:#}");
                        self.check_uncompressed_request(&cause, window).await?;
                        return Ok(microcompact_changed);
                    }
                };
                let boundary = shard.commit_compaction_window(&cut, &generated)?;
                gate.committed();
                generated.log_boundary(&boundary);
                if let Some(record) = &mut record {
                    record.finish(&generated, &boundary);
                }
                let additions = window.split_off(persisted_len);
                self.reload_history(&shard)?;
                *window = render(self, &self.message_additions())?;
                window.extend(additions);
                committed = true;
            }
            let mut result = values(window)?;
            if TokenCounter::estimate(&result) > capacity {
                crate::services::compaction::clear_tool_results(&mut result);
                anyhow::ensure!(TokenCounter::estimate(&result) <= capacity,
                    "Context overflow: the admitted inputs and required context exceed the model window");
                *window = result
                    .into_iter()
                    .map(serde_json::from_value)
                    .collect::<serde_json::Result<_>>()?;
            }
            return Ok(committed || microcompact_changed);
        }

        // A generated text summary is a durable boundary, just like a native
        // Responses window. Assign the cut to an actual transcript row before
        // committing so a retained tool call and its results remain together.
        if let (Some(original), Some(ExecutorHistory::Session(shard))) =
            (physical_cut, self.executor_history.clone())
        {
            let end = self.protected_input.unwrap_or_else(|| {
                self.messages
                    .iter()
                    .rposition(|m| m["role"] == "user" && m["origin"] != "compaction_git")
                    .unwrap_or(self.messages.len())
            });
            if let Some(mut at) = compressor.retained_input(&self.messages, end, &level) {
                if TokenCounter::estimate(&rows[at..]) > capacity {
                    at = end;
                }
                let cut = shard.read(|r| {
                    r.compaction_window(
                        &self.messages,
                        (at < self.messages.len()).then_some(at),
                        self.parent_message_id.as_deref(),
                        &self.provider.provider.api_format,
                    )
                })?;
                anyhow::ensure!(
                    cut.fingerprint == original.fingerprint,
                    "Session changed during compaction"
                );
                if cut.prefix_len > 0 {
                    let mut prefix = self.messages[..cut.prefix_len].to_vec();
                    prefix.retain(|m| m["origin"] != "memory_change");
                    let text = compressor.window_summary(&prefix, &level).await;
                    shard.commit_compaction_window(&cut, &text.as_str().into())?;
                    gate.committed();
                    let tail = window.split_off(persisted_len);
                    self.reload_history(&shard)?;
                    *window = render(self, &self.message_additions())?;
                    window.extend(tail);
                    let mut rebuilt = values(window)?;
                    if TokenCounter::estimate(&rebuilt) > capacity {
                        crate::services::compaction::clear_tool_results(&mut rebuilt);
                        anyhow::ensure!(TokenCounter::estimate(&rebuilt) <= capacity,"Context overflow: the admitted inputs and required context exceed the model window");
                        *window = rebuilt
                            .into_iter()
                            .map(serde_json::from_value)
                            .collect::<serde_json::Result<_>>()?;
                    }
                    return Ok(true);
                }
            }
        }

        // Compress only the history preceding the current typed exchange. Its
        // multimodal blocks, receipts and any following steers travel together.
        let protected = self.protected_input;
        let cut = protected.unwrap_or(rows.len()).min(rows.len());
        let prefix_index = match &self.executor_history {
            Some(ExecutorHistory::Session(shard)) => shard.read(|r| {
                crate::services::memory_files::prompt::visible_indexes(
                    r,
                    self.context_boundary_id.as_deref(),
                    &self.messages,
                )
            })?,
            _ => self.session_reminders.resident_indexes.clone(),
        };
        // Repack the classified host prefix, not the first user's own text.
        // Remove it at its original rendering site before summarizing; a user's
        // later quotation of the same material is not a host attachment.
        if let Some(first) = rows.iter_mut().find(|m| m["role"] == "user") {
            if !self.context_reminders.is_empty() && first["provider_content"].is_null() {
                match &mut first["content"] {
                    Value::String(text) => {
                        let prefix = self.context_reminders.join("\n\n") + "\n\n";
                        if text.starts_with(&prefix) {
                            text.drain(..prefix.len());
                        }
                    }
                    Value::Array(parts)
                        if parts.len() >= self.context_reminders.len()
                            && parts.iter().zip(&self.context_reminders).all(|(p, block)| {
                                p["type"] == "text" && p["text"].as_str() == Some(block)
                            }) =>
                    {
                        parts.drain(..self.context_reminders.len());
                    }
                    _ => {}
                }
            }
        }
        let mut parts = self.session_reminders.clone();
        parts.resident_indexes = prefix_index;
        let mut suffix = rows.split_off(cut);
        self.remove_compaction_indexes(&mut rows);
        self.remove_compaction_indexes(&mut suffix);
        if protected.is_some() && TokenCounter::estimate(&suffix) > capacity {
            crate::services::compaction::clear_tool_results(&mut suffix);
            if durable_len.is_none() && TokenCounter::estimate(&suffix) > capacity {
                // The summary planner can split an input too large for a normal turn.
                return Ok(microcompact_changed);
            }
            anyhow::ensure!(
                TokenCounter::estimate(&suffix) <= capacity,
                "Context overflow: the current turn's admitted inputs exceed the model window"
            );
        }
        let mut compressed = if rows.is_empty() {
            rows
        } else if tiered && level == CompressionLevel::None {
            let result = crate::memory::TieredCompressor::new().compress(&rows);
            if rows == result && estimated <= capacity {
                return Ok(microcompact_changed);
            }
            result
        } else {
            let (result, metadata) = compressor
                .compress(rows.clone(), None, level, Some(&self.provider))
                .await;
            if metadata.was_compressed {
                result
            } else if estimated <= capacity {
                // An unchanged window keeps the normal request's cached prefix.
                return Ok(microcompact_changed);
            } else {
                rows
            }
        };
        if let Some(at) = compressed.iter().position(|row| row["_is_summary"] == true) {
            parts.git = None;
            if let Some(git) = crate::services::git_snapshot::capture_boundary(
                std::path::Path::new(&self.workspace_cwd),
            ) {
                compressed.insert(at + 1, crate::services::compaction::git_row(git));
            }
        }
        let prefix = crate::prompt::first_message_reminders(parts);
        let prefix_index =
            (!prefix.is_empty()).then(|| json!({"role":"user","content":prefix.join("\n\n")}));
        let index_at = prefix_index.map(|index| {
            let at = compressed
                .iter()
                .take_while(|row| row["role"] == "system")
                .count();
            compressed.insert(at, index);
            at
        });
        let suffix_start = compressed.len();
        compressed.extend(suffix);
        if protected.is_some() && TokenCounter::estimate(&compressed) > capacity {
            // Discard older exchanges before sacrificing current-turn tool results.
            compressed = compressed
                .into_iter()
                .enumerate()
                .filter_map(|(i, row)| {
                    (i >= suffix_start
                        || Some(i) == index_at
                        || row["role"] == "system"
                        || row["_is_summary"] == true
                        || row["origin"] == "compaction_git")
                        .then_some(row)
                })
                .collect();
            if TokenCounter::estimate(&compressed) > capacity {
                crate::services::compaction::clear_tool_results(&mut compressed);
                if durable_len.is_none() && TokenCounter::estimate(&compressed) > capacity {
                    return Ok(microcompact_changed);
                }
                anyhow::ensure!(TokenCounter::estimate(&compressed) <= capacity,
                    "Context overflow: the admitted inputs and required context exceed the model window");
            }
        }
        *window = compressed
            .into_iter()
            .map(serde_json::from_value)
            .collect::<serde_json::Result<_>>()?;
        Ok(microcompact_changed)
    }
}
