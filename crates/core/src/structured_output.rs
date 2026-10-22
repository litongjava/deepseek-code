//! Provider-enforced structured output with independent local schema validation.
//!
//! Some compatible endpoints do not enforce a response schema: DeepSeek ignores
//! Anthropic `output_config.format` and rejects OpenAI `json_schema` response
//! formats. For those the schema is stated in the system prompt instead, and the
//! reply is still validated locally before it is accepted.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

/// Whether the endpoint enforces `output_config.format` / `response_format`.
pub fn provider_enforces_schema(base_url: &str) -> bool {
    !base_url.contains("deepseek.com")
}

fn prompt_instruction(schema: &Value) -> String {
    format!(
        "Your final answer must be exactly one JSON value that conforms to the JSON schema below. \
Do not wrap it in Markdown code fences and do not add any text before or after it.\n\
<json_schema>\n{schema}\n</json_schema>"
    )
}

/// Appends the schema instruction to an Anthropic Messages `system` field.
pub fn append_anthropic_instruction(body: &mut Value, schema: &Value) {
    let text = prompt_instruction(schema);
    match &mut body["system"] {
        Value::String(system) if !system.is_empty() => {
            system.push_str("\n\n");
            system.push_str(&text);
        }
        Value::Array(blocks) => blocks.push(json!({"type":"text","text":text})),
        other => *other = Value::String(text),
    }
}

/// Appends the schema instruction to the system turn of an OpenAI chat body.
pub fn append_openai_instruction(body: &mut Value, schema: &Value) {
    let text = prompt_instruction(schema);
    let Some(messages) = body["messages"].as_array_mut() else {
        return;
    };
    match messages.first_mut() {
        Some(first) if first["role"] == "system" && first["content"].is_string() => {
            let system = first["content"].as_str().unwrap_or_default();
            first["content"] = Value::String(format!("{system}\n\n{text}"));
        }
        _ => messages.insert(0, json!({"role":"system","content":text})),
    }
}

/// Removes surrounding whitespace and one enclosing Markdown code fence, which
/// prompt-guided models still add occasionally.
pub fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(inner) = trimmed
        .strip_prefix("```")
        .and_then(|rest| rest.strip_suffix("```"))
    else {
        return trimmed;
    };
    match inner.split_once('\n') {
        Some((language, body)) if language.trim().chars().all(|c| c.is_ascii_alphanumeric()) => {
            body.trim()
        }
        _ => inner.trim(),
    }
}

fn check_local_references(value: &Value) -> Result<()> {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef") {
                    ensure!(
                        value
                            .as_str()
                            .is_some_and(|reference| reference.starts_with('#')),
                        "Structured-output schemas support only local fragment references"
                    );
                }
                check_local_references(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                check_local_references(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn validate_schema(schema: &Value) -> Result<()> {
    ensure!(schema.is_object(), "JSON schema must be an object");
    ensure!(
        serde_json::to_vec(schema)?.len() <= 1_048_576,
        "JSON schema exceeds 1 MiB"
    );
    check_local_references(schema)?;
    jsonschema::validator_for(schema)
        .map_err(|error| anyhow::anyhow!("Invalid JSON schema: {error}"))?;
    Ok(())
}

pub fn validate_result(schema: &Value, text: &str) -> Result<Value> {
    validate_schema(schema)?;
    let value: Value =
        serde_json::from_str(strip_code_fence(text)).context("Structured output is not valid JSON")?;
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| anyhow::anyhow!("Invalid JSON schema: {error}"))?;
    let errors: Vec<_> = validator
        .iter_errors(&value)
        .take(3)
        .map(|error| error.to_string())
        .collect();
    ensure!(
        errors.is_empty(),
        "Structured output does not match JSON schema: {}",
        errors.join("; ")
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn validates_required_types_enum_and_additional_properties() {
        let schema = json!({"type":"object","properties":{"answer":{"type":"integer","minimum":1},"status":{"enum":["done"]}},"required":["answer","status"],"additionalProperties":false});
        assert!(validate_result(&schema, r#"{"answer":2,"status":"done"}"#).is_ok());
        for text in [
            r#"{"answer":"2","status":"done"}"#,
            r#"{"answer":0,"status":"done"}"#,
            r#"{"answer":2}"#,
            r#"{"answer":2,"status":"done","extra":1}"#,
            "not json",
        ] {
            assert!(validate_result(&schema, text).is_err());
        }
    }
    #[test]
    fn fenced_replies_are_accepted_and_instructions_reach_the_system_prompt() {
        let schema = json!({"type":"object","properties":{"n":{"type":"integer"}},"required":["n"]});
        for text in ["```json\n{\"n\":1}\n```", "```\n{\"n\":1}\n```", " {\"n\":1}\n"] {
            assert_eq!(strip_code_fence(text), r#"{"n":1}"#);
            assert!(validate_result(&schema, text).is_ok());
        }
        assert!(!provider_enforces_schema("https://api.deepseek.com/anthropic"));
        assert!(provider_enforces_schema("https://api.anthropic.com"));

        let mut anthropic = json!({"system":"base"});
        append_anthropic_instruction(&mut anthropic, &schema);
        let system = anthropic["system"].as_str().unwrap();
        assert!(system.starts_with("base\n\n") && system.contains(r#""required":["n"]"#));
        let mut blocks = json!({"system":[{"type":"text","text":"base"}]});
        append_anthropic_instruction(&mut blocks, &schema);
        assert_eq!(blocks["system"].as_array().unwrap().len(), 2);

        let mut openai = json!({"messages":[{"role":"system","content":"base"},{"role":"user","content":"hi"}]});
        append_openai_instruction(&mut openai, &schema);
        assert!(openai["messages"][0]["content"].as_str().unwrap().contains("<json_schema>"));
        let mut bare = json!({"messages":[{"role":"user","content":"hi"}]});
        append_openai_instruction(&mut bare, &schema);
        assert_eq!(bare["messages"][0]["role"], "system");
        assert_eq!(bare["messages"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn local_refs_work_and_remote_refs_or_invalid_schema_fail() {
        let schema = json!({"$defs":{"value":{"type":"integer"}},"type":"object","properties":{"value":{"$ref":"#/$defs/value"}},"required":["value"]});
        assert!(validate_result(&schema, r#"{"value":4}"#).is_ok());
        assert!(validate_result(&schema, r#"{"value":"four"}"#).is_err());
        assert!(validate_schema(&json!({"$ref":"https://example.invalid/schema"})).is_err());
        assert!(validate_schema(&json!({"type":42})).is_err());
    }
}
