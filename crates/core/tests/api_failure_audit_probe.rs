use deepseek_code_core::performance::{sanitize_message, ApiError, RequestFailure};

#[test]
fn all_cookie_values_must_be_redacted() {
    let text = sanitize_message("Cookie: first=fixture-one; second=fixture-two");
    assert!(!text.contains("fixture-two"), "second cookie leaked");
}

#[test]
fn basic_proxy_credentials_must_be_redacted() {
    let text = sanitize_message("Proxy-Authorization: Basic Zml4dHVyZTpwYXNz");
    assert!(
        !text.contains("Zml4dHVyZTpwYXNz"),
        "Basic credential leaked"
    );
}

#[test]
fn echoed_request_body_must_not_enter_diagnostics() {
    let raw = anyhow::Error::new(ApiError {
        status: 400,
        retry_after: None,
        message: r#"{"error":{"message":"invalid parameter"},"request":{"messages":[{"role":"user","content":"fixture-private-prompt"}]}}"#.into(),
    });
    let text = RequestFailure::from_error(&raw, 1).diagnostic();
    assert!(
        !text.contains("fixture-private-prompt"),
        "echoed request body leaked"
    );
}
