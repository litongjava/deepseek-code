use deepseek_code_core::performance::{safe_remote_error, ApiError, RequestFailure};

#[test]
fn free_text_in_json_error_fields_does_not_leak() {
    for field in ["message", "detail", "error_description", "code"] {
        let mut body = serde_json::json!({});
        body[field] = serde_json::json!("PRIVATE_REVIEW3_PROMPT");
        let message = safe_remote_error(body.to_string().as_bytes(), "application/json");
        let raw = anyhow::Error::new(ApiError {
            status: 400,
            retry_after: None,
            message,
            server_request_id: None,
            remote_code: None,
        });
        let diagnostic = RequestFailure::from_error(&raw, 1).diagnostic();
        assert!(
            !diagnostic.contains("PRIVATE_REVIEW3_PROMPT"),
            "free text leaked via {field}"
        );
    }
}
