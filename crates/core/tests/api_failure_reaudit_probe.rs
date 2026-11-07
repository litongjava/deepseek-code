//! Regression for the second audit: a remote error body or message must never
//! reproduce request content, in any field, spacing or escaping.
use deepseek_code_core::performance::{safe_remote_error, sanitize_message, RequestFailure};

const PRIVATE: &str = "PRIVATE_FIXTURE_PROMPT";

fn json_body(value: serde_json::Value) -> String {
    safe_remote_error(value.to_string().as_bytes(), "application/json")
}

#[test]
fn a_spaced_input_echo_in_the_error_message_is_withheld() {
    let text = json_body(serde_json::json!({
        "error": {"message": format!("rejected payload: {{\"input\" : \"{PRIVATE}\"}}")}
    }));
    assert!(!text.contains(PRIVATE), "spaced request input leaked: {text}");
}

#[test]
fn request_structures_are_withheld_in_every_supported_shape() {
    let shapes = [
        // Different whitespace around the key and colon.
        format!("{{\"input\":\"{PRIVATE}\"}}"),
        format!("{{ \"input\" : \"{PRIVATE}\" }}"),
        format!("{{\n  \"input\"\n  :\n  \"{PRIVATE}\"\n}}"),
        // Nested conversation objects and single quotes.
        format!("{{\"messages\":[{{\"role\":\"user\",\"content\":\"{PRIVATE}\"}}]}}"),
        format!("{{'input': '{PRIVATE}'}}"),
        // Escaped JSON embedded in a string.
        format!("{{\\\"input\\\":\\\"{PRIVATE}\\\"}}"),
        // Arrays of request parts.
        format!("[\"{PRIVATE}\"]"),
    ];
    for shape in shapes {
        for field in ["message", "detail", "error_description"] {
            let key = if field == "message" {
                "error".to_owned()
            } else {
                field.to_owned()
            };
            let mut value = serde_json::json!({});
            if field == "message" {
                value["error"] = serde_json::json!({"message": shape});
            } else {
                value[&key] = serde_json::json!(shape);
            }
            let text = json_body(value);
            assert!(!text.contains(PRIVATE), "leaked via {field} shape {shape}: {text}");
        }
    }
}

#[test]
fn a_plain_text_body_is_never_reproduced() {
    let text = safe_remote_error(PRIVATE.as_bytes(), "text/plain");
    assert!(!text.contains(PRIVATE), "plain request echo leaked: {text}");
    assert!(text.contains("content omitted"), "{text}");

    // A plain-text status line keeps its meaning through a category only.
    let unauthorized = safe_remote_error(b"Unauthorized", "text/plain");
    assert!(!unauthorized.contains("Unauthorized"), "{unauthorized}");
    assert!(unauthorized.contains("authentication failed"), "{unauthorized}");
    assert!(unauthorized.contains("content omitted"), "{unauthorized}");

    let limited = safe_remote_error(b"rate limit exceeded, slow down", "text/plain");
    assert!(limited.contains("rate limited"), "{limited}");
    assert!(!limited.contains("exceeded"), "{limited}");
}

#[test]
fn a_structured_message_survives_wrapping_without_reproducing_the_body() {
    let raw = serde_json::json!({
        "error": {"message": format!("invalid: {{\"input\" : \"{PRIVATE}\"}}")}
    });
    let message = safe_remote_error(raw.to_string().as_bytes(), "application/json");
    let raw = anyhow::Error::new(deepseek_code_core::performance::ApiError {
        status: 400,
        retry_after: None,
        message,
    });
    let error = anyhow::Error::new(RequestFailure::from_error(&raw, 1))
        .context("API request failed");
    let diagnostic = deepseek_code_core::performance::request_diagnostic(&error);
    assert!(!diagnostic.contains(PRIVATE), "{diagnostic}");
    // The classification still works, so the failure stays diagnosable.
    assert!(diagnostic.contains("http=400"), "{diagnostic}");
    assert!(diagnostic.contains("class=permanent"), "{diagnostic}");
}

#[test]
fn credentials_inside_a_structured_message_are_redacted() {
    let result = sanitize_message("upstream failed: Api-Key: fixture-private-key");
    assert!(
        !result.contains("fixture-private-key"),
        "inline API key leaked: {result}"
    );
}

#[test]
fn a_legitimate_error_message_still_reaches_the_diagnostic() {
    let text = json_body(serde_json::json!({
        "error": {"message": "max_tokens is invalid", "type": "invalid_request_error"}
    }));
    assert!(text.contains("max_tokens is invalid"), "{text}");

    let failure = RequestFailure::from_error(
        &anyhow::Error::new(deepseek_code_core::performance::ApiError {
            status: 400,
            retry_after: None,
            message: text,
        }),
        1,
    );
    assert!(failure.diagnostic().contains("max_tokens is invalid"));
}
