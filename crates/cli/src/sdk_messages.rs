//! Reference-compatible SDK messages (`system`, `assistant`, `user`, `stream_event`)
//! derived from engine events. The legacy event lines are still emitted next to
//! them, so existing stream-json consumers keep working unchanged.
use crate::session::emit_json;
use anyhow::Result;
use claude_core::native_engine::tool_loop::EngineEvent;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

static INIT_EMITTED: AtomicBool = AtomicBool::new(false);

/// Reference permission mode names used by SDK clients.
pub fn reference_mode(mode: &str) -> &str {
    match mode {
        "ask" => "default",
        "accept-edits" => "acceptEdits",
        "bypass" => "bypassPermissions",
        "dont-ask" => "dontAsk",
        other => other,
    }
}

/// Emits `system/init` once per process, before the first turn's messages.
pub fn emit_init_once(init: impl FnOnce() -> Value) -> Result<()> {
    if INIT_EMITTED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    emit_json(&init())
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Block {
    Text,
    Thinking,
}

pub struct Translator {
    session_id: String,
    partial: bool,
    message_id: String,
    model: String,
    block: Option<(Block, String)>,
    index: usize,
    open_message: bool,
    pub turns: u32,
}

impl Translator {
    pub fn new(session_id: &str, partial: bool) -> Self {
        Self {
            session_id: session_id.to_owned(),
            partial,
            message_id: String::new(),
            model: String::new(),
            block: None,
            index: 0,
            open_message: false,
            turns: 0,
        }
    }

    fn envelope(&self, kind: &str, message: Value) -> Value {
        json!({"type":kind,"message":message,"parent_tool_use_id":null,
            "session_id":self.session_id,"uuid":uuid::Uuid::new_v4().to_string()})
    }

    fn stream(&self, event: Value) -> Result<()> {
        if !self.partial {
            return Ok(());
        }
        emit_json(&json!({"type":"stream_event","event":event,"parent_tool_use_id":null,
            "session_id":self.session_id,"uuid":uuid::Uuid::new_v4().to_string()}))
    }

    fn assistant(&self, content: Value) -> Result<()> {
        emit_json(&self.envelope(
            "assistant",
            json!({"id":self.message_id,"type":"message","role":"assistant","model":self.model,
                "content":[content],"stop_reason":null,"stop_sequence":null}),
        ))
    }

    fn start_message(&mut self, model: &str) -> Result<()> {
        // Providers may announce a round twice; an empty open message is reused.
        if self.open_message && self.index == 0 && self.block.is_none() {
            self.model = model.to_owned();
            return Ok(());
        }
        self.close_message(None)?;
        self.message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        self.model = model.to_owned();
        self.index = 0;
        self.open_message = true;
        self.turns += 1;
        self.stream(json!({"type":"message_start","message":{"id":self.message_id,"type":"message",
            "role":"assistant","model":self.model,"content":[],"stop_reason":null,"stop_sequence":null}}))
    }

    fn ensure_message(&mut self) -> Result<()> {
        if !self.open_message {
            let model = self.model.clone();
            self.start_message(&model)?;
        }
        Ok(())
    }

    fn close_block(&mut self) -> Result<()> {
        let Some((kind, text)) = self.block.take() else {
            return Ok(());
        };
        self.stream(json!({"type":"content_block_stop","index":self.index}))?;
        self.index += 1;
        self.assistant(match kind {
            Block::Text => json!({"type":"text","text":text}),
            Block::Thinking => json!({"type":"thinking","thinking":text,"signature":""}),
        })
    }

    fn close_message(&mut self, stop_reason: Option<&str>) -> Result<()> {
        self.close_block()?;
        if !self.open_message {
            return Ok(());
        }
        self.open_message = false;
        self.stream(json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":null},"usage":{}}))?;
        self.stream(json!({"type":"message_stop"}))
    }

    fn delta(&mut self, kind: Block, delta: &str) -> Result<()> {
        self.ensure_message()?;
        if self.block.as_ref().is_some_and(|(open, _)| *open != kind) {
            self.close_block()?;
        }
        if self.block.is_none() {
            self.block = Some((kind, String::new()));
            self.stream(json!({"type":"content_block_start","index":self.index,"content_block":match kind {
                Block::Text => json!({"type":"text","text":""}),
                Block::Thinking => json!({"type":"thinking","thinking":"","signature":""}),
            }}))?;
        }
        self.block.as_mut().unwrap().1.push_str(delta);
        self.stream(json!({"type":"content_block_delta","index":self.index,"delta":match kind {
            Block::Text => json!({"type":"text_delta","text":delta}),
            Block::Thinking => json!({"type":"thinking_delta","thinking":delta}),
        }}))
    }

    pub fn observe(&mut self, event: &EngineEvent) -> Result<()> {
        match event {
            EngineEvent::MessageStart { model } => self.start_message(model),
            EngineEvent::Text(delta) if !delta.is_empty() => self.delta(Block::Text, delta),
            EngineEvent::Thinking(delta) if !delta.is_empty() => self.delta(Block::Thinking, delta),
            EngineEvent::ToolUseStart {
                tool_use_id,
                tool_name,
                tool_input,
                ..
            } => {
                self.ensure_message()?;
                self.close_block()?;
                self.stream(json!({"type":"content_block_start","index":self.index,
                    "content_block":{"type":"tool_use","id":tool_use_id,"name":tool_name,"input":{}}}))?;
                self.stream(json!({"type":"content_block_delta","index":self.index,
                    "delta":{"type":"input_json_delta","partial_json":tool_input.to_string()}}))?;
                self.stream(json!({"type":"content_block_stop","index":self.index}))?;
                self.index += 1;
                self.assistant(json!({"type":"tool_use","id":tool_use_id,"name":tool_name,"input":tool_input}))
            }
            EngineEvent::ToolUseDone {
                tool_use_id,
                output,
                is_error,
                ..
            }
            | EngineEvent::PipelineToolResult {
                tool_use_id,
                output,
                is_error,
                ..
            } => {
                // A tool result ends the model round that requested it.
                self.close_message(Some("tool_use"))?;
                emit_json(&self.envelope(
                    "user",
                    json!({"role":"user","content":[{"type":"tool_result","tool_use_id":tool_use_id,
                        "content":tool_result_content(output),"is_error":is_error}]}),
                ))
            }
            EngineEvent::MessageStop { stop_reason, .. } => {
                self.close_message(stop_reason.as_deref().or(Some("end_turn")))
            }
            _ => Ok(()),
        }
    }

    /// Flushes buffered text when a turn ends without a stop event.
    pub fn finish(&mut self) -> Result<()> {
        self.close_message(None)
    }
}

/// Reference result subtype for a finished turn.
pub fn result_subtype(error: Option<&str>) -> &'static str {
    match error {
        None => "success",
        Some(error) if error.contains("max_iterations") || error.contains("aximum turns") => {
            "error_max_turns"
        }
        Some(_) => "error_during_execution",
    }
}

/// A failed turn's result when the turn ended before producing its own result.
pub fn error_result(session_id: &str, error: &str) -> Value {
    json!({"type":"result","subtype":"error_during_execution","is_error":true,"session_id":session_id,
        "error":error,"result":"","num_turns":0,"duration_ms":0,"permission_denials":[],
        "uuid":uuid::Uuid::new_v4().to_string()})
}

/// Tool output as reference content: MCP results arrive as a JSON array of
/// content blocks and are passed through as blocks, anything else as text.
fn tool_result_content(output: &str) -> Value {
    if let Ok(Value::Array(blocks)) = serde_json::from_str::<Value>(output) {
        if !blocks.is_empty() && blocks.iter().all(|block| block["type"].is_string()) {
            return Value::Array(blocks);
        }
    }
    json!(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_keeps_content_blocks_and_plain_text() {
        assert_eq!(tool_result_content(r#"[{"type":"text","text":"5"}]"#), json!([{"type":"text","text":"5"}]));
        assert_eq!(tool_result_content("[1, 2]"), json!("[1, 2]"));
        assert_eq!(tool_result_content("[]"), json!("[]"));
        assert_eq!(tool_result_content("plain"), json!("plain"));
    }
}
