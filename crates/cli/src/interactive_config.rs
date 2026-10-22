//! Interactive provider management. Credentials remain environment references.
use crate::terminal::input::TerminalInput;
use crate::{args::Cli, config, session::Session};
use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use claude_core::{config::ConfigManager, runtime::RuntimePaths};
use serde_json::json;

async fn ask(input: &mut TerminalInput, prompt: &str) -> Result<String> {
    let prompt = format!("{prompt}: ");
    let line = tokio::select! {
        line = input.read_answer(&prompt) => line?.context("Input closed")?,
        _ = tokio::signal::ctrl_c() => bail!("Cancelled"),
    };
    ensure!(line.trim() != "/cancel", "Cancelled");
    Ok(line.trim().into())
}

/// Asks until `check` accepts the answer, explaining each rejection.
async fn ask_valid(
    input: &mut TerminalInput,
    prompt: &str,
    default: Option<&str>,
    check: impl Fn(&str) -> std::result::Result<String, String>,
) -> Result<String> {
    let label = match default {
        Some(default) => format!("{prompt} [{default}]"),
        None => prompt.to_owned(),
    };
    for _ in 0..3 {
        let answer = ask(input, &label).await?;
        let answer = if answer.is_empty() { default.unwrap_or_default().to_owned() } else { answer };
        match check(&answer) {
            Ok(value) => return Ok(value),
            Err(reason) => eprintln!("  {reason}"),
        }
    }
    bail!("Provider setup cancelled after three invalid answers; nothing was saved")
}

fn looks_like_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    ["sk-", "sk_", "key-", "bearer "].iter().any(|prefix| lower.starts_with(prefix))
        || (value.len() >= 32 && value.chars().any(|c| c.is_ascii_lowercase()) && value.chars().any(|c| c.is_ascii_digit()))
}

fn check_env_name(value: &str) -> std::result::Result<String, String> {
    if looks_like_secret(value) {
        return Err("That looks like an API key, not a variable name, and it was not saved. Put the key in an environment variable first (bash: export DEEPSEEK_API_KEY=sk-...; Windows: setx DEEPSEEK_API_KEY sk-... and open a new terminal), then enter the variable's name, e.g. DEEPSEEK_API_KEY.".into());
    }
    if value.is_empty()
        || value.as_bytes()[0].is_ascii_digit()
        || !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err("Enter an environment variable NAME: letters, digits and _, not starting with a digit (e.g. DEEPSEEK_API_KEY).".into());
    }
    Ok(value.to_owned())
}

/// DeepSeek serves the Anthropic protocol under `/anthropic`.
fn deepseek_base_url(protocol: &str, url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let rest = trimmed.split_once("://")?.1;
    let host = rest.split(['/', ':']).next()?.to_ascii_lowercase();
    (host == "api.deepseek.com" && protocol == "anthropic" && !trimmed.ends_with("/anthropic"))
        .then(|| format!("{trimmed}/anthropic"))
}

