//! Platform-neutral checks: no child process, no POSIX shell. These run on
//! every test target, so the operation contract and its validation stay covered
//! where a `.sh` fixture cannot run. Windows executable and CLI integration
//! remain unverified until a Windows agent runs them.

use super::*;
use serde_json::json;

#[test]
fn only_read_operations_are_classified_as_reads() {
    for operation in [
        "repo_view",
        "issue_list",
        "issue_view",
        "pr_list",
        "pr_view",
    ] {
        assert!(super::is_read_operation(Some(operation)), "{operation}");
    }
    for operation in [
        "issue_create",
        "issue_comment",
        "pr_create",
        "pr_comment",
        "pr_merge",
        "repo_delete",
    ] {
        assert!(!super::is_read_operation(Some(operation)), "{operation}");
    }
    assert!(!super::is_read_operation(None));
    for operation in OPERATIONS {
        assert!(super::is_known_operation(operation), "{operation}");
    }
    assert!(!super::is_known_operation("repo_delete"));
}

#[test]
fn the_schema_exposes_exactly_the_supported_operations() {
    let definition = super::definition();
    assert_eq!(definition.name, "GitHub");
    let schema = &definition.input_schema;
    let operations: Vec<&str> = schema["properties"]["operation"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(operations, OPERATIONS);
    for forbidden in [
        "token", "url", "host", "confirm", "command", "args", "shell",
    ] {
        assert!(
            schema["properties"].get(forbidden).is_none(),
            "{forbidden} must not be an input: {schema}"
        );
    }
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(["operation"]));
    assert_eq!(
        schema["properties"]["merge_method"]["enum"],
        json!(["merge", "squash", "rebase"])
    );
    assert_eq!(schema["properties"]["per_page"]["maximum"], MAX_PER_PAGE);
}

#[test]
fn a_repository_must_be_a_plain_owner_and_name() {
    assert_eq!(
        super::repository(&json!({"repo":"octo/demo"})).unwrap(),
        Some("octo/demo".into())
    );
    assert_eq!(super::repository(&json!({})).unwrap(), None);
    for repo in [
        "../../etc",
        "octo",
        "octo/de mo",
        "octo/demo/extra",
        "-R/--repo",
        "octo/-x",
        "./demo",
        "octo/demo?x=1",
    ] {
        assert!(
            super::repository(&json!({"repo":repo})).is_err(),
            "{repo} was accepted"
        );
    }
}

#[test]
fn merge_rejects_unknown_methods_and_never_builds_a_forcing_flag() {
    assert_eq!(super::merge_method(&json!({})).unwrap(), "merge");
    assert_eq!(
        super::merge_method(&json!({"merge_method":"squash"})).unwrap(),
        "squash"
    );
    assert!(super::merge_method(&json!({"merge_method":"force"})).is_err());
    let (args, body) = super::write_arguments(
        "pr_merge",
        Some("octo/demo"),
        &json!({"number":4,"merge_method":"rebase","match_head_commit":"abc1234"}),
    )
    .unwrap();
    assert!(body.is_none(), "a merge carries no body");
    assert!(args.contains(&"--rebase".to_owned()), "{args:?}");
    for forbidden in ["--admin", "--auto", "--delete-branch", "--disable-auto"] {
        assert!(!args.contains(&forbidden.to_owned()), "{args:?}");
    }
    assert!(super::write_arguments(
        "pr_merge",
        None,
        &json!({"number":4,"match_head_commit":"not hex"})
    )
    .is_err());
}

#[test]
fn a_create_body_is_never_an_argument() {
    let (args, body) = super::write_arguments(
        "issue_create",
        Some("octo/demo"),
        &json!({"title":"t","body":"line 1
--looks-like-a-flag"}),
    )
    .unwrap();
    assert_eq!(
        body.as_deref(),
        Some(
            "line 1
--looks-like-a-flag"
        )
    );
    assert!(args.iter().any(|arg| arg == "--body-file"), "{args:?}");
    assert!(args.iter().all(|arg| !arg.contains("line 1")), "{args:?}");
    // A comment must have text; a merge must not.
    assert!(super::write_arguments("issue_comment", None, &json!({"number":1})).is_err());
}

#[test]
fn stderr_tokens_are_redacted() {
    let text = "403 from ghp_S3CR3T and github_pat_S3CR3T tail";
    let redacted = super::redact(text);
    assert!(!redacted.contains("S3CR3T"), "{redacted}");
    assert_eq!(
        redacted.matches("[redacted token]").count(),
        2,
        "{redacted}"
    );
    assert!(redacted.ends_with("tail"), "{redacted}");
}

#[test]
fn pagination_links_are_read_from_the_next_entry_only() {
    let headers = "HTTP/2.0 200 OK\nLink: <https://api.github.com/x?page=5>; rel=\"next\", <https://api.github.com/x?page=9>; rel=\"last\"";
    assert_eq!(super::next_page(headers, 4), Some(5));
    assert_eq!(
        super::next_page("HTTP/2.0 200 OK\nLink: <x?page=9>; rel=\"last\"", 8),
        None
    );
    assert_eq!(super::next_page("HTTP/2.0 200 OK", 1), None);
    // A next link never goes backwards, even if a server sends a stale one.
    assert_eq!(
        super::next_page("Link: <x?page=1>; rel=\"next\"", 7),
        Some(8)
    );
}

#[test]
fn include_output_is_split_at_the_first_blank_line() {
    let (headers, body) = super::split_include("HTTP/2.0 200 OK\nLink: x\n\n[1,2]").unwrap();
    assert!(headers.contains("Link: x"));
    assert_eq!(body, "[1,2]");
    let (headers, body) = super::split_include("HTTP/2.0 200 OK\r\nLink: y\r\n\r\n[]").unwrap();
    assert!(headers.contains("Link: y"));
    assert_eq!(body, "[]");
    assert!(super::split_include("[1,2]").is_none());
}

#[test]
fn labels_cannot_change_the_query_shape() {
    assert_eq!(super::percent_encode("help wanted"), "help%20wanted");
    assert_eq!(super::percent_encode("a&b=c"), "a%26b%3Dc");
}

/// `GET /repos/{owner}/{repo}/pulls` has no label parameter, so a label filter
/// there would be silently dropped by GitHub. It is refused as invalid input on
/// every platform, and the advertised contract says so.
#[test]
fn the_label_filter_belongs_to_the_issue_list_only() {
    assert!(super::ensure_labels_supported("issue_list", &["bug".to_owned()]).is_ok());
    assert!(super::ensure_labels_supported("pr_list", &[]).is_ok());
    let error = super::ensure_labels_supported("pr_list", &["bug".to_owned()]).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("pr_list does not support labels"),
        "{message}"
    );

    let definition = super::definition();
    let labels = definition.input_schema["properties"]["labels"]["description"]
        .as_str()
        .unwrap();
    assert!(labels.contains("issue_list"), "{labels}");
    assert!(!labels.contains("on a list"), "{labels}");
    assert!(
        definition
            .description
            .contains("only issue_list accepts a labels filter"),
        "{}",
        definition.description
    );
}
