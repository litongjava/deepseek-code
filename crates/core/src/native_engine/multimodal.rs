//! Image conversion at the model boundary, independent of persisted copies.
use anyhow::Result;
use serde_json::{json, Value};

use super::super::anthropic_client::AnthropicContent;
use super::super::openai_client::OpenAIContent;

pub(super) fn openai_content(content: &Value) -> Result<OpenAIContent> {
    let messages = normalize_messages(&[json!({"content": content})])?;
    Ok(serde_json::from_value(messages[0]["content"].clone())?)
}

pub(super) fn anthropic_content(content: &Value) -> Result<AnthropicContent> {
    let mut messages = normalize_messages(&[json!({"content": content})])?;
    if let Some(blocks) = messages[0]["content"].as_array_mut() {
        for block in blocks {
            if block["type"] == "image_url" {
                let url = block["image_url"]["url"]
                    .as_str()
                    .expect("normalized image URL");
                let (media_type, data) = url
                    .strip_prefix("data:")
                    .and_then(|url| url.split_once(";base64,"))
                    .expect("normalized embedded image");
                *block = json!({"type":"image","source":{"type":"base64","media_type":media_type,"data":data}});
            }
        }
    }
    Ok(serde_json::from_value(messages[0]["content"].clone())?)
}

pub(super) fn normalize_messages(messages: &[Value]) -> Result<Vec<Value>> {
    messages
        .iter()
        .map(|message| {
            let mut message = message.clone();
            if let Some(blocks) = message["content"].as_array_mut() {
                for block in blocks {
                    if block["type"] == "attachment" {
                        *block =
                            json!({"type":"text","text":crate::runtime::attachment_text(block)?});
                    }
                    if matches!(block["type"].as_str(), Some("image" | "image_url")) {
                        let normalized = crate::runtime::normalize_user_content(&json!([block]))?;
                        *block = normalized[0].clone();
                        // Display metadata belongs to saved messages, not provider protocols.
                        if let Some(object) = block.as_object_mut() {
                            object.remove("fileName");
                            object.remove("size");
                        }
                    }
                }
            }
            Ok(message)
        })
        .collect()
}
