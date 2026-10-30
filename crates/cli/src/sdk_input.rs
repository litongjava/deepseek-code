//! Incremental NDJSON input for non-interactive clients. This reader never waits
//! for stdin EOF before delivering a complete user message.
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::io::{BufRead, Read};
use tokio::sync::mpsc;

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct UserMessage {
    pub text: String,
    pub(crate) initial_hook: bool,
    pub session_id: Option<String>,
    pub uuid: Option<String>,
    pub envelope: Value,
    pub content: Value,
}

#[derive(Debug)]
pub enum Frame {
    User(UserMessage),
    Control(Value),
    Response(Value),
}

pub fn parse_frame(line: &str) -> Result<Frame> {
    let value: Value = serde_json::from_str(line.trim_start_matches('\u{feff}'))?;
    match value["type"].as_str() {
        Some("user") => parse_line(line).map(Frame::User),
        Some("control_request") => {
            ensure!(
                value["request_id"].as_str().is_some_and(|s| !s.is_empty()),
                "control_request needs request_id"
            );
            ensure!(
                value["request"]["subtype"].is_string(),
                "control_request needs request.subtype"
            );
            Ok(Frame::Control(value))
        }
        Some("control_response") => {
            ensure!(
                value["response"]["request_id"].is_string(),
                "control_response needs response.request_id"
            );
            Ok(Frame::Response(value))
        }
        _ => bail!("Unsupported SDK frame type"),
    }
}

fn optional_string(value: &Value, field: &str) -> Result<Option<String>> {
    value
        .get(field)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .with_context(|| format!("SDK field '{field}' must be a string"))
        })
        .transpose()
}

pub fn parse_line(line: &str) -> Result<UserMessage> {
    let frame: Value = serde_json::from_str(line.trim_start_matches('\u{feff}'))
        .context("Invalid stream-json input")?;
    ensure!(frame["type"]=="user","stream-json input currently accepts only SDK user messages (type: user); control messages are not supported");
    let message = frame
        .get("message")
        .context("SDK user frame needs message")?;
    ensure!(message["role"] == "user", "SDK message.role must be user");
    let content = deepseek_code_core::runtime::normalize_user_content(&message["content"])?;
    let text = match &content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => {
            let mut texts = Vec::new();
            for block in blocks {
                if block["type"] != "text" {
                    continue;
                }
                texts.push(
                    block["text"]
                        .as_str()
                        .context("SDK text block needs a string text field")?,
                );
            }
            texts.join("\n")
        }
        _ => bail!("SDK message.content must be a string or an array of text blocks"),
    };
    Ok(UserMessage {
        initial_hook: false,
        text,
        content,
        session_id: optional_string(&frame, "session_id")?,
        uuid: optional_string(&frame, "uuid")?,
        envelope: frame,
    })
}

pub fn stdin() -> mpsc::Receiver<Result<Frame>> {
    frames_from_reader(std::io::BufReader::new(std::io::stdin()))
}

#[cfg(test)]
pub fn from_reader(reader: impl BufRead + Send + 'static) -> mpsc::Receiver<Result<UserMessage>> {
    let (tx, rx) = mpsc::channel(1);
    let mut frames = frames_from_reader(reader);
    tokio::spawn(async move {
        while let Some(frame) = frames.recv().await {
            let value = frame.and_then(|f| match f {
                Frame::User(u) => Ok(u),
                _ => bail!("Expected user message"),
            });
            if tx.send(value).await.is_err() {
                break;
            }
        }
    });
    rx
}

pub fn frames_from_reader(
    mut reader: impl BufRead + Send + 'static,
) -> mpsc::Receiver<Result<Frame>> {
    // Small bounded queue applies backpressure while a model turn is running.
    let (send, receive) = mpsc::channel(1);
    std::thread::spawn(move || {
        let mut number = 0;
        loop {
            number += 1;
            let result = (|| -> Result<Option<Frame>> {
                let mut bytes = Vec::new();
                let read = reader
                    .by_ref()
                    .take((MAX_FRAME_BYTES + 1) as u64)
                    .read_until(b'\n', &mut bytes)?;
                if read == 0 {
                    return Ok(None);
                }
                ensure!(
                    bytes.len() <= MAX_FRAME_BYTES,
                    "SDK input frame exceeds 8 MiB"
                );
                let text = std::str::from_utf8(&bytes).context("SDK input must be UTF-8")?;
                if text.trim().is_empty() {
                    return Ok(Some(Frame::User(UserMessage {
                        initial_hook: false,
                        text: String::new(),
                        session_id: None,
                        uuid: None,
                        envelope: Value::Null,
                        content: Value::Null,
                    })));
                }
                parse_frame(text).map(Some)
            })()
            .with_context(|| format!("stream-json input line {number}"));
            match result {
                Ok(None) => break,
                Ok(Some(Frame::User(message))) if message.envelope.is_null() => continue,
                Ok(Some(message)) => {
                    if send.blocking_send(Ok(message)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = send.blocking_send(Err(error));
                    break;
                }
            }
        }
    });
    receive
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn supports_official_text_envelopes_without_dropping_unknown_content() {
        let message=parse_line(&json!({"type":"user","session_id":"s","uuid":"m","message":{"role":"user","content":[{"type":"text","text":"你好"},{"type":"text","text":"second"}]}}).to_string()).unwrap();
        assert_eq!(message.text, "你好\nsecond");
        assert_eq!(message.session_id.as_deref(), Some("s"));
        for content in [
            json!([{ "type":"image","source":{} }]),
            json!(null),
            json!(""),
        ] {
            assert!(parse_line(
                &json!({"type":"user","message":{"role":"user","content":content}}).to_string()
            )
            .is_err());
        }
        assert!(parse_line(r#"{"type":"control_request"}"#).is_err());
    }

    #[tokio::test]
    async fn delivers_each_frame_before_eof_and_reports_line_numbers() {
        // Real local streaming transport, kept open while the first frame is read.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut writer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (reader, _) = listener.accept().unwrap();
        let mut input = from_reader(std::io::BufReader::new(reader));
        writeln!(
            writer,
            "{}",
            json!({"type":"user","message":{"role":"user","content":"first"}})
        )
        .unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), input.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first.text, "first");
        writeln!(writer, "\nnot-json").unwrap();
        let error = input.recv().await.unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("line 3"));
        assert!(input.recv().await.is_none());
    }

    #[tokio::test]
    async fn accepts_final_frame_without_newline_and_rejects_non_utf8() {
        let mut input = from_reader(std::io::Cursor::new(
            br#"{"type":"user","message":{"role":"user","content":"last"}}"#.to_vec(),
        ));
        assert_eq!(input.recv().await.unwrap().unwrap().text, "last");
        assert!(input.recv().await.is_none());
        let mut input = from_reader(std::io::Cursor::new(vec![0xff, b'\n']));
        assert!(format!("{:#}", input.recv().await.unwrap().unwrap_err()).contains("UTF-8"));
    }
}
