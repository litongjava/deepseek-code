//! Bound outgoing call IDs without changing persisted history or tool arguments.
use base64::Engine;
use serde_json::Value;

fn normalize(id: &mut Value) {
    if let Some(raw) = id.as_str().filter(|raw| raw.len() > 64) {
        let digest = ring::digest::digest(&ring::digest::SHA256, raw.as_bytes());
        *id = Value::String(format!(
            "call_{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref())
        ));
    }
}

pub(super) fn normalize_responses(items: &mut [Value]) {
    for item in items {
        if matches!(
            item["type"].as_str(),
            Some("function_call" | "function_call_output")
        ) {
            if let Some(id) = item.get_mut("call_id") {
                normalize(id);
            }
        }
    }
}

pub(super) fn normalize_chat(body: &mut Value) {
    if let Some(messages) = body["messages"].as_array_mut() {
        for message in messages {
            if let Some(id) = message.get_mut("tool_call_id") {
                normalize(id);
            }
            if let Some(calls) = message["tool_calls"].as_array_mut() {
                for call in calls {
                    if let Some(id) = call.get_mut("id") {
                        normalize(id);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn long_call_ids_keep_pairs_distinct_and_short_ids_unchanged() {
        let first = "a".repeat(69);
        let second = format!("{}b", "a".repeat(68));
        let mut items = vec![
            json!({"type":"function_call","call_id":first,"arguments":first}),
            json!({"type":"function_call_output","call_id":first,"output":first}),
            json!({"type":"function_call","call_id":second}),
            json!({"type":"function_call","call_id":"a".repeat(64)}),
            json!({"type":"reasoning","encrypted_content":first}),
        ];
        normalize_responses(&mut items);
        assert_eq!(items[0]["call_id"], items[1]["call_id"]);
        assert_ne!(items[0]["call_id"], items[2]["call_id"]);
        assert!(items[0]["call_id"].as_str().unwrap().len() <= 64);
        assert_eq!(items[3]["call_id"], "a".repeat(64));
        assert_eq!(items[0]["arguments"], first);
        assert_eq!(items[1]["output"], first);
        assert_eq!(items[4]["encrypted_content"], first);
        let mut chat = json!({"messages":[
            {"role":"assistant","tool_calls":[{"id":first}]},
            {"role":"tool","tool_call_id":first}
        ]});
        normalize_chat(&mut chat);
        assert_eq!(
            chat["messages"][0]["tool_calls"][0]["id"],
            items[0]["call_id"]
        );
        assert_eq!(chat["messages"][1]["tool_call_id"], items[0]["call_id"]);
        let original = chat.clone();
        normalize_chat(&mut chat);
        assert_eq!(chat, original);
    }
}
