//! Reviewable feedback drafts; submission requires a separate explicit action.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const ENDPOINT: &str = "https://api.anthropic.com/api/claude_cli_feedback";
const LIMIT: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub struct Draft {
    pub id: String,
    pub session_id: String,
    pub includes_transcript: bool,
    pub content: Value,
    pub submitted_id: Option<String>,
}

pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut result = text.to_owned();
    let mut secrets: Vec<_> = secrets.iter().filter(|secret| !secret.is_empty()).collect();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in secrets {
        result = result.replace(secret, "[REDACTED]");
    }
    static PATTERNS: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    for pattern in PATTERNS.get_or_init(|| [
        r"(?i)\b(?:sk-ant-|sk-|ghp_|github_pat_)[a-z0-9_-]{10,}",
        r"\bAKIA[A-Z0-9]{16}\b",
        r"\bAIza[A-Za-z0-9_-]{35}\b",
        r"(?i)\b(?:bearer\s+)[a-z0-9._~+/-]+=*",
        r#"(?i)(?:api[_-]?key|access[_-]?token|refresh[_-]?token|secret|password|authorization)\s*["']?\s*[:=]\s*["']?[^\s"',}\]]+"#,
    ].into_iter().map(|pattern| regex::Regex::new(pattern).expect("constant feedback pattern")).collect()) {
        result = pattern
            .replace_all(&result, "[REDACTED]")
            .into_owned();
    }
    result
}

fn redact_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => *text = redact(text, secrets),
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| redact_value(value, secrets)),
        Value::Object(values) => {
            for (key, value) in values {
                let key: String = key
                    .chars()
                    .filter(|ch| ch.is_alphanumeric())
                    .flat_map(char::to_lowercase)
                    .collect();
                if [
                    "apikey",
                    "accesstoken",
                    "refreshtoken",
                    "password",
                    "secret",
                    "authorization",
                ]
                .iter()
                .any(|s| key.contains(s))
                {
                    *value = json!("[REDACTED]");
                } else {
                    redact_value(value, secrets);
                }
            }
        }
        _ => {}
    }
}

pub fn path(root: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(id).is_ok(),
        "Invalid feedback draft ID"
    );
    Ok(root.join("feedback").join(format!("{id}.json")))
}

fn save(root: &Path, draft: &Draft) -> Result<PathBuf> {
    let path = path(root, &draft.id)?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let bytes = serde_json::to_vec_pretty(draft)?;
    ensure!(
        bytes.len() <= LIMIT,
        "Feedback exceeds 1 MiB; omit the transcript or shorten the description"
    );
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    // Windows inherited ACLs vary across installations. DPAPI additionally
    // binds transcript drafts to the current account; /feedback show decrypts.
    #[cfg(windows)]
    let bytes = [
        b"CLAUDE-FEEDBACK-DPAPI\n".as_slice(),
        &super::anthropic_auth::protect(&bytes)?,
    ]
    .concat();
    std::io::Write::write_all(&mut file, &bytes)?;
    file.persist(&path)?;
    Ok(path)
}

pub fn prepare(
    root: &Path,
    session_id: &str,
    description: &str,
    transcript: Option<Value>,
    secrets: &[String],
) -> Result<Draft> {
    ensure!(
        !description.trim().is_empty(),
        "Feedback description is required"
    );
    let includes_transcript = transcript.is_some();
    let transcript = transcript.unwrap_or_else(|| json!([]));
    let mut content = json!({
        "latestAssistantMessageId":null,
        "message_count":transcript.as_array().map_or(0, Vec::len),
        "datetime":chrono::Utc::now().to_rfc3339(),
        "description":description,
        "platform":std::env::consts::OS,
        "version":format!("deepseek-code/{}",env!("CARGO_PKG_VERSION")),
        "transcript":transcript,
    });
    redact_value(&mut content, secrets);
    let draft = Draft {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session_id.into(),
        includes_transcript,
        content,
        submitted_id: None,
    };
    save(root, &draft)?;
    Ok(draft)
}

pub fn load(root: &Path, id: &str, session_id: &str) -> Result<Draft> {
    let path = path(root, id)?;
    ensure!(
        path.metadata()?.len() <= (LIMIT + 65536) as u64,
        "Feedback draft is too large"
    );
    let bytes = std::fs::read(path)?;
    #[cfg(windows)]
    let bytes = if let Some(encrypted) = bytes.strip_prefix(b"CLAUDE-FEEDBACK-DPAPI\n") {
        super::anthropic_auth::unprotect(encrypted)?
    } else {
        bytes
    };
    ensure!(bytes.len() <= LIMIT, "Feedback draft is too large");
    let draft: Draft = serde_json::from_slice(&bytes)?;
    ensure!(
        draft.id == id && draft.session_id == session_id,
        "Feedback draft belongs to another session"
    );
    Ok(draft)
}

/// Only this explicit entry point can send a reviewed draft to the real service.
pub async fn submit(root: &Path, id: &str, session_id: &str, key: &str) -> Result<String> {
    submit_to(root, id, session_id, key, ENDPOINT).await
}

