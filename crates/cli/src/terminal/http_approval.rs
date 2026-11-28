use super::{change_view, printable, Approval, Value};

pub(crate) const TOOL: &str = "WebFetchHttp";
const SCOPES: [(&str, &str); 4] = [
    ("Allow once", "http_once"),
    ("Allow for this session", "http_session"),
    ("Allow for this project", "http_project"),
    ("Allow for this user across projects", "http_user"),
];

pub(super) fn menu(input: &Value) -> Approval {
    let origin = printable(input["origin"].as_str().unwrap_or_default());
    let mut lines = vec![
        change_view::heading("WebFetch: unencrypted HTTP"),
        change_view::body(&format!("  Origin: {origin}")),
        change_view::muted("  Traffic can be read or changed in transit."),
    ];
    if let Some(url) = input["url"].as_str() {
        lines.push(change_view::body(&format!("  Request: {}", printable(url))));
    }
    if let Some(reason) = input["reason"].as_str() {
        lines.push(change_view::muted(&format!(
            "  HTTPS: {}",
            printable(reason)
        )));
    }
    if input["local"] == true {
        lines.push(change_view::muted(
            "  This origin is on the local or private network.",
        ));
    }
    let mut options: Vec<String> = SCOPES.iter().map(|(label, _)| (*label).into()).collect();
    options.push("Deny".into());
    Approval {
        lines,
        question: "Allow plaintext HTTP to this origin?".into(),
        options,
        remember: None,
        auto: false,
        http: true,
    }
}

/// Only the four numbered choices grant a scope. Generic approvals must not grant HTTP access.
pub(crate) fn answer(selection: &str) -> &'static str {
    match selection.trim() {
        "1" => SCOPES[0].1,
        "2" => SCOPES[1].1,
        "3" => SCOPES[2].1,
        "4" => SCOPES[3].1,
        _ => "no",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn http_menu_numbers_match_engine_scopes_in_live_and_text_prompts() {
        let input = json!({"origin":"http://example.test:81", "url":"http://example.test:81/docs", "reason":"TLS\x1b[0m failed", "local":false});
        let menu = super::super::approval(TOOL, &input, None, true);
        let text = menu.text();
        assert!(text.contains("Origin: http://example.test:81"));
        assert!(text.contains("Request: http://example.test:81/docs"));
        assert!(text.contains("TLS[0m failed") && !text.contains('\x1b'));
        assert!(!text.contains("private network") && !text.contains("1/y"));
        for (i, (label, wire)) in [
            ("Allow once", "http_once"),
            ("Allow for this session", "http_session"),
            ("Allow for this project", "http_project"),
            ("Allow for this user across projects", "http_user"),
            ("Deny", "no"),
        ]
        .iter()
        .enumerate()
        {
            let n = i + 1;
            assert_eq!(&menu.options[i], label);
            assert!(text.contains(&format!("{n}. {label}")));
            assert_eq!(answer(&n.to_string()), *wire);
        }
        assert_eq!(menu.options.len(), 5);
    }

    #[test]
    fn local_http_prompt_discloses_private_access() {
        let text = menu(&json!({"origin":"http://localhost:8080", "local":true})).text();
        assert!(text.contains("origin is on the local or private network"));
    }

    #[test]
    fn deny_cancel_missing_or_generic_answers_grant_nothing() {
        for selection in [
            "5",
            "n",
            "esc",
            "cancel",
            "",
            "y",
            "yes",
            "0",
            "6",
            "http_user",
            "4\nextra",
        ] {
            assert_eq!(answer(selection), "no", "{selection:?}");
        }
    }
}