pub async fn wizard(
    paths: &RuntimePaths,
    input: &mut TerminalInput,
    edit: Option<&str>,
) -> Result<String> {
    paths.read_config()?;
    eprintln!("Provider setup; /cancel cancels. Enter environment variable NAMES, never API keys.");
    let mut manager = ConfigManager::new(paths.data_dir.clone());
    let id = match edit {
        Some(id) => id.to_owned(),
        None => {
            ask_valid(input, "Provider ID", None, |id| {
                if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                    Ok(id.to_owned())
                } else {
                    Err("Use letters, digits, - or _ for the ID (e.g. deepseek).".into())
                }
            })
            .await?
        }
    };
    let previous = manager.get_provider(&id).cloned();
    ensure!(
        edit.is_some() || previous.is_none(),
        "Provider exists; use /provider edit {id}"
    );
    ensure!(edit.is_none() || previous.is_some(), "Provider not found");
    let protocol = ask_valid(input, "Protocol (openai / anthropic / gemini)", None, |value| {
        let value = value.to_ascii_lowercase();
        if ["openai", "anthropic", "claude", "gemini"].contains(&value.as_str()) {
            Ok(value)
        } else {
            Err("Enter openai, anthropic or gemini.".into())
        }
    })
    .await?;
    let url = ask_valid(input, "Base URL", None, |url| {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            Err("The URL must start with http:// or https:// (e.g. https://api.deepseek.com).".into())
        } else if url.contains('@') || url.contains('?') || url.contains('#') {
            Err("Leave credentials, query strings and fragments out of the URL; keys go in an environment variable.".into())
        } else {
            Ok(url.to_owned())
        }
    })
    .await?;
    let url = match deepseek_base_url(&protocol, &url) {
        Some(corrected) => {
            eprintln!("  DeepSeek serves the Anthropic protocol at {corrected}; using that URL.");
            corrected
        }
        None => url,
    };
    let suggested = url.contains("deepseek.com").then_some("DEEPSEEK_API_KEY");
    let key_env = ask_valid(input, "API key environment variable name", suggested, check_env_name).await?;
    if std::env::var_os(&key_env).is_none() {
        eprintln!("  Note: {key_env} is not set in this terminal; set it before selecting this provider.");
    }
    let models = ask_valid(input, "Model IDs (comma separated; first is default)", None, |models| {
        let models: Vec<_> = models.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        if models.is_empty() {
            Err("Enter at least one model ID (e.g. deepseek-flash).".into())
        } else if models.iter().any(|m| m.contains("::") || m.contains(char::is_whitespace)) {
            Err("Model IDs cannot contain spaces or ::; separate several with commas.".into())
        } else {
            Ok(models.join(","))
        }
    })
    .await?;
    let models: Vec<_> = models.split(',').collect();
    let mut provider = previous.unwrap_or_else(|| config::provider_template(models[0]));
    provider.id = id.clone();
    provider.name = id.clone();
    provider.provider_type = if protocol == "claude" {
        "anthropic".into()
    } else {
        protocol.clone()
    };
    provider.base_url = url;
    provider.api_key = None;
    provider.enabled = true;
    provider.settings.remove("builtinId");
    provider.settings.insert("apiKeyEnv".into(), json!(key_env));
    provider.models = models
        .iter()
        .map(|id| {
            let mut model = config::provider_template(id).models.remove(0);
            model.id = (*id).into();
            model.name = (*id).into();
            if protocol == "gemini" {
                model.api = Some("google-generative-ai".into());
            }
            model
        })
        .collect();
    if protocol == "gemini" {
        provider
            .settings
            .insert("builtinId".into(), json!("google"));
    }
    manager.update_config(|c| {
        c.providers.retain(|p| p.id != id);
        c.providers.push(provider);
    })?;
    if std::env::var_os(&key_env).is_some() {
        eprintln!("Saved {id} ({} models); the API key is read from {key_env}.", models.len());
    } else {
        eprintln!(
            "Saved {id} ({} models). Set {key_env} in the launching shell before selecting it.",
            models.len()
        );
    }
    Ok(id)
}

pub fn select(paths: &RuntimePaths, provider: &str, model: &str) -> Result<config::Selection> {
    // Deliberately discard startup endpoint/auth overrides on an explicit switch.
    let mut cli = Cli::parse_from(["dcl"]);
    cli.provider = Some(provider.into());
    cli.model = Some(model.into());
    cli.base_url = None;
    config::select(&cli, paths, None, None)
}

pub fn status(session: &Session) {
    eprintln!(
        "Provider: {}\nModel: {}\nRouting: {}",
        session.provider_id,
        session.model,
        if session.auto_route {
            "automatic"
        } else {
            "selected provider"
        }
    );
}