async fn submit_to(
    root: &Path,
    id: &str,
    session_id: &str,
    key: &str,
    endpoint: &str,
) -> Result<String> {
    let mut draft = load(root, id, session_id)?;
    ensure!(
        draft.submitted_id.is_none(),
        "This draft was already submitted"
    );
    ensure!(!key.is_empty(), "Anthropic account or API key is required");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let request = client
        .post(endpoint)
        .header(
            "User-Agent",
            concat!("deepseek-code-cli/", env!("CARGO_PKG_VERSION")),
        )
        .json(&json!({"content": serde_json::to_string(&draft.content)?}));
    let mut response =
        super::anthropic_auth::authorize(request, key, "https://api.anthropic.com", false)
            .await?
            .send()
            .await?;
    ensure!(
        response.status().is_success(),
        "Feedback service returned HTTP {}; draft retained",
        response.status().as_u16()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 65536,
            "Feedback response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    let response: Value = serde_json::from_slice(&bytes)?;
    let feedback_id = response["feedback_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .context("Feedback service did not confirm a feedback ID; draft retained")?
        .to_owned();
    draft.submitted_id = Some(feedback_id.clone());
    save(root, &draft).with_context(|| format!("Feedback accepted as {feedback_id}, but its local receipt could not be saved; do not resend this draft"))?;
    Ok(feedback_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn explicit_submission_uses_mock_and_preserves_draft_on_failure() {
        use axum::{routing::post, Router};
        let root = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route(
                        "/success",
                        post(
                            |headers: axum::http::HeaderMap,
                             axum::Json(body): axum::Json<Value>| async move {
                                assert_eq!(headers["x-api-key"], "mock-credential");
                                assert_eq!(
                                    headers["user-agent"],
                                    concat!("deepseek-code-cli/", env!("CARGO_PKG_VERSION"))
                                );
                                let content: Value =
                                    serde_json::from_str(body["content"].as_str().unwrap())
                                        .unwrap();
                                assert_eq!(content["description"], "Mock feedback");
                                assert_eq!(
                                    content["version"],
                                    concat!("deepseek-code/", env!("CARGO_PKG_VERSION"))
                                );
                                assert_eq!(content["transcript"], json!([]));
                                axum::Json(json!({"feedback_id":"mock-confirmed"}))
                            },
                        ),
                    )
                    .route(
                        "/failure",
                        post(|| async {
                            (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                "do not echo response secrets",
                            )
                        }),
                    )
                    .route(
                        "/invalid",
                        post(|| async { axum::Json(json!({"unconfirmed":true})) }),
                    ),
            )
            .await
            .unwrap();
        });
        let draft = prepare(root.path(), "session", "Mock feedback", None, &[]).unwrap();
        for endpoint in ["failure", "invalid"] {
            let error = submit_to(
                root.path(),
                &draft.id,
                "session",
                "mock-credential",
                &format!("{base}/{endpoint}"),
            )
            .await
            .unwrap_err();
            assert!(!error.to_string().contains("response secrets"));
            assert!(load(root.path(), &draft.id, "session")
                .unwrap()
                .submitted_id
                .is_none());
        }
        assert_eq!(
            submit_to(
                root.path(),
                &draft.id,
                "session",
                "mock-credential",
                &format!("{base}/success")
            )
            .await
            .unwrap(),
            "mock-confirmed"
        );
        assert_eq!(
            load(root.path(), &draft.id, "session")
                .unwrap()
                .submitted_id
                .as_deref(),
            Some("mock-confirmed")
        );
        assert!(submit_to(
            root.path(),
            &draft.id,
            "session",
            "mock-credential",
            &format!("{base}/success")
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("already submitted"));
        server.abort();
    }

    #[test]
    fn camel_case_credentials_and_overlapping_secrets_are_redacted() {
        let mut value = json!({"accessToken":"private-access", "nested":{"refreshToken":"private-refresh", "api-key":"private-api"},"text":"prefix-long-secret"});
        redact_value(&mut value, &["prefix".into(), "prefix-long-secret".into()]);
        let text = value.to_string();
        assert!(!text.contains("private-"));
        assert!(!text.contains("long-secret"));
    }

    #[test]
    fn draft_storage_is_account_private() {
        let root = tempfile::tempdir().unwrap();
        let draft = prepare(root.path(), "session", "private-description", None, &[]).unwrap();
        let path = path(root.path(), &draft.id).unwrap();
        #[cfg(windows)]
        assert!(!String::from_utf8_lossy(&std::fs::read(&path).unwrap())
            .contains("private-description"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            load(root.path(), &draft.id, "session").unwrap().content["description"],
            "private-description"
        );
    }
    #[test]
    fn draft_is_local_redacted_and_scoped() {
        let root = tempfile::tempdir().unwrap();
        let draft = prepare(
            root.path(),
            "session",
            "error for sk-ant-abcdefghijk123456, password=example",
            Some(json!([{"content":"Bearer secret123; key custom-secret","api_key":"structured"}])),
            &["custom-secret".into()],
        )
        .unwrap();
        let encoded = serde_json::to_string(&draft.content).unwrap();
        for secret in [
            "abcdefghijk",
            "example",
            "secret123",
            "custom-secret",
            "structured",
        ] {
            assert!(!encoded.contains(secret));
        }
        assert!(load(root.path(), &draft.id, "different").is_err());
        assert!(load(root.path(), "../invalid", "session").is_err());
        assert!(load(root.path(), &draft.id, "session")
            .unwrap()
            .submitted_id
            .is_none());
        assert_eq!(
            std::fs::read_dir(root.path().join("feedback"))
                .unwrap()
                .count(),
            1
        );
    }
}
