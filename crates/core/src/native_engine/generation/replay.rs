//! Recovery keeps normalized visual content beside the text span of its source row.
use super::{output_budget, summarize_history, RecoveryContext, Summarizer};
use crate::native_engine::{
    anthropic_client::{AnthropicClient, AnthropicMessage},
    followup::WireRequest,
    openai_client::OpenAIClient,
    provider_manager::{ApiFormat, ResolvedProvider},
    request_context::ContextUsage,
    token_counter::TokenCounter,
    tool_loop::multimodal,
};
use crate::services::compaction::checkpoint::Checkpoint;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::ops::Range;
use std::sync::Arc;

pub(super) struct MediaRow {
    span: Range<usize>,
    wire: WireRequest,
}

pub(super) struct Replay<'a> {
    pub wire: WireRequest,
    pub focus: &'a str,
    pub has_main_turn: bool,
    pub source_tokens: u64,
    pub media: Vec<MediaRow>,
}

pub(in crate::native_engine) async fn summarize_replay(
    provider: ResolvedProvider,
    system: &str,
    rows: Vec<Value>,
    focus: &str,
    has_main_turn: bool,
    wire: WireRequest,
    window: Option<WireRequest>,
    checkpoint: &mut Checkpoint<'_>,
) -> Result<String> {
    // A cold import or oversized history must still be recoverable from before
    // local compression. Count the typed rows with the normal turn estimator.
    let source_tokens = TokenCounter::estimate(&rows) as u64;
    let focus = focus.trim();
    let mut text = if focus.is_empty() {
        "Conversation JSON:\n[".to_owned()
    } else {
        format!("Summary focus requested by the user:\n{focus}\n\nConversation JSON:\n[")
    };
    let mut media = Vec::new();
    let count = rows.len();
    for (index, mut row) in rows.into_iter().enumerate() {
        let start = if index == 0 { 0 } else { text.len() };
        if index > 0 {
            text.push(',');
        }
        let mut images = Vec::new();
        for part in row["content"].as_array_mut().into_iter().flatten() {
            if part["type"] == "image_url" {
                let reference = json!({"attached_image": images.len() + 1});
                images.push(std::mem::replace(part, reference));
            }
        }
        text.push_str(&serde_json::to_string(&row)?);
        if !images.is_empty() {
            media.push(MediaRow {
                span: start..text.len() + usize::from(index + 1 == count),
                wire: image_request(&provider, system, checkpoint.session, images)?,
            });
        }
    }
    text.push(']');
    let replay = Replay {
        wire,
        focus,
        has_main_turn,
        source_tokens,
        media,
    };
    summarize_history(
        provider,
        system,
        &text,
        Some(checkpoint),
        Some(replay),
        window,
    )
    .await
}

/// Feed the normalized content to the same request serializers as a normal
/// user message. A recovery node reuses this body for every split of the row.
fn image_request(
    provider: &ResolvedProvider,
    system: &str,
    session: &str,
    images: Vec<Value>,
) -> Result<WireRequest> {
    let output = output_budget(provider).0;
    let count = images.len() as u64;
    let content = Value::Array(images);
    let wire = if provider.provider.api_format == ApiFormat::Anthropic {
        let message = AnthropicMessage {
            role: "user".into(),
            content: multimodal::anthropic_content(&content)?,
        };
        let body = AnthropicClient::new().request_body(
            provider,
            vec![message],
            Some(system),
            vec![],
            output,
            false,
        )?;
        WireRequest {
            api: "anthropic-messages".into(),
            body,
            builtin_vision: None,
        }
    } else {
        let message = serde_json::from_value(json!({"role":"user", "content":content}))?;
        OpenAIClient::new().request_body(
            provider,
            vec![message],
            Some(system),
            vec![],
            output,
            Some(session),
        )?
    };
    let usage = ContextUsage::from_body(&wire.api, &wire.body, None, output);
    ensure!(
        usage.history.unknown_parts == count,
        "Cannot represent all images in the selected protocol; original history retained"
    );
    Ok(wire)
}

fn user_parts(wire: &WireRequest) -> impl Iterator<Item = &Value> {
    wire.body[wire.messages_key()]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["role"] == "user")
        .flat_map(|row| {
            row.get("parts")
                .unwrap_or(&row["content"])
                .as_array()
                .into_iter()
                .flatten()
        })
}

impl Replay<'_> {
    /// Local compression can remove an earlier user turn. Compare the emitted
    /// protocol parts in order, allowing cache annotations at different positions.
    pub fn retains_images(&self) -> bool {
        let mut actual = user_parts(&self.wire);
        self.media
            .iter()
            .flat_map(|row| user_parts(&row.wire))
            .all(|image| {
                actual.any(|part| {
                    image.as_object().is_some_and(|fields| {
                        fields.iter().all(|(key, value)| {
                            key == "cache_control" || part.get(key) == Some(value)
                        })
                    })
                })
            })
    }
}

/// Each split of a visual row carries that row's images. Subsequent merges
/// consume the validated fragment summaries in chronological order.
pub(super) fn recovery_sections<'a>(
    input: &'a str,
    media: Vec<MediaRow>,
    summarizer: &Summarizer<'_, '_>,
) -> Result<Vec<(&'a str, Option<Arc<RecoveryContext>>)>> {
    let mut sections = Vec::new();
    let mut offset = 0;
    for row in media {
        if row.span.start > offset {
            sections.push((&input[offset..row.span.start], summarizer.fallback.clone()));
        }
        offset = row.span.end;
        let mut wire = row.wire;
        let output = summarizer.last_budget;
        let mut tokens = ContextUsage::from_body(&wire.api, &wire.body, None, output)
            .history
            .estimated_tokens;
        if let Some(window) = &summarizer.fallback {
            let prefix = window.wire.body["input"]
                .as_array()
                .context("Missing Responses window")?;
            wire.body["input"]
                .as_array_mut()
                .context("Missing Responses input")?
                .splice(..0, prefix.iter().cloned());
            tokens += window.tokens;
        }
        let fallback = RecoveryContext {
            tokens,
            checkpoint_system: Checkpoint::request_key(
                summarizer.system,
                &wire.body.to_string(),
                output,
            ),
            wire,
        };
        sections.push((&input[row.span], Some(Arc::new(fallback))));
    }
    if offset < input.len() {
        sections.push((&input[offset..], summarizer.fallback.clone()));
    }
    Ok(sections)
}