pub async fn setup(paths: &RuntimePaths, input: &mut TerminalInput) -> Result<config::Selection> {
    loop {
        for p in paths.read_config()?.providers.iter().filter(|p| p.enabled) {
            eprintln!(
                "{}: {}",
                p.id,
                p.models
                    .iter()
                    .filter(|m| m.enabled)
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let choice = ask(input, "Provider ID, 'add' to configure, or /cancel to exit").await?;
        let result: Result<Option<config::Selection>> = async {
            if choice == "add" {
                wizard(paths, input, None).await?;
                return Ok(None);
            }
            let model = ask(input, "Model ID").await?;
            Ok(Some(select(paths, &choice, &model)?))
        }
        .await;
        match result {
            Ok(Some(selection)) => return Ok(selection),
            Ok(None) => {}
            Err(error) => eprintln!("Error: {error:#}"),
        }
    }
}

pub async fn handle(
    line: &str,
    paths: &RuntimePaths,
    session: &mut Session,
    input: &mut TerminalInput,
) -> Result<bool> {
    let mut words = line.split_whitespace();
    let command = words.next().unwrap_or_default();
    if command == "/statusline" {
        crate::statusline_commands::handle(line.strip_prefix("/statusline").unwrap().trim(), session, input).await?;
        return Ok(true);
    }
    if command == "/config" {
        return settings_menu(
            line.strip_prefix("/config").unwrap().trim(),
            paths,
            session,
            input,
        )
        .await
        .map(|()| true);
    }
    if !["/provider", "/model", "/test"].contains(&command) {
        return Ok(false);
    }
    if command == "/test" {
        ensure!(words.next().is_none(), "Usage: /test");
        status(session);
        eprintln!(
            "Testing a real text request (no tools or conversation history); Ctrl+C cancels."
        );
        let started = std::time::Instant::now();
        let response = tokio::select! {
            result = session.engine.generate_text(&session.provider_id, &session.model, "This is a connection test. Reply only OK.", "Reply OK.") => result?,
            _ = tokio::signal::ctrl_c() => bail!("Test cancelled"),
        };
        eprintln!(
            "Connection test passed in {:.2}s. Response: {}",
            started.elapsed().as_secs_f64(),
            response.chars().take(200).collect::<String>()
        );
        return Ok(true);
    }
    let argument = words.next();
    if command == "/provider" && matches!(argument, Some("add" | "edit")) {
        let edit = if argument == Some("edit") {
            Some(words.next().context("Usage: /provider edit ID")?)
        } else {
            None
        };
        ensure!(words.next().is_none(), "Unexpected argument");
        let id = wizard(paths, input, edit).await?;
        let answer = ask(input, &format!("Switch this session to {id} now? [Y/n]")).await?;
        if !answer.eq_ignore_ascii_case("n") && !answer.eq_ignore_ascii_case("no") {
            let config = paths.read_config()?;
            let model = config
                .providers
                .iter()
                .find(|provider| provider.id == id)
                .and_then(|provider| provider.models.iter().find(|model| model.enabled))
                .map(|model| model.id.clone())
                .context("The saved provider has no enabled models")?;
            session.switch(paths, select(paths, &id, &model)?).await?;
            status(session);
        }
        return Ok(true);
    }
    ensure!(
        words.next().is_none(),
        "Use /provider [ID|add|edit ID] or /model [ID]"
    );
    let config = paths.read_config()?;
    let providers: Vec<_> = config.providers.iter().filter(|p| p.enabled).collect();
    let (provider, model) = if command == "/provider" {
        for (i, p) in providers.iter().enumerate() {
            eprintln!("{}. {} — {} ({})", i + 1, p.id, p.name, p.provider_type);
        }
        if argument == Some("list") {
            return Ok(true);
        }
        ensure!(
            !providers.is_empty(),
            "No saved providers; use /provider add"
        );
        let choice = match argument {
            Some(id) => id.to_owned(),
            None => ask(input, "Provider ID or number (blank keeps current)").await?,
        };
        if choice.is_empty() {
            return Ok(true);
        }
        let provider = providers
            .iter()
            .find(|p| p.id == choice)
            .copied()
            .or_else(|| {
                choice
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| providers.get(i).copied())
            })
            .context("Provider not found")?;
        let model = provider
            .models
            .iter()
            .find(|m| m.enabled && m.id == session.model)
            .or_else(|| provider.models.iter().find(|m| m.enabled))
            .context("Provider has no enabled models")?;
        (provider.id.clone(), model.id.clone())
    } else {
        let provider = providers
            .iter()
            .find(|p| p.id == session.provider_id)
            .with_context(|| {
                let saved = providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>();
                let next = match saved.first() {
                    Some(first) => format!("Switch to a saved provider with /provider {first} (saved: {}).", saved.join(", ")),
                    None => "Start with -m MODEL to use another model, or save a provider with /provider add.".into(),
                };
                format!(
                    "The current provider ({}) comes from command-line options or environment variables and is not saved, so it has no model list. {next}",
                    session.provider_id
                )
            })?;
        let models: Vec<_> = provider.models.iter().filter(|m| m.enabled).collect();
        for (i, m) in models.iter().enumerate() {
            eprintln!("{}. {}", i + 1, m.id);
        }
        if argument == Some("list") {
            return Ok(true);
        }
        let choice = match argument {
            Some(id) => id.to_owned(),
            None => ask(input, "Model ID or number (blank keeps current)").await?,
        };
        if choice.is_empty() {
            return Ok(true);
        }
        let model = models
            .iter()
            .find(|m| m.id == choice)
            .copied()
            .or_else(|| {
                choice
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| models.get(i).copied())
            })
            .context("Model not found")?;
        (provider.id.clone(), model.id.clone())
    };
    let selection = select(paths, &provider, &model)?;
    session.switch(paths, selection).await?;
    status(session);
    eprintln!("Conversation history preserved. Use /test to verify connectivity.");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_answers_explain_rejections_and_fix_deepseek_urls() {
        let error = check_env_name("sk-170b23aa867d47839ee0a01eddf73ff5").unwrap_err();
        assert!(error.contains("looks like an API key") && error.contains("DEEPSEEK_API_KEY"));
        assert!(check_env_name("1KEY").unwrap_err().contains("letters, digits"));
        assert_eq!(check_env_name("DEEPSEEK_API_KEY").unwrap(), "DEEPSEEK_API_KEY");
        assert_eq!(
            deepseek_base_url("anthropic", "https://api.deepseek.com/").as_deref(),
            Some("https://api.deepseek.com/anthropic")
        );
        assert_eq!(deepseek_base_url("anthropic", "https://api.deepseek.com/anthropic"), None);
        assert_eq!(deepseek_base_url("openai", "https://api.deepseek.com"), None);
    }

    fn answers(lines: &[&str]) -> TerminalInput {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for line in lines {
            tx.send((*line).into()).unwrap();
        }
        rx.into()
    }

    #[tokio::test]
    async fn wizard_preserves_other_providers_and_never_saves_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().to_path_buf()).unwrap();
        for (id, protocol) in [("one", "openai"), ("two", "anthropic"), ("three", "gemini")] {
            wizard(
                &paths,
                &mut answers(&[
                    id,
                    protocol,
                    "http://127.0.0.1:8317",
                    "INTERACTIVE_TEST_UNSET_KEY",
                    "model-a,model-b",
                ]),
                None,
            )
            .await
            .unwrap();
        }
        let config = paths.read_config().unwrap();
        assert_eq!(config.providers.len(), 3);
        assert!(config.providers.iter().all(|p| p.api_key.is_none()));
        let gemini = config.providers.iter().find(|p| p.id == "three").unwrap();
        assert_eq!(gemini.settings["builtinId"], "google");
        assert_eq!(
            gemini.models[0].api.as_deref(),
            Some("google-generative-ai")
        );
        assert!(select(&paths, "three", "model-a").is_err());
        let before = std::fs::read(paths.data_dir.join("config.json")).unwrap();
        assert!(wizard(&paths, &mut answers(&["one"]), None).await.is_err());
        assert!(wizard(&paths, &mut answers(&["/cancel"]), Some("one"))
            .await
            .is_err());
        assert_eq!(
            before,
            std::fs::read(paths.data_dir.join("config.json")).unwrap()
        );
        wizard(
            &paths,
            &mut answers(&[
                "openai",
                "http://localhost:9999/v1",
                "INTERACTIVE_TEST_UNSET_KEY",
                "new-model",
            ]),
            Some("one"),
        )
        .await
        .unwrap();
        assert_eq!(paths.read_config().unwrap().providers.len(), 3);
    }

    #[tokio::test]
    async fn switching_and_testing_preserves_history_and_failure_preserves_selection() {
        use axum::{routing::post, Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/v1/chat/completions", post(|Json(body): Json<serde_json::Value>| async move {
            assert!(body.get("tools").is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)));
            assert!(!body.to_string().contains("private-history-marker"));
            Json(json!({"id":"test","object":"chat.completion","created":0,"model":"model-a","choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}))
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().to_path_buf()).unwrap();
        let mut manager = ConfigManager::new(paths.data_dir.clone());
        for id in ["one", "two"] {
            let mut p = config::provider_template("model-a");
            p.id = id.into();
            p.base_url = url.clone();
            p.api_key = Some("mock-only".into());
            manager.add_provider(p).unwrap();
        }
        let cli = Cli::parse_from(["dcl"]);
        let mut session = Session::open(
            &cli,
            &paths,
            select(&paths, "one", "model-a").unwrap(),
            None,
        )
        .await
        .unwrap();
        session
            .store
            .append(&session.id, "user", "private-history-marker")
            .unwrap();
        let before = serde_json::to_value(session.store.history(&session.id).unwrap()).unwrap();
        let id = session.id.clone();
        let mut menu_input = answers(&["theme", "light", "done"]);
        handle("/config", &paths, &mut session, &mut menu_input)
            .await
            .unwrap();
        assert_eq!(
            crate::display_commands::load(&session).unwrap().theme,
            "light"
        );
        let mut input = answers(&[]);
        handle("/config permissions plan", &paths, &mut session, &mut input)
            .await
            .unwrap();
        assert_eq!(session.permission_mode(), "plan");
        assert!(handle(
            "/config invalid-setting value",
            &paths,
            &mut session,
            &mut input
        )
        .await
        .is_err());
        assert_eq!(session.permission_mode(), "plan");
        handle("/provider two", &paths, &mut session, &mut input)
            .await
            .unwrap();
        assert_eq!(session.provider_id, "two");
        assert!(handle("/model missing", &paths, &mut session, &mut input)
            .await
            .is_err());
        assert_eq!(session.provider_id, "two");
        assert_eq!(session.model, "model-a");
        handle("/test", &paths, &mut session, &mut input)
            .await
            .unwrap();
        assert_eq!(session.id, id);
        assert_eq!(
            before,
            serde_json::to_value(session.store.history(&session.id).unwrap()).unwrap()
        );
        let mut failed = select(&paths, "two", "model-a").unwrap();
        for provider in &mut failed.providers {
            if provider.id == "two" {
                provider.base_url = format!("{url}/missing");
            }
        }
        session.switch(&paths, failed).await.unwrap();
        assert!(handle("/test", &paths, &mut session, &mut input)
            .await
            .is_err());
        assert_eq!(
            before,
            serde_json::to_value(session.store.history(&session.id).unwrap()).unwrap()
        );
        let resumed = session.store.resume(&session.id).unwrap();
        assert_eq!(resumed.provider.as_deref(), Some("two"));
        session.close().await.unwrap();
        server.abort();
    }
}

