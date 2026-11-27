//! Rebuild provider input from the same session view used on resume.
use super::*;

const MAX_MID_TURN_COMPACTIONS: usize = 6;

const MID_TURN_CONTINUATION_FOCUS: &str = "This automatic compaction runs mid-turn while the assistant is actively executing a task. Preserve the user's original request and constraints, all files modified or inspected, key tool outputs and test/verification results so far, unresolved errors, and the exact next concrete step so the assistant can continue executing immediately without waiting for another user message.";

const NATIVE_MID_TURN_CONTINUATION_PROMPT: &str = "Context was automatically compacted mid-turn. Continue executing the current task immediately from where you left off without waiting for a new user prompt.";

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
                ..Default::default()
            },
        )?;
        self.replay_tools.clear();
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
    ) -> Result<(Option<crate::db::session_store::CompactionWindow>, bool)> {
        self.flush_replay_segment(shard, None)?;
        let (mut messages, boundary, (index, entries), (protected, pending), cut) =
            shard.read(|r| {
                let (messages, boundary) = r.model_history(&self.provider.provider.api_format)?;
                let context = crate::services::turn_context::replay_at(
                    shard,
                    r,
                    boundary.as_deref(),
                    &messages,
                )?;
                let protection = r.protected_inputs(&messages)?;
                let cut = capture_cut
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
        let microcompact_changed = if self.standalone_frontend {
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
        self.session_reminders.resident_indexes = index;
        self.session_reminders.skills =
            crate::services::turn_context::invoked_skills_reminder(&entries, &messages);
        self.context_reminders =
            crate::prompt::first_message_reminders(self.session_reminders.clone());
        self.turn_context = entries;
        self.messages = messages;
        self.applied_compaction = boundary.is_some();
        if boundary.is_some() {
            self.shell_cwd_needs_report
                .store(true, std::sync::atomic::Ordering::Release);
            self.shell_redundant_cd_cooldown
                .store(0, std::sync::atomic::Ordering::Release);
        }
        self.context_boundary_id = boundary;
        Ok((cut, microcompact_changed))
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

    /// Check the session's `/autocompact` threshold between tool iterations.
    /// When crossed, try lightweight `microcompact` first; if context still
    /// meets the threshold, run a real durable compaction (native or LLM) and
    /// roll over the active assistant segment so the turn continues seamlessly.
    pub(super) async fn maybe_mid_turn_auto_compact<
        M: serde::Serialize + serde::de::DeserializeOwned + Clone,
    >(
        &mut self,
        iteration: usize,
        current_text: &str,
        window: &mut Vec<M>,
        durable_len: usize,
        render: fn(&Self, &[(Vec<String>, Vec<String>)]) -> Result<Vec<M>>,
    ) -> Result<MidTurnCompact> {
        let (Some(ExecutorHistory::Session(shard)), Some(_previous_assistant), Some(conv_id)) = (
            self.executor_history.clone(),
            self.parent_message_id.clone(),
            self.conv_id.clone(),
        ) else {
            return Ok(MidTurnCompact::Unmanaged);
        };
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
        let estimated_messages = TokenCounter::estimate(&rows) as u64;
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
        let estimated = reported_plus_delta
            .max(estimated_messages.saturating_add(self.last_request_overhead_tokens));
        if estimated < threshold {
            return Ok(MidTurnCompact::Managed);
        }

        // Flush the active segment's tool calls and text before reloading so
        // both microcompact and full summarization see the latest progress.
        self.flush_replay_segment(&shard, Some(current_text))?;
        let additions = window.split_off(durable_len.min(window.len()));
        let (_, microcompact_changed) = self.reload_with_cut(&shard, false)?;
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

        if self.mid_turn_compactions >= MAX_MID_TURN_COMPACTIONS
            || iteration < self.mid_turn_compact_cooldown_until
            || self.messages.is_empty()
        {
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
                    self.mid_turn_compact_cooldown_until = iteration + 3;
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
        let native = crate::services::compaction::native::supports(&self.provider);
        let generation = async {
            if native {
                use crate::native_engine::request_recording::CompactionRecord;
                let (mut input, _) =
                    shard.read(|r| r.model_history(&self.provider.provider.api_format))?;
                input.retain(|row| row["origin"] != "memory_change");
                self.remove_compaction_indexes(&mut input);
                let mut options = self
                    .openai_client
                    .request_options
                    .clone()
                    .compaction("native");
                options.compaction_metrics = Some(
                    super::super::compaction_metrics::MetricsStore(shard.clone()),
                );
                let record = CompactionRecord::start(
                    &mut options,
                    "auto",
                    &serde_json::to_string(&input)?,
                );
                let generated =
                    crate::services::compaction::native::generate_messages_with_options(
                        &self.provider,
                        options,
                        input
                            .into_iter()
                            .map(serde_json::from_value)
                            .collect::<serde_json::Result<_>>()?,
                        &focus,
                        Some(&conv_id),
                        data_root,
                    )
                    .await?;
                Ok::<_, anyhow::Error>((generated, record))
            } else {
                use crate::native_engine::request_recording::CompactionRecord;
                let mut wire = self.build_mid_turn_summary_wire(window)?;
                wire.allow_text_summary();
                let mut durable = self.prepare_main_history()?;
                durable.retain(|row| row["origin"] != "memory_change");
                let opaque = self.opaque_responses_window(
                    &durable,
                    crate::services::compaction::PROMPT,
                    &conv_id,
                )?;
                if opaque.is_some() {
                    durable.remove(0);
                }
                let mut options = match self.provider.provider.api_format {
                    ApiFormat::Anthropic => self.anthropic_client.request_options.clone(),
                    ApiFormat::OpenAI => self.openai_client.request_options.clone(),
                }
                .compaction("auto");
                options.compaction_metrics = Some(
                    super::super::compaction_metrics::MetricsStore(shard.clone()),
                );
                let record = CompactionRecord::start(
                    &mut options,
                    "auto",
                    &serde_json::to_string(&durable)?,
                );
                let selected = shard.scoped_stores();
                let mut checkpoint = crate::services::compaction::checkpoint::Checkpoint::open(
                    &selected,
                    &conv_id,
                    &self.provider,
                    crate::services::compaction::PROMPT,
                )?;
                checkpoint.options = options;
                let text = super::super::generation::replay::summarize_replay(
                    self.provider.clone(),
                    crate::services::compaction::PROMPT,
                    durable,
                    &focus,
                    true,
                    wire,
                    opaque,
                    &mut checkpoint,
                )
                .await?;
                Ok((text.into(), record))
            }
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
            Err(error) => {
                tracing::warn!(
                    module = "HistoryReplay",
                    "Mid-turn auto-compact failed: {error:#}"
                );
                let _ = self
                    .event_tx
                    .send(EngineEvent::CompactionNotice(format!(
                        "Mid-turn auto-compact failed ({error:#}); continuing with existing context."
                    )))
                    .await;
                self.mid_turn_compact_cooldown_until = iteration + 3;
                return Ok(if microcompact_changed {
                    MidTurnCompact::Synced
                } else {
                    MidTurnCompact::Unmanaged
                });
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
                self.mid_turn_compact_cooldown_until = iteration + 3;
                return Ok(if microcompact_changed {
                    MidTurnCompact::Synced
                } else {
                    MidTurnCompact::Unmanaged
                });
            }
        };
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
        self.stop_hook_active = false;
        self.last_reported_input_tokens = None;
        self.last_request_message_tokens = None;
        self.mid_turn_compactions += 1;

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
            self.mid_turn_compact_cooldown_until = iteration + 3;
        }
        Ok(MidTurnCompact::Compacted)
    }

    pub(super) fn reload_session_history(&mut self) -> Result<()> {
        let Some(ExecutorHistory::Session(shard)) = self.executor_history.clone() else {
            anyhow::bail!("Admitted input requires durable session history");
        };
        self.reload_history(&shard)
    }

    async fn compact_native_window(
        &self,
        shard: &crate::db::session_store::SessionDb,
        cut: &crate::db::session_store::CompactionWindow,
        mut input: Vec<Value>,
    ) -> Result<()> {
        use crate::native_engine::request_recording::CompactionRecord;
        let mut options = self
            .openai_client
            .request_options
            .clone()
            .compaction("native");
        options.compaction_metrics = Some(super::super::compaction_metrics::MetricsStore(
            shard.clone(),
        ));
        let mut record =
            CompactionRecord::start(&mut options, "auto", &serde_json::to_string(&input)?);
        self.remove_compaction_indexes(&mut input);
        let compacted = crate::services::compaction::native::generate_messages_with_options(
            &self.provider,
            options,
            input
                .into_iter()
                .map(serde_json::from_value)
                .collect::<serde_json::Result<_>>()?,
            self.system_prompt.as_deref().unwrap_or(""),
            self.conv_id.as_deref(),
            self.search_config_path
                .as_deref()
                .and_then(crate::data_layout::data_root_of),
        )
        .await?;
        let boundary = shard.commit_compaction_window(cut, &compacted)?;
        if let Some(record) = &mut record {
            record.finish(&compacted, &boundary);
        }
        Ok(())
    }

    /// Native compaction for a request window that is not backed by the
    /// durable shard. It changes only this request.
    async fn compact_in_flight<M: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        mut input: Vec<Value>,
    ) -> Result<Vec<M>> {
        use crate::native_engine::request_recording::CompactionRecord;
        let mut options = self
            .openai_client
            .request_options
            .clone()
            .compaction("native");
        let mut record =
            CompactionRecord::start(&mut options, "auto", &serde_json::to_string(&input)?);
        self.remove_compaction_indexes(&mut input);
        let compacted = crate::services::compaction::native::generate_messages_with_options(
            &self.provider,
            options,
            input
                .into_iter()
                .map(serde_json::from_value)
                .collect::<serde_json::Result<_>>()?,
            self.system_prompt.as_deref().unwrap_or(""),
            self.conv_id.as_deref(),
            self.search_config_path
                .as_deref()
                .and_then(crate::data_layout::data_root_of),
        )
        .await?;
        if let Some(record) = &mut record {
            record.finish(&compacted, "in-flight-native-window");
        }
        let mut native = compacted.native;
        if let Some(window) = native.as_mut() {
            prepare_native_window(
                window,
                self.search_config_path
                    .as_deref()
                    .and_then(crate::data_layout::data_root_of),
                self.conv_id.as_deref().unwrap_or_default(),
            );
        }
        Ok(vec![serde_json::from_value(json!({
            "role":"user",
            "content":compacted.text,
            "provider_content":native,
        }))?])
    }

    // A live window can refresh its durable prefix. A summary already owns
    // a snapshot, possibly cut before a queued input or at a partial boundary.
    // Returns `true` when durable session history or microcompact refreshed the
    // window so the caller can synchronize its in-memory `conversation_messages`.
    pub(super) async fn replay_window<M: serde::Serialize + serde::de::DeserializeOwned>(
        &mut self,
        window: &mut Vec<M>,
        durable_len: Option<usize>,
        mid_turn_managed: bool,
        render: fn(&Self, &[(Vec<String>, Vec<String>)]) -> Result<Vec<M>>,
    ) -> Result<bool> {
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
        } else if estimated <= (limit * 70 / 100).min(capacity) && (native || estimated <= threshold)
        {
            return Ok(false);
        }
        let mut physical_cut = None;
        let mut microcompact_changed = false;
        let mut persisted_len = window.len();
        let mut durable_window = None;
        if let (Some(durable_len), Some(ExecutorHistory::Session(shard))) =
            (durable_len, self.executor_history.clone())
        {
            let original = values(window)?;
            let additions = window.split_off(durable_len);
            let (cut, changed) = self.reload_with_cut(&shard, true)?;
            physical_cut = cut;
            microcompact_changed = changed;
            *window = render(self, &self.message_additions())?;
            persisted_len = window.len();
            let durable = values(window)?;
            // An explicit request can carry a window that the durable shard does
            // not contain (for example a replayed conversation). Compact that
            // in-flight window instead of rewriting the stored history.
            let matches = original.len() >= durable.len() && original[..durable.len()] == durable[..];
            durable_window = (!matches).then_some(original);
            window.extend(additions);
            rows = if native {
                match &durable_window {
                    Some(provided) => provided.clone(),
                    None => values(window)?,
                }
            } else {
                values(window)?
            };
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

        if native {
            if let Some(provided) = durable_window {
                *window = self.compact_in_flight(provided).await?;
                return Ok(false);
            }
            if durable_len.is_none() {
                return Ok(false);
            }
            let Some(ExecutorHistory::Session(shard)) = self.executor_history.clone() else {
                anyhow::bail!("Native Responses compression requires durable session history");
            };
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
                self.compact_native_window(&shard, &cut, rows).await?;
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
                    .rposition(|m| m["role"] == "user")
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
        let prefix_index = (!prefix_index.is_empty())
            .then(|| json!({"role":"user","content":prefix_index.join("\n\n")}));
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
                        || row["_is_summary"] == true)
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
