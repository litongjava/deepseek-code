//! Human-readable MCP bodies shared by live output and restored transcripts.
use serde_json::Value;

pub fn body(value: &Value) -> Option<String> {
    let blocks = value
        .as_array()
        .or_else(|| value.get("content")?.as_array())?;
    if !blocks
        .iter()
        .all(|block| block.get("type").and_then(Value::as_str).is_some())
    {
        return None;
    }
    Some(
        blocks
            .iter()
            .map(|block| match block["type"].as_str().unwrap_or_default() {
                "text" => block["text"].as_str().unwrap_or_default().to_owned(),
                "resource" => block["resource"]["text"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!(
                            "[Resource: {}]",
                            block["resource"]["uri"].as_str().unwrap_or("embedded")
                        )
                    }),
                "resource_link" => format!(
                    "{}: {}",
                    block["name"].as_str().unwrap_or("Resource"),
                    block["uri"].as_str().unwrap_or_default()
                ),
                "image" | "audio" => format!(
                    "[{}: {}]",
                    block["type"].as_str().unwrap(),
                    block["mimeType"]
                        .as_str()
                        .or_else(|| block["source"]["media_type"].as_str())
                        .unwrap_or("attached")
                ),
                _ => serde_json::to_string_pretty(block).unwrap_or_default(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unwraps_text_resources_and_attachments_without_serializing_binary_data() {
        let blocks = json!([
            {"type":"text","text":"# Result\n正文"},
            {"type":"resource","resource":{"uri":"file:///a","text":"source"}},
            {"type":"image","mimeType":"image/png","data":"PRIVATE_BINARY"},
            {"type":"resource_link","name":"Report","uri":"https://example.test/report"}
        ]);
        let expected =
            "# Result\n正文\nsource\n[image: image/png]\nReport: https://example.test/report";
        assert_eq!(body(&blocks).as_deref(), Some(expected));
        assert_eq!(
            body(&json!({"content":blocks,"isError":true})).as_deref(),
            Some(expected)
        );
        assert!(body(&json!({"answer":42})).is_none());
        assert!(body(&json!([1, 2])).is_none());
    }
}
