//! Explicit account actions. No credentials are imported from the official CLI.
use crate::{
    config::{self, Selection},
    session::Session,
};
use anyhow::{ensure, Result};
use deepseek_code_core::{config::ConfigManager, runtime::RuntimePaths, services::anthropic_auth};

const PROVIDER_ID: &str = "anthropic-oauth";

pub async fn run(paths: &RuntimePaths, command: &crate::args::AuthCommands) -> Result<()> {
    match command {
        crate::args::AuthCommands::Login => {
            login_account(paths).await?;
            eprintln!("Signed in. Start dcl to use the Anthropic account provider.");
        }
        crate::args::AuthCommands::Logout => {
            remove_account(paths).await?;
            eprintln!("Signed out locally. Official CLI and browser sessions were not changed.");
        }
        crate::args::AuthCommands::Status => println!(
            "{}",
            serde_json::to_string_pretty(&anthropic_auth::status(&paths.data_dir)?)?
        ),
    }
    Ok(())
}

async fn login_account(paths: &RuntimePaths) -> Result<()> {
    let flow = anthropic_auth::begin_login().await?;
    eprintln!("Authorize this Rust CLI in your browser. Callback expires in five minutes; Ctrl+C cancels.\n{}", flow.authorization_url);
    if let Err(error) = anthropic_auth::open_browser(&flow.authorization_url) {
        eprintln!("{error}. Open the URL above manually.");
    }
    tokio::select! {
        result = flow.finish(&paths.data_dir) => { result?; },
        _ = tokio::signal::ctrl_c() => anyhow::bail!("Login cancelled; callback listener closed"),
    }
    install_provider(paths)
}

async fn remove_account(paths: &RuntimePaths) -> Result<()> {
    anthropic_auth::logout(&paths.data_dir).await?;
    paths.read_config()?;
    let mut manager = ConfigManager::new(paths.data_dir.clone());
    manager.update_config(|config| {
        config.providers.retain(|provider| {
            provider.id != PROVIDER_ID
                || !provider
                    .api_key
                    .as_deref()
                    .is_some_and(anthropic_auth::is_oauth_marker)
        });
        if config.default_provider_id.as_deref() == Some(PROVIDER_ID) {
            config.default_provider_id = None;
        }
    })
}

pub async fn handle(
    command: &str,
    argument: &str,
    paths: &RuntimePaths,
    session: &mut Session,
) -> Result<bool> {
    match command {
        "/login" => {
            ensure!(
                argument.trim().is_empty() || argument.trim() == "status",
                "Usage: /login [status]"
            );
            if argument.trim() == "status" {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&anthropic_auth::status(&paths.data_dir)?)?
                );
                return Ok(true);
            }
            login_account(paths).await?;
            session
                .switch(
                    paths,
                    Selection {
                        providers: paths.providers()?,
                        provider_id: PROVIDER_ID.into(),
                        model: "claude-sonnet-4-6".into(),
                    },
                )
                .await?;
            eprintln!("Signed in to Anthropic for this application. Selected Claude Sonnet 4.6. Credentials are stored separately from the official CLI.");
        }
        "/logout" => {
            ensure!(argument.trim().is_empty(), "Usage: /logout");
            remove_account(paths).await?;
            if session.provider_id == PROVIDER_ID {
                session.set_fast(false).await?;
                let providers = paths.providers()?;
                let fallback = providers
                    .iter()
                    .filter(|p| p.enabled && !p.api_key.is_empty())
                    .find_map(|p| {
                        p.models
                            .iter()
                            .find(|m| m.enabled)
                            .map(|m| (p.id.clone(), m.id.clone()))
                    });
                if let Some((provider_id, model)) = fallback {
                    session
                        .switch(
                            paths,
                            Selection {
                                providers,
                                provider_id,
                                model,
                            },
                        )
                        .await?;
                }
            }
            eprintln!("Signed out of this application's Anthropic account. Official CLI credentials were not changed. Browser/server-side sessions are not revoked.");
        }
        "/fast" => {
            let argument = argument.trim();
            ensure!(
                ["", "status", "on", "off"].contains(&argument),
                "Usage: /fast [on|off|status]"
            );
            let selected = session.selected_provider().await?;
            if argument == "on" {
                ensure!(
                    matches!(
                        selected.provider.api_format,
                        deepseek_code_core::native_engine::provider_manager::ApiFormat::Anthropic
                    ),
                    "Fast mode requires an Anthropic messages provider"
                );
                eprintln!("Checking Anthropic fast-mode account eligibility (extra usage billing may apply)…");
                tokio::select! {
                    result = anthropic_auth::check_fast_eligibility(&selected.provider.api_key, &selected.provider.base_url, &selected.model.id) => result?,
                    _ = tokio::signal::ctrl_c() => anyhow::bail!("Fast-mode check cancelled; setting unchanged"),
                }
                session.set_fast(true).await?;
            } else if argument == "off" {
                session.set_fast(false).await?;
            }
            let current = session.selected_provider().await?;
            eprintln!("Fast mode: {} for {}. This is a session setting; service eligibility and rate limits still apply.", if current.model.fast { "on" } else { "off" }, current.model.id);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn install_provider(paths: &RuntimePaths) -> Result<()> {
    paths.read_config()?;
    let mut provider = config::provider_template("claude-sonnet-4-6");
    provider.id = PROVIDER_ID.into();
    provider.name = "Anthropic account".into();
    provider.base_url = "https://api.anthropic.com".into();
    provider.api_key = Some(anthropic_auth::credential_marker(&paths.data_dir)?);
    provider
        .models
        .extend(config::provider_template("claude-opus-4-6").models);
    for model in &mut provider.models {
        model.context_window = Some(200_000);
        model.supports_vision = true;
    }
    let mut manager = ConfigManager::new(paths.data_dir.clone());
    manager.update_config(|config| {
        config
            .providers
            .retain(|existing| existing.id != PROVIDER_ID);
        config.providers.push(provider);
        config.default_provider_id = Some(PROVIDER_ID.into());
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_contains_reference_only_not_account_tokens() {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().to_path_buf()).unwrap();
        install_provider(&paths).unwrap();
        let providers = paths.providers().unwrap();
        let provider = providers.iter().find(|p| p.id == PROVIDER_ID).unwrap();
        assert!(anthropic_auth::is_oauth_marker(&provider.api_key));
        assert_eq!(provider.models.len(), 2);
        assert!(provider.models.iter().all(|model| !model.fast));
        let saved = std::fs::read_to_string(paths.data_dir.join("config.json")).unwrap();
        assert!(!saved.contains("access_token") && !saved.contains("refresh_token"));
    }
}