async fn settings_menu(
    argument: &str,
    paths: &RuntimePaths,
    session: &mut Session,
    input: &mut TerminalInput,
) -> Result<()> {
    let mut direct = if argument.is_empty() {
        None
    } else {
        Some(argument.to_owned())
    };
    loop {
        let interactive = direct.is_none();
        let choice = if let Some(argument) = direct.take() {
            argument
        } else {
            let display = crate::display_commands::load(session)?;
            eprintln!("Settings | {}\nProvider: {} | Model: {}\nPermissions: {} | Theme: {} | Color: {} | Focus: {} | Renderer: {}\nprovider | model | permissions | theme | color | focus | output-style | statusline | tui | effort | autocompact\nChoose a setting, or enter done to return.", paths.data_dir.display(), session.provider_id, session.model, session.permission_mode(), display.theme, display.color, display.focus, display.tui);
            ask(input, "Setting").await?
        };
        if matches!(choice.as_str(), "" | "done" | "exit") {
            return Ok(());
        }
        let (setting, value) = choice
            .split_once(char::is_whitespace)
            .unwrap_or((&choice, ""));
        ensure!(
            [
                "provider",
                "model",
                "permissions",
                "theme",
                "color",
                "focus",
                "output-style",
                "statusline",
                "tui",
                "effort",
                "autocompact"
            ]
            .contains(&setting),
            "Unknown setting"
        );
        let value = if value.trim().is_empty() && !matches!(setting, "provider" | "model") {
            ask(
                input,
                &format!("{setting} value (blank uses command default)"),
            )
            .await?
        } else {
            value.trim().to_owned()
        };
        if matches!(setting, "provider" | "model") {
            Box::pin(handle(
                &format!("/{setting} {value}"),
                paths,
                session,
                input,
            ))
            .await?;
        } else if setting == "statusline" {
            crate::statusline_commands::handle(&value, session, input).await?;
        } else if setting == "permissions" {
            if value.is_empty() {
                eprintln!("Permission mode: {}", session.permission_mode());
            } else {
                session.set_permission_mode(&value).await?;
            }
        } else if matches!(setting, "effort" | "autocompact") {
            crate::session_commands::handle(&format!("/{setting}"), &value, paths, session).await?;
        } else {
            crate::display_commands::handle(&format!("/{setting}"), &value, paths, session).await?;
        }
        if !interactive {
            return Ok(());
        }
    }
}
