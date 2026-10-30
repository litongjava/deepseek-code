use crate::session::Session;
use anyhow::{ensure, Result};
use deepseek_code_core::services::{anthropic_auth, feedback};

pub async fn handle(command: &str, argument: &str, session: &Session) -> Result<bool> {
    if !matches!(command, "/feedback" | "/bug") {
        return Ok(false);
    }
    let argument = argument.trim();
    if argument.is_empty() {
        eprintln!("/feedback DESCRIPTION creates a local draft.\n/feedback share DESCRIPTION also includes the current transcript.\n/feedback show ID displays the exact draft.\n/feedback send ID sends that draft to Anthropic. Review it first; transcript redaction cannot detect every private detail.");
        return Ok(true);
    }
    let (verb, rest) = argument
        .split_once(char::is_whitespace)
        .unwrap_or((argument, ""));
    match verb {
        "show" => println!(
            "{}",
            serde_json::to_string_pretty(&feedback::load(
                &session.paths.data_dir,
                rest.trim(),
                &session.id
            )?)?
        ),
        "send" => {
            let key = if anthropic_auth::status(&session.paths.data_dir)?.authenticated {
                anthropic_auth::credential_marker(&session.paths.data_dir)?
            } else {
                let provider = session.selected_provider().await?;
                ensure!(provider.provider.base_url.trim_end_matches('/') == "https://api.anthropic.com", "Sign in with /login before sending feedback; the selected provider is not Anthropic");
                provider.provider.api_key
            };
            let id = tokio::select! {
                response = feedback::submit(&session.paths.data_dir, rest.trim(), &session.id, &key) => response?,
                _ = tokio::signal::ctrl_c() => anyhow::bail!("Feedback submission interrupted; delivery status is unknown. Draft retained."),
            };
            eprintln!("Feedback submitted: {id}");
        }
        _ => {
            let (description, transcript) = if verb == "share" {
                (
                    rest.trim(),
                    Some(serde_json::to_value(session.store.history(&session.id)?)?),
                )
            } else {
                (argument, None)
            };
            let mut secrets = session
                .paths
                .providers()?
                .into_iter()
                .map(|provider| provider.api_key)
                .collect::<Vec<_>>();
            if let Ok(provider) = session.selected_provider().await {
                secrets.push(provider.provider.api_key);
            }
            for name in [
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "CLAUDE_CODE_OAUTH_TOKEN",
            ] {
                if let Ok(value) = std::env::var(name) {
                    secrets.push(value);
                }
            }
            let draft = feedback::prepare(
                &session.paths.data_dir,
                &session.id,
                description,
                transcript,
                &secrets,
            )?;
            eprintln!("Local feedback draft: {}\n{}\nTranscript included: {}\nReview with /feedback show {}; send explicitly with /feedback send {}.", draft.id, feedback::path(&session.paths.data_dir, &draft.id)?.display(), draft.includes_transcript, draft.id, draft.id);
        }
    }
    Ok(true)
}
