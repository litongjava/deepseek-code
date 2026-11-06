use super::{ChildState, EngineEvent};
use crate::db::message_repo::{self, ToolCallRow};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Full history is durable on every event; the activity preview is independent.
pub(super) struct ChildTranscript {
    pub state: ChildState,
    pub receiver: tokio::sync::mpsc::Receiver<EngineEvent>,
    pub web_evidence: Vec<Value>,
    assistant_id: String,
    text: String,
    thinking: String,
    saw_text: bool,
    tools: HashMap<String, ToolCallRow>,
    arguments: HashMap<String, String>,
    evidence_ids: HashSet<String>,
    collect_web_evidence: bool,
    last_preview: Instant,
}

impl ChildTranscript {
    pub fn new(
        state: ChildState,
        receiver: tokio::sync::mpsc::Receiver<EngineEvent>,
        collect_web_evidence: bool,
    ) -> Result<Self> {
        let transcript = Self {
            state,
            receiver,
            collect_web_evidence,
            web_evidence: Vec::new(),
            assistant_id: uuid::Uuid::new_v4().to_string(),
            text: String::new(),
            thinking: String::new(),
            saw_text: false,
            tools: HashMap::new(),
            arguments: HashMap::new(),
            evidence_ids: HashSet::new(),
            last_preview: Instant::now(),
        };
        transcript.persist()?;
        Ok(transcript)
    }

    fn persist(&self) -> Result<()> {
        self.state.context.db.with_conn(|conn| -> Result<()> {
            conn.execute("INSERT INTO messages(id,conversation_id,role,content,thinking,created_at,sort_order) VALUES(?1,?2,'assistant',?3,?4,?5,(SELECT COALESCE(MAX(sort_order),-1)+1 FROM messages WHERE conversation_id=?2)) ON CONFLICT(id) DO UPDATE SET content=excluded.content,thinking=excluded.thinking",
                rusqlite::params![self.assistant_id, self.state.value["session_id"].as_str().unwrap_or_default(), self.text, self.thinking, chrono::Utc::now().to_rfc3339()])?;
            Ok(())
        })??;
        Ok(())
    }

    fn save_tool(&self, row: &ToolCallRow) -> Result<()> {
        self.state.context.db.with_conn(|conn| {
            message_repo::save_stream_tool_result(
                conn,
                self.state.value["session_id"].as_str().unwrap_or_default(),
                &self.assistant_id,
                &self.text,
                row,
            )
        })??;
        Ok(())
    }

    fn tool(&self, id: &str, name: &str) -> ToolCallRow {
        self.tools.get(id).cloned().unwrap_or_else(|| ToolCallRow {
            id: id.to_owned(),
            message_id: self.assistant_id.clone(),
            name: name.to_owned(),
            input: None,
            output: None,
            is_error: false,
            sort_order: self.tools.len() as i64,
        })
    }

    pub fn finish_text(&mut self, text: &str) -> Result<()> {
        // execute() returns an aggregate already represented by streamed segments.
        if !self.saw_text && !text.is_empty() {
            self.text.push_str(text);
            self.saw_text = true;
            self.persist()?;
        }
        Ok(())
    }

    pub async fn process(&mut self, event: EngineEvent) -> Result<()> {
        if let Some(event) = self.reduce(event)? {
            let _ = self.state.event_tx.send(event).await;
        }
        Ok(())
    }

    pub async fn drain(&mut self) -> Result<()> {
        while let Ok(event) = self.receiver.try_recv() {
            self.process(event).await?;
        }
        Ok(())
    }

    fn reduce(&mut self, event: EngineEvent) -> Result<Option<EngineEvent>> {
        let mut progress = None;
        match event {
            EngineEvent::ContextReset { .. } => {
                self.assistant_id = uuid::Uuid::new_v4().to_string();
                self.text.clear();
                self.thinking.clear();
                self.tools.clear();
                self.arguments.clear();
                self.persist()?;
            }
            event @ (EngineEvent::AskUser { .. }
            | EngineEvent::ToolPermission { .. }
            | EngineEvent::AgentProgress { .. }) => return Ok(Some(event)),
            EngineEvent::Usage(usage) => {
                self.state.usage(&usage);
                self.state
                    .context
                    .save_setting(&self.state.key, &self.state.value)?;
            }
            EngineEvent::RequestSnapshot(snapshot) => {
                self.state.snapshot(&snapshot, &self.assistant_id)?
            }
            event @ (EngineEvent::Text(_) | EngineEvent::Thinking(_)) => {
                let text = match event {
                    EngineEvent::Text(text) => {
                        self.text.push_str(&text);
                        self.saw_text = true;
                        text
                    }
                    EngineEvent::Thinking(text) => {
                        self.thinking.push_str(&text);
                        text
                    }
                    _ => unreachable!(),
                };
                self.persist()?;
                self.state.streamed(&text);
                let mut preview = self.state.value["preview"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                preview.push_str(&text);
                self.state.value["preview"] = json!(preview
                    .chars()
                    .rev()
                    .take(8000)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>());
            }
            EngineEvent::ToolArgDelta { tool_use_id, delta } => {
                self.state.streamed(&delta);
                let id = format!("{}:{tool_use_id}", self.assistant_id);
                let mut row = self.tool(&id, "");
                let input = self.arguments.entry(id.clone()).or_default();
                input.push_str(&delta);
                row.input = Some(input.clone());
                self.save_tool(&row)?;
                self.tools.insert(id, row);
            }
            EngineEvent::ToolUseStart {
                tool_use_id,
                tool_name,
                tool_input,
                ..
            } => {
                let id = format!("{}:{tool_use_id}", self.assistant_id);
                let mut row = self.tool(&id, &tool_name);
                row.name = tool_name.clone();
                if row.output.is_none() {
                    row.input = Some(tool_input.to_string());
                }
                self.save_tool(&row)?;
                self.tools.insert(id, row);
                self.state.value["current_tool"] = json!(&tool_name);
                progress = Some(self.state.progress("running", Some(tool_name)));
            }
            EngineEvent::ToolUseDone {
                tool_use_id,
                tool_name,
                tool_input,
                output,
                is_error,
            }
            | EngineEvent::PipelineToolResult {
                tool_use_id,
                tool_name,
                tool_input,
                output,
                is_error,
                ..
            } => {
                let id = format!("{}:{tool_use_id}", self.assistant_id);
                if self.collect_web_evidence
                    && matches!(tool_name.as_str(), "WebSearch" | "WebFetch")
                    && self.evidence_ids.insert(id.clone())
                {
                    self.web_evidence.push(json!({"tool_use_id":tool_use_id,"name":tool_name,"input":tool_input,"output":serde_json::from_str::<Value>(&output).unwrap_or_else(|_|Value::String(output.clone())),"is_error":is_error}));
                }
                let mut row = self.tool(&id, &tool_name);
                let input = Some(tool_input.to_string());
                if row.output.as_deref() == Some(output.as_str())
                    && row.name == tool_name
                    && row.input == input
                    && row.is_error == is_error
                {
                    return Ok(None);
                }
                row.name = tool_name.clone();
                row.input = input;
                row.output = Some(output);
                row.is_error = is_error;
                self.save_tool(&row)?;
                self.tools.insert(id, row);
                self.state.value["current_tool"] = Value::Null;
                progress = Some(self.state.progress(
                    if is_error {
                        "tool_failed"
                    } else {
                        "tool_completed"
                    },
                    Some(tool_name),
                ));
            }
            _ => {}
        }
        if progress.is_some() || self.last_preview.elapsed().as_millis() >= 250 {
            self.state
                .context
                .save_setting(&self.state.key, &self.state.value)?;
            self.last_preview = Instant::now();
        }
        Ok(progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (
        tempfile::TempDir,
        ChildTranscript,
        tokio::sync::mpsc::Sender<EngineEvent>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let context = crate::services::session_messages::context(temp.path()).unwrap();
        let id = crate::runtime::SessionStore::new(context.db.clone())
            .create(temp.path(), "fixture", "fixture")
            .unwrap();
        let (event_tx, _) = tokio::sync::mpsc::channel(64);
        let state = ChildState {
            context,
            key: format!("delegation.{id}"),
            value: json!({"session_id":id,"status":"running"}),
            event_tx,
            request_id: String::new(),
            request_base: 0,
            streamed_bytes: 0,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        (temp, ChildTranscript::new(state, rx, false).unwrap(), tx)
    }

    #[tokio::test]
    async fn final_drain_preserves_full_text_thinking_and_deduplicates_results() {
        let (_temp, mut transcript, tx) = fixture();
        let text = "界".repeat(9001);
        tx.try_send(EngineEvent::Text(text.clone())).unwrap();
        tx.try_send(EngineEvent::Thinking("private reasoning".into()))
            .unwrap();
        tx.try_send(EngineEvent::ToolUseStart {
            tool_use_id: "call".into(),
            tool_name: "Read".into(),
            tool_input: json!({}),
            text_before: String::new(),
            display_title: None,
        })
        .unwrap();
        tx.try_send(EngineEvent::ToolArgDelta {
            tool_use_id: "call".into(),
            delta: "{\"path\":\"file\"}".into(),
        })
        .unwrap();
        tx.try_send(EngineEvent::ToolUseDone {
            tool_use_id: "call".into(),
            tool_name: "Read".into(),
            tool_input: json!({"path":"file"}),
            output: "result".into(),
            is_error: false,
        })
        .unwrap();
        tx.try_send(EngineEvent::PipelineToolResult {
            tool_use_id: "call".into(),
            tool_name: "Read".into(),
            tool_input: json!({"path":"file"}),
            output: "result".into(),
            is_error: false,
            completed_count: 1,
            total_count: 1,
        })
        .unwrap();
        transcript.drain().await.unwrap();
        transcript.finish_text(&text).unwrap();
        assert_eq!(
            transcript.state.value["preview"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            8000
        );
        transcript
            .state
            .context
            .db
            .with_conn(|conn| {
                let messages = message_repo::get_messages_by_conversation(
                    conn,
                    transcript.state.value["session_id"].as_str().unwrap(),
                )
                .unwrap();
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].content, text);
                assert_eq!(messages[0].thinking.as_deref(), Some("private reasoning"));
                let tools =
                    message_repo::get_tool_calls_for_message(conn, &messages[0].id).unwrap();
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0].id, format!("{}:call", messages[0].id));
                assert_eq!(tools[0].output.as_deref(), Some("result"));
            })
            .unwrap();
    }

    #[test]
    fn cancellation_drains_reset_segments_and_partial_tool_input() {
        let (_temp, mut transcript, tx) = fixture();
        let context = transcript.state.context.clone();
        let session = transcript.state.value["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        transcript
            .reduce(EngineEvent::Text("before".into()))
            .unwrap();
        tx.try_send(EngineEvent::ContextReset {
            session_id: session.clone(),
            boundary_id: "boundary".into(),
        })
        .unwrap();
        tx.try_send(EngineEvent::Thinking("after thinking".into()))
            .unwrap();
        tx.try_send(EngineEvent::Text("after".into())).unwrap();
        tx.try_send(EngineEvent::ToolUseStart {
            tool_use_id: "call".into(),
            tool_name: "Read".into(),
            tool_input: json!({}),
            text_before: String::new(),
            display_title: None,
        })
        .unwrap();
        tx.try_send(EngineEvent::ToolArgDelta {
            tool_use_id: "call".into(),
            delta: "{\"path\":".into(),
        })
        .unwrap();
        drop(transcript);
        context
            .db
            .with_conn(|conn| {
                let messages = message_repo::get_messages_by_conversation(conn, &session).unwrap();
                assert_eq!(messages.len(), 2);
                assert_eq!(messages[0].content, "before");
                assert_eq!(messages[1].content, "after");
                assert_eq!(messages[1].thinking.as_deref(), Some("after thinking"));
                let tools =
                    message_repo::get_tool_calls_for_message(conn, &messages[1].id).unwrap();
                assert_eq!(tools[0].input.as_deref(), Some("{\"path\":"));
                assert!(tools[0].output.is_none());
            })
            .unwrap();
    }
}

impl Drop for ChildTranscript {
    fn drop(&mut self) {
        // Cancellation can drop the execution future with events still queued.
        self.receiver.close();
        while let Ok(event) = self.receiver.try_recv() {
            match self.reduce(event) {
                Ok(Some(event)) => {
                    let _ = self.state.event_tx.try_send(event);
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    "Could not persist child transcript during cancellation: {error:#}"
                ),
            }
        }
    }
}
