//! Interactive provider management. Credentials are stored inline, or read
//! from the environment when the user names a variable.
use crate::terminal::input::TerminalInput;
use crate::{args::Cli, config, session::Session};
use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use deepseek_code_core::{config::ConfigManager, runtime::RuntimePaths};
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
async fn ask_valid<T>(
    input: &mut TerminalInput,
    prompt: &str,
    default: Option<&str>,
    check: impl Fn(&str) -> std::result::Result<T, String>,
) -> Result<T> {
    let label = match default {
        Some(default) => format!("{prompt} [{default}]"),
        None => prompt.to_owned(),
    };
    for _ in 0..3 {
        let answer = ask(input, &label).await?;
        let answer = if answer.is_empty() { default.unwrap_or_default().to_owned() } else { answer };
        match check(&answer) {
            Ok(value) => return Ok(value),
            Err(reason) => command_eprintln!("  {reason}"),
        }
    }
    bail!("Provider setup cancelled after three invalid answers; nothing was saved")
}

/// Where the wizard keeps the credential for the provider it saves.
#[derive(Debug)]
enum Credential {
    /// Written to `api_key` in config.json.
    Key(String),
    /// The name of an environment variable read at launch.
    Env(String),
}

fn looks_like_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    ["sk-", "sk_", "key-", "bearer "].iter().any(|prefix| lower.starts_with(prefix))
        || (value.len() >= 32 && value.chars().any(|c| c.is_ascii_lowercase()) && value.chars().any(|c| c.is_ascii_digit()))
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &value[prefix.len()..])
}

fn is_env_name(value: &str) -> bool {
    !value.is_empty()
        && !value.as_bytes()[0].is_ascii_digit()
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Variable names are conventionally upper case; anything else is more likely
/// a key the user pasted. `set` covers lower-case names already exported.
fn looks_like_env_name(value: &str) -> bool {
    is_env_name(value)
        && (value.chars().all(|c| !c.is_ascii_lowercase())
            || std::env::var_os(value).is_some())
}

fn key_credential(value: &str) -> std::result::Result<Credential, String> {
    let value = strip_prefix_ignore_case(value, "bearer ").unwrap_or(value).trim();
    if value.is_empty() {
        return Err("Enter an API key, or an environment variable NAME such as DEEPSEEK_API_KEY.".into());
    }
    if value.chars().any(char::is_whitespace) || value.chars().any(char::is_control) {
        return Err("An API key has no spaces or control characters; paste just the key, or enter an environment variable NAME (e.g. DEEPSEEK_API_KEY).".into());
    }
    Ok(Credential::Key(value.to_owned()))
}

fn env_credential(value: &str) -> std::result::Result<Credential, String> {
    if is_env_name(value) {
        Ok(Credential::Env(value.to_owned()))
    } else {
        Err("Enter an environment variable NAME: letters, digits and _, not starting with a digit (e.g. DEEPSEEK_API_KEY), or prefix a literal key with key:.".into())
    }
}

/// Accepts either an inline API key or the name of an environment variable.
/// `key:` and `env:` force one meaning when the value is ambiguous.
fn parse_credential(value: &str) -> std::result::Result<Credential, String> {
    let value = value.trim();
    if let Some(key) = strip_prefix_ignore_case(value, "key:") {
        return key_credential(key);
    }
    if let Some(name) = strip_prefix_ignore_case(value, "env:") {
        return env_credential(name.trim());
    }
    if looks_like_secret(value) {
        return key_credential(value);
    }
    if looks_like_env_name(value) {
        return Ok(Credential::Env(value.to_owned()));
    }
    key_credential(value)
}

/// DeepSeek serves the Anthropic protocol under `/anthropic`.
fn deepseek_base_url(protocol: &str, url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let rest = trimmed.split_once("://")?.1;
    let host = rest.split(['/', ':']).next()?.to_ascii_lowercase();
    (host == "api.deepseek.com" && protocol == "anthropic" && !trimmed.ends_with("/anthropic"))
        .then(|| format!("{trimmed}/anthropic"))
}

/// OpenCode Go serves each protocol under `/inference/go/...`; the bare host
/// is a web page. Returns the URL to use when the entered one is not an API base.
fn opencode_base_url(protocol: &str, url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let rest = trimmed.split_once("://")?.1;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.split(':').next()?.to_ascii_lowercase();
    let path = format!("/{path}");
    if host != "opencode.ai"
        || path.starts_with("/inference/go/")
        || path.starts_with("/zen")
    {
        return None;
    }
    let suffix = match protocol {
        "anthropic" | "claude" => "/inference/go/anthropic/v1",
        "gemini" => "/inference/go/google/v1beta",
        _ => "/inference/go/openai/v1",
    };
    Some(format!("https://opencode.ai{suffix}"))
}

/// The protocols the wizard offers, in prompt order.
const PROTOCOLS: &[(&str, &str)] = &[
    ("openai", "OpenAI Chat Completions-compatible endpoints"),
    ("anthropic", "Claude and Anthropic-compatible endpoints"),
    ("gemini", "Google Gemini"),
    ("responses", "OpenAI Responses-compatible endpoints"),
];

/// Accepts the numbered choice, or the protocol name for convenience.
fn parse_protocol(value: &str) -> std::result::Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "openai" => Ok("openai".into()),
        "2" | "anthropic" | "claude" => Ok("anthropic".into()),
        "3" | "gemini" => Ok("gemini".into()),
        "4" | "responses" | "openai-responses" => Ok("responses".into()),
        _ => Err("Enter 1, 2, 3 or 4 (openai, anthropic, gemini or responses).".into()),
    }
}

fn validate_model_ids(value: &str) -> std::result::Result<Vec<String>, String> {
    let models: Vec<_> = value.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if models.is_empty() {
        Err("Enter at least one model ID (e.g. deepseek-flash).".into())
    } else if models
        .iter()
        .any(|m| m.contains("::") || m.contains(char::is_whitespace))
    {
        Err("Model IDs cannot contain spaces or ::; separate several with commas.".into())
    } else {
        Ok(models.into_iter().map(str::to_owned).collect())
    }
}

fn push_unique(models: &mut Vec<String>, model: String) {
    if !models.contains(&model) {
        models.push(model);
    }
}

/// Reads comma-separated numbers from the listed models, or typed IDs.
fn parse_model_selection(
    value: &str,
    listed: &[String],
) -> std::result::Result<Vec<String>, String> {
    let tokens: Vec<_> = value.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if tokens.is_empty() {
        return Ok(vec![listed[0].clone()]);
    }
    let mut chosen = Vec::new();
    for token in tokens {
        if token.eq_ignore_ascii_case("all") {
            for model in listed {
                push_unique(&mut chosen, model.clone());
            }
        } else if let Ok(number) = token.parse::<usize>() {
            let model = number
                .checked_sub(1)
                .and_then(|index| listed.get(index))
                .ok_or_else(|| format!("Enter a number from 1 to {}, a model ID, or all.", listed.len()))?;
            push_unique(&mut chosen, model.clone());
        } else if token.contains("::") || token.contains(char::is_whitespace) {
            return Err("Model IDs cannot contain spaces or ::; separate several with commas.".into());
        } else {
            push_unique(&mut chosen, token.to_owned());
        }
    }
    Ok(chosen)
}

/// Lists the endpoint's models as a numbered choice. When the endpoint cannot
/// be reached, or the key is not in this terminal, typed IDs still work.
async fn choose_models(
    input: &mut TerminalInput,
    protocol: &str,
    url: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>> {
    let mut listing_failed = false;
    let listed = match api_key.filter(|key| !key.is_empty()) {
        Some(key) => {
            command_eprintln!("  Listing models from {url}…");
            match tokio::time::timeout(
                std::time::Duration::from_secs(20),
                deepseek_code_core::native_engine::model_listing::list_models(protocol, url, key),
            )
            .await
            {
                Ok(Ok(models)) => models,
                Ok(Err(error)) => {
                    command_eprintln!("  Could not list models: {error:#}");
                    listing_failed = true;
                    Vec::new()
                }
                Err(_) => {
                    command_eprintln!("  Listing models timed out.");
                    listing_failed = true;
                    Vec::new()
                }
            }
        }
        None => Vec::new(),
    };
    if listed.is_empty() {
        if listing_failed {
            command_eprintln!(
                "  Check the Base URL and key, or /cancel and run /provider add again."
            );
        }
        return ask_valid(
            input,
            "Model IDs (comma separated; first is default)",
            None,
            validate_model_ids,
        )
        .await;
    }
    command_eprintln!("Models available from {url}:");
    for (index, model) in listed.iter().enumerate() {
        command_eprintln!("  {:>2}. {model}", index + 1);
    }
    ask_valid(
        input,
        "Model numbers (comma separated; first is default, all for every model)",
        Some("1"),
        |value| parse_model_selection(value, &listed),
    )
    .await
}

pub async fn wizard(
    paths: &RuntimePaths,
    input: &mut TerminalInput,
    edit: Option<&str>,
) -> Result<String> {
    paths.read_config()?;
    command_eprintln!(
        "Provider setup; /cancel cancels. Enter an API key directly, or the NAME of an environment variable that holds it."
    );
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
    command_eprintln!("Protocol:");
    for (index, (name, description)) in PROTOCOLS.iter().enumerate() {
        command_eprintln!("  {}. {name} — {description}", index + 1);
    }
    let protocol = ask_valid(input, "Protocol (1-4)", Some("1"), parse_protocol).await?;
    let url = ask_valid(input, "Base URL", None, |url| {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            Err("The URL must start with http:// or https:// (e.g. https://api.deepseek.com).".into())
        } else if url.contains('@') || url.contains('?') || url.contains('#') {
            Err("Leave credentials, query strings and fragments out of the URL; enter the API key in the next question.".into())
        } else {
            Ok(url.to_owned())
        }
    })
    .await?;
    let url = match deepseek_base_url(&protocol, &url) {
        Some(corrected) => {
            command_eprintln!("  DeepSeek serves the Anthropic protocol at {corrected}; using that URL.");
            corrected
        }
        None => url,
    };
    let url = match opencode_base_url(&protocol, &url) {
        Some(corrected) => {
            command_eprintln!("  OpenCode Go serves this protocol at {corrected}; using that URL.");
            command_eprintln!(
                "  For every OpenCode model and protocol, choose the OpenCode Go preset (/provider add opencode-go)."
            );
            corrected
        }
        None => url,
    };
    let suggested = if url.contains("deepseek.com") {
        Some("DEEPSEEK_API_KEY")
    } else if url.contains("opencode.ai") {
        Some("OPENCODE_API_KEY")
    } else {
        None
    };
    let credential = ask_valid(
        input,
        "API key, or environment variable name (key:<key> forces a literal key)",
        suggested,
        parse_credential,
    )
    .await?;
    let (stored_key, key_env) = match credential {
        Credential::Key(key) => {
            command_eprintln!("  The key will be saved in config.json, readable only by you.");
            (Some(key), None)
        }
        Credential::Env(name) => {
            if std::env::var_os(&name).is_none() {
                command_eprintln!("  Note: {name} is not set in this terminal; set it before selecting this provider.");
            }
            (None, Some(name))
        }
    };
    let key_for_listing = match (&stored_key, &key_env) {
        (Some(key), _) => Some(key.clone()),
        (None, Some(name)) => std::env::var(name).ok(),
        (None, None) => None,
    };
    let models = choose_models(input, &protocol, &url, key_for_listing.as_deref()).await?;
    let mut provider = previous.unwrap_or_else(|| config::provider_template(&models[0]));
    provider.id = id.clone();
    provider.name = id.clone();
    provider.provider_type = if protocol == "responses" {
        "openai".into()
    } else if protocol == "claude" {
        "anthropic".into()
    } else {
        protocol.clone()
    };
    provider.base_url = url;
    provider.api_key = stored_key.clone();
    provider.enabled = true;
    provider.settings.remove("builtinId");
    provider.settings.remove("apiKeyEnv");
    if let Some(name) = &key_env {
        provider.settings.insert("apiKeyEnv".into(), json!(name));
    }
    provider.models = models
        .iter()
        .map(|id| {
            let mut model = config::provider_template(id).models.remove(0);
            model.id = id.clone();
            model.name = id.clone();
            if protocol == "responses" {
                model.api = Some("openai-responses".into());
            }
            if protocol == "gemini" {
                model.api = Some("google-generative-ai".into());
            }
            model
        })
        .collect();
    if protocol == "responses" {
        provider.settings.insert("builtinId".into(), json!("openai"));
    }
    if protocol == "gemini" {
        provider
            .settings
            .insert("builtinId".into(), json!("google"));
    }
    manager.update_config(|c| {
        c.providers.retain(|p| p.id != id);
        c.providers.push(provider);
    })?;
    match (&stored_key, &key_env) {
        (Some(_), _) => command_eprintln!(
            "Saved {id} ({} models); the API key is stored in config.json (readable only by you).",
            models.len()
        ),
        (None, Some(name)) if std::env::var_os(name).is_some() => {
            command_eprintln!("Saved {id} ({} models); the API key is read from {name}.", models.len());
        }
        (None, Some(name)) => command_eprintln!(
            "Saved {id} ({} models). Set {name} in the launching shell before selecting it.",
            models.len()
        ),
        (None, None) => unreachable!("the wizard always has a credential"),
    }
    let key_available = match (&stored_key, &key_env) {
        (Some(_), _) => true,
        (None, Some(name)) => std::env::var_os(name).is_some(),
        (None, None) => false,
    };
    if key_available {
        command_eprintln!("Checking the connection with {}…", models[0]);
        match verify(paths, &id, &models[0]).await {
            Ok(()) => command_eprintln!("Connected with {}.", models[0]),
            Err(error) => {
                let detail = format!("{error:#}");
                command_eprintln!("The connection test failed: {detail}");
                command_eprintln!("{id} is saved; fix it with /provider edit {id}.");
                // A gateway can serve a model list while refusing one model in
                // this region; that is not something the URL or key can fix.
                if detail.to_ascii_lowercase().contains("region") {
                    command_eprintln!(
                        "{} is not available in this region; pick another model with /provider edit {id}.",
                        models[0]
                    );
                }
            }
        }
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
    command_eprintln!(
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

/// A provider offered by the first-run setup.
struct Preset {
    id: &'static str,
    name: &'static str,
    /// Entry of the embedded catalog shared with Desktop; None for DeepSeek,
    /// which uses its OpenAI-compatible endpoint directly.
    builtin: Option<&'static str>,
    models: &'static [&'static str],
    key_url: &'static str,
}

const PRESETS: &[Preset] = &[
    Preset { id: "deepseek", name: "DeepSeek (recommended)", builtin: None, models: &["deepseek-flash", "deepseek-v4-pro"], key_url: "https://platform.deepseek.com/api_keys" },
    Preset { id: "anthropic", name: "Anthropic Claude", builtin: Some("anthropic"), models: &["claude-sonnet-4-5", "claude-opus-4-6", "claude-haiku-4-5"], key_url: "https://console.anthropic.com/settings/keys" },
    Preset { id: "openai", name: "OpenAI", builtin: Some("openai"), models: &["gpt-5.1", "gpt-5-mini", "gpt-4.1"], key_url: "https://platform.openai.com/api-keys" },
    Preset { id: "moonshotai-cn", name: "Moonshot Kimi (China)", builtin: Some("moonshotai-cn"), models: &["kimi-k2.7-code", "kimi-k2-thinking"], key_url: "https://platform.moonshot.cn/console/api-keys" },
    Preset { id: "zai-coding-cn", name: "Zhipu GLM Coding Plan (China)", builtin: Some("zai-coding-cn"), models: &["glm-5.1", "glm-4.7"], key_url: "https://open.bigmodel.cn/usercenter/apikeys" },
    Preset { id: "minimax-cn", name: "MiniMax (China)", builtin: Some("minimax-cn"), models: &["MiniMax-M3", "MiniMax-M2.7"], key_url: "https://platform.minimaxi.com/user-center/basic-information/interface-key" },
    Preset { id: "qwen-token-plan-cn", name: "Alibaba Qwen Token Plan (China)", builtin: Some("qwen-token-plan-cn"), models: &["qwen3.6-flash", "deepseek-v4-flash", "glm-5.1"], key_url: "https://bailian.console.aliyun.com/" },
    Preset { id: "opencode-go", name: "OpenCode Go", builtin: Some("opencode-go"), models: &["deepseek-v4.1-flash", "deepseek-v4-pro", "glm-5.3", "kimi-k3"], key_url: "https://opencode.ai/auth" },
    Preset { id: "openrouter", name: "OpenRouter", builtin: Some("openrouter"), models: &["anthropic/claude-sonnet-4.5", "anthropic/claude-opus-4.6"], key_url: "https://openrouter.ai/keys" },
];

fn preset_by_id(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

fn preset_key_env(preset: &Preset) -> String {
    match preset.builtin {
        Some(id) => deepseek_code_core::native_engine::provider_runtime::credential_env(id),
        None => "DEEPSEEK_API_KEY".into(),
    }
}

/// The saved provider for a preset, with models taken from the catalog.
fn preset_provider(preset: &Preset, api_key: Option<String>) -> Result<deepseek_code_core::config::ProviderConfig> {
    let mut provider = config::provider_template(preset.models[0]);
    provider.id = preset.id.into();
    provider.name = preset.name.trim_end_matches(" (recommended)").into();
    // A pasted key is stored; otherwise the key is read from the environment.
    if api_key.is_none() {
        provider.settings.insert("apiKeyEnv".into(), json!(preset_key_env(preset)));
    }
    provider.api_key = api_key;
    provider.enabled = true;
    provider.is_default = true;
    match preset.builtin {
        None => {
            provider.provider_type = "openai".into();
            provider.base_url = "https://api.deepseek.com".into();
            provider.models = preset
                .models
                .iter()
                .map(|id| {
                    let mut model = config::provider_template(id).models.remove(0);
                    model.id = (*id).into();
                    model.name = (*id).into();
                    model
                })
                .collect();
        }
        Some(builtin) => {
            let entry = deepseek_code_core::native_engine::provider_runtime::catalog_entry(builtin)
                .with_context(|| format!("The built-in catalog has no {builtin} entry"))?;
            provider.provider_type = "openai".into();
            provider.base_url = entry["baseUrl"].as_str().unwrap_or_default().into();
            provider.settings.insert("builtinId".into(), json!(builtin));
            let catalog = entry["models"].as_array().cloned().unwrap_or_default();
            provider.models = preset
                .models
                .iter()
                .filter_map(|id| catalog.iter().find(|model| model["id"] == *id))
                .map(|model| {
                    let id = model["id"].as_str().unwrap_or_default();
                    let mut config = config::provider_template(id).models.remove(0);
                    config.id = id.into();
                    config.name = model["name"].as_str().unwrap_or(id).into();
                    config.api = model["api"].as_str().map(String::from);
                    config.context_window = model["contextWindow"].as_u64().map(|value| value as u32);
                    config.max_tokens = model["maxTokens"].as_u64().map(|value| (value as u32).min(32_000));
                    config.supports_vision = true;
                    config
                })
                .collect();
            ensure!(!provider.models.is_empty(), "No preset model is in the built-in catalog");
        }
    }
    Ok(provider)
}

/// Sends a tiny request with the saved provider so a bad key shows up now.
async fn verify(paths: &RuntimePaths, provider: &str, model: &str) -> Result<()> {
    let selection = select(paths, provider, model)?;
    let manager = deepseek_code_core::native_engine::provider_manager::ProviderManager::in_memory(
        selection.providers,
        paths.data_dir.join("providers.json"),
    );
    let resolved = manager
        .resolve_selected(&selection.provider_id, &selection.model)
        .context("The saved provider could not be resolved")?;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        deepseek_code_core::native_engine::generation::generate(resolved, "Reply OK.", "This is a connection test. Reply only OK."),
    )
    .await
    .context("The connection test timed out")??;
    Ok(())
}

async fn configure_preset(paths: &RuntimePaths, input: &mut TerminalInput, preset: &Preset) -> Result<Option<config::Selection>> {
    let key_env = preset_key_env(preset);
    loop {
        let api_key = if std::env::var_os(&key_env).is_some_and(|value| !value.is_empty()) {
            command_eprintln!("Using the API key from {key_env}.");
            None
        } else {
            command_eprintln!("Get an API key at {}", preset.key_url);
            let prompt = format!("Paste the API key (hidden), or press Enter to set {key_env} yourself: ");
            let key = input.read_secret(&prompt).await?.context("Input closed")?.trim().to_owned();
            if key.is_empty() {
                command_eprintln!("Set it in your shell, then start dcl again:");
                command_eprintln!("  bash/zsh:   export {key_env}=<your key>");
                command_eprintln!("  PowerShell: setx {key_env} \"<your key>\"   (then open a new terminal)");
                return Ok(None);
            }
            anyhow::ensure!(!key.contains(char::is_whitespace), "An API key has no spaces; paste it again");
            Some(key)
        };
        let provider = preset_provider(preset, api_key.clone())?;
        let model = provider.models[0].id.clone();
        let mut manager = ConfigManager::new(paths.data_dir.clone());
        manager.update_config(|c| {
            c.providers.retain(|p| p.id != provider.id);
            for other in &mut c.providers {
                other.is_default = false;
            }
            c.default_provider_id = Some(provider.id.clone());
            c.providers.push(provider);
        })?;
        command_eprintln!("Checking the connection with {model}…");
        match verify(paths, preset.id, &model).await {
            Ok(()) => {
                command_eprintln!(
                    "Connected. {} is saved as the default provider{}.",
                    preset.name.trim_end_matches(" (recommended)"),
                    if api_key.is_some() { " (key stored in config.json, readable only by you)" } else { "" }
                );
                let model = choose_default_model(input, preset).await?;
                crate::config::save_default_model(paths, preset.id, &model)?;
                return Ok(Some(select(paths, preset.id, &model)?));
            }
            Err(error) => {
                command_eprintln!("The connection test failed: {error:#}");
                if api_key.is_none() {
                    command_eprintln!("Check the value of {key_env}, or unset it to paste a key here.");
                    return Ok(None);
                }
                let again = ask(input, "Paste a different key? [Y/n]").await?;
                if again.eq_ignore_ascii_case("n") {
                    return Ok(Some(select(paths, preset.id, &model)?));
                }
            }
        }
    }
}

/// After the key works: which of the provider's models runs by default.
async fn choose_default_model(input: &mut TerminalInput, preset: &Preset) -> Result<String> {
    if preset.models.len() < 2 {
        return Ok(preset.models[0].to_owned());
    }
    if crate::terminal::picker::available() {
        use crate::terminal::picker::{Entry, Picker};
        let entries: Vec<Entry> = preset.models.iter().map(|model| Entry { title: (*model).into(), detail: String::new() }).collect();
        let picker = Picker { heading: "Choose the default model", subtitle: Some("Change it later with /model"), entries: &entries, search: false, hint: "Enter to confirm \u{b7} Esc for the first", selected: 0 };
        return Ok(preset.models[picker.run()?.unwrap_or(0)].to_owned());
    }
    command_eprintln!("\nChoose the default model:");
    for (index, model) in preset.models.iter().enumerate() {
        command_eprintln!("  {}. {model}", index + 1);
    }
    let choice = ask(input, "Number [1]").await?;
    let index = choice.parse::<usize>().ok().filter(|n| (1..=preset.models.len()).contains(n)).unwrap_or(1);
    Ok(preset.models[index - 1].to_owned())
}

/// First-run setup: pick a built-in provider (DeepSeek by default), a saved
/// one, or a custom endpoint; the key is checked before the session starts.
pub async fn setup(paths: &RuntimePaths, input: &mut TerminalInput) -> Result<config::Selection> {
    let saved: Vec<_> = paths.read_config()?.providers.into_iter().filter(|p| p.enabled).collect();
    if saved.is_empty() {
        command_eprintln!("Welcome to DeepSeek Code! Connect a model provider to get started.");
    }
    loop {
        command_eprintln!("\nChoose a provider:");
        for (index, preset) in PRESETS.iter().enumerate() {
            command_eprintln!("  {:>2}. {}", index + 1, preset.name);
        }
        let custom = PRESETS.len() + 1;
        command_eprintln!("  {custom:>2}. Another OpenAI- or Anthropic-compatible endpoint");
        for provider in &saved {
            let models: Vec<_> = provider.models.iter().filter(|m| m.enabled).map(|m| m.id.as_str()).collect();
            command_eprintln!("      saved: {} ({})", provider.id, models.join(", "));
        }
        let choice = ask(input, "Number or saved provider ID [1]").await?;
        let choice = if choice.is_empty() { "1".to_owned() } else { choice };
        let result: Result<Option<config::Selection>> = async {
            match choice.parse::<usize>() {
                Ok(number) if (1..=PRESETS.len()).contains(&number) => configure_preset(paths, input, &PRESETS[number - 1]).await,
                Ok(number) if number == custom => {
                    let id = wizard(paths, input, None).await?;
                    let config = paths.read_config()?;
                    let model = config
                        .providers
                        .iter()
                        .find(|provider| provider.id == id)
                        .and_then(|provider| provider.models.first())
                        .map(|model| model.id.clone())
                        .context("The saved provider has no models")?;
                    Ok(Some(select(paths, &id, &model)?))
                }
                Ok(_) => anyhow::bail!("Choose a number from the list"),
                Err(_) => {
                    let provider = saved.iter().find(|provider| provider.id == choice).context("No saved provider has that ID")?;
                    let model = provider.models.iter().find(|m| m.enabled).map(|m| m.id.clone()).context("That provider has no enabled models")?;
                    Ok(Some(select(paths, &choice, &model)?))
                }
            }
        }
        .await;
        match result {
            Ok(Some(selection)) => return Ok(selection),
            Ok(None) => {}
            Err(error) => command_eprintln!("Error: {error:#}"),
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
    // Status uses the same scrollable report as a running turn. Config and
    // usage retain their settings tabs; arguments keep the text forms.
    if words.clone().next().is_none() && crate::terminal::picker::available() {
        if command == "/status" {
            let text = crate::status_commands::text(session, "").await?;
            crate::terminal::live::show_inspection("Status", &text)?;
            return Ok(true);
        }
        let tab = match command {
            "/config" => Some("Config"),
            "/cost" | "/usage" => Some("Usage"),
            _ => None,
        };
        if let Some(tab) = tab {
            crate::settings_dialog::run(tab, paths, session, input).await?;
            return Ok(true);
        }
    }
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
        command_eprintln!(
            "Testing a real text request (no tools or conversation history); Ctrl+C cancels."
        );
        let started = std::time::Instant::now();
        let response = tokio::select! {
            result = session.engine.generate_text(&session.provider_id, &session.model, "This is a connection test. Reply only OK.", "Reply OK.") => result?,
            _ = tokio::signal::ctrl_c() => bail!("Test cancelled"),
        };
        command_eprintln!(
            "Connection test passed in {:.2}s. Response: {}",
            started.elapsed().as_secs_f64(),
            response.chars().take(200).collect::<String>()
        );
        return Ok(true);
    }
    let argument = words.next();
    if command == "/provider" && matches!(argument, Some("add" | "edit")) {
        if argument == Some("add") {
            if let Some(target) = words.next() {
                let preset = preset_by_id(target).with_context(|| {
                    format!(
                        "Unknown preset {target}; run /provider add without an ID for the wizard"
                    )
                })?;
                ensure!(words.next().is_none(), "Unexpected argument");
                if let Some(selection) = configure_preset(paths, input, preset).await? {
                    session.switch(paths, selection).await?;
                    config::save_default_model(paths, &session.provider_id, &session.model)?;
                    status(session);
                }
                return Ok(true);
            }
        }
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
            config::save_default_model(paths, &session.provider_id, &session.model)?;
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
            command_eprintln!("{}. {} — {} ({})", i + 1, p.id, p.name, p.provider_type);
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
        if argument.is_none() && crate::terminal::picker::available() && !models.is_empty() {
            // As in the reference CLI: a model picker with the effort below it.
            use crate::terminal::picker::{Entry, Key, Picked, Picker};
            let efforts = crate::session_commands::EFFORTS;
            let current_effort = crate::session_commands::session_effort(session)?;
            let mut effort = efforts.iter().position(|level| Some(*level) == current_effort.as_deref()).unwrap_or(0);
            let mut selected = models.iter().position(|m| m.id == session.model).unwrap_or(0);
            loop {
                let entries: Vec<Entry> = models
                    .iter()
                    .map(|m| Entry {
                        title: if m.id == session.model { format!("{} \u{2714}", m.id) } else { m.id.clone() },
                        detail: if m.name.is_empty() || m.name == m.id { String::new() } else { m.name.clone() },
                    })
                    .collect();
                let level = efforts[effort];
                let level_label = if level == "default" { "Default effort (model default)".to_owned() } else { format!("{}{} effort", level[..1].to_uppercase(), &level[1..]) };
                let subtitle = format!("Switch between {} models. \u{25cf} {level_label} \u{2190}/\u{2192} to adjust", provider.name);
                let picker = Picker { heading: "Select model", subtitle: Some(&subtitle), entries: &entries, search: false, hint: "Enter to switch \u{b7} Esc to cancel", selected };
                match picker.run_keys(&[Key::Left, Key::Right])? {
                    Picked::Key(Key::Left, index) => {
                        selected = index.unwrap_or(selected);
                        effort = (effort + efforts.len() - 1) % efforts.len();
                    }
                    Picked::Key(Key::Right, index) => {
                        selected = index.unwrap_or(selected);
                        effort = (effort + 1) % efforts.len();
                    }
                    Picked::Entry(index) => {
                        let chosen = (efforts[effort] != "default").then(|| efforts[effort].to_owned());
                        if chosen != current_effort {
                            crate::session_commands::set_session_effort(session, chosen.clone())?;
                            command_eprintln!("Session effort: {}", chosen.as_deref().unwrap_or("default"));
                        }
                        if models[index].id != session.model {
                            let selection = select(paths, &provider.id, &models[index].id)?;
                            session.switch(paths, selection).await?;
                            config::save_default_model(paths, &session.provider_id, &session.model)?;
                            status(session);
                            command_eprintln!("Conversation history preserved. Use /test to verify connectivity.");
                        }
                        return Ok(true);
                    }
                    _ => return Ok(true),
                }
            }
        }
        for (i, m) in models.iter().enumerate() {
            command_eprintln!("{}. {}", i + 1, m.id);
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
    config::save_default_model(paths, &session.provider_id, &session.model)?;
    status(session);
    command_eprintln!("Conversation history preserved. Use /test to verify connectivity.");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_use_catalog_models_and_their_key_variables() {
        for preset in PRESETS {
            let provider = preset_provider(preset, None).unwrap();
            assert!(!provider.models.is_empty(), "{}", preset.id);
            // OpenCode Go routes each model to its own base URL through the catalog.
            if preset.id != "opencode-go" {
                assert!(provider.base_url.starts_with("https://"), "{}", preset.id);
            }
        }
        let deepseek = preset_provider(&PRESETS[0], Some("sk-test".into())).unwrap();
        assert_eq!(deepseek.base_url, "https://api.deepseek.com");
        assert_eq!(deepseek.models[0].id, "deepseek-flash");
        assert_eq!(deepseek.api_key.as_deref(), Some("sk-test"));
        assert_eq!(preset_key_env(&PRESETS[3]), "MOONSHOT_API_KEY");
        let anthropic = preset_provider(&PRESETS[1], None).unwrap();
        assert_eq!(anthropic.settings["builtinId"], "anthropic");
        assert_eq!(anthropic.models[0].api.as_deref(), Some("anthropic-messages"));
    }

    #[test]
    fn opencode_go_preset_uses_the_catalog_and_open_code_key() {
        let preset = preset_by_id("opencode-go").expect("OpenCode Go preset");
        let provider = preset_provider(preset, None).unwrap();
        assert_eq!(provider.base_url, "");
        assert_eq!(provider.settings["builtinId"], "opencode-go");
        assert_eq!(provider.settings["apiKeyEnv"], "OPENCODE_API_KEY");
        assert_eq!(preset_key_env(preset), "OPENCODE_API_KEY");
        assert!(provider
            .models
            .iter()
            .all(|model| model.api.as_deref().is_some_and(|api| !api.is_empty())));
        assert!(provider
            .models
            .iter()
            .any(|model| model.api.as_deref() == Some("openai-completions")));
        assert_eq!(
            provider.models.iter().find(|model| model.id == "deepseek-v4.1-flash").unwrap().api.as_deref(),
            Some("openai-completions")
        );
    }

    #[test]
    fn opencode_base_urls_are_corrected_for_each_protocol() {
        assert_eq!(
            opencode_base_url("openai", "https://opencode.ai/v1").as_deref(),
            Some("https://opencode.ai/inference/go/openai/v1")
        );
        assert_eq!(
            opencode_base_url("responses", "https://opencode.ai/v1").as_deref(),
            Some("https://opencode.ai/inference/go/openai/v1")
        );
        assert_eq!(
            opencode_base_url("anthropic", "https://opencode.ai/v1").as_deref(),
            Some("https://opencode.ai/inference/go/anthropic/v1")
        );
        assert_eq!(
            opencode_base_url("gemini", "https://opencode.ai/v1").as_deref(),
            Some("https://opencode.ai/inference/go/google/v1beta")
        );
        assert_eq!(
            opencode_base_url("openai", "https://opencode.ai/inference/go/openai/v1"),
            None
        );
        assert_eq!(
            opencode_base_url("openai", "https://opencode.ai/zen/go/v1"),
            None
        );
        assert_eq!(opencode_base_url("openai", "https://example.com/v1"), None);
    }

    #[test]
    fn numbered_protocol_and_model_choices_are_parsed() {
        assert_eq!(parse_protocol("").unwrap(), "openai");
        assert_eq!(parse_protocol("1").unwrap(), "openai");
        assert_eq!(parse_protocol("2").unwrap(), "anthropic");
        assert_eq!(parse_protocol("Claude").unwrap(), "anthropic");
        assert_eq!(parse_protocol("3").unwrap(), "gemini");
        assert_eq!(parse_protocol("4").unwrap(), "responses");
        assert_eq!(parse_protocol("OpenAI-Responses").unwrap(), "responses");
        assert!(parse_protocol("5").is_err());
        let listed = vec!["model-a".to_owned(), "model-b".to_owned(), "model-c".to_owned()];
        assert_eq!(parse_model_selection("", &listed).unwrap(), ["model-a"]);
        assert_eq!(parse_model_selection("2,3", &listed).unwrap(), ["model-b", "model-c"]);
        assert_eq!(parse_model_selection("2,2", &listed).unwrap(), ["model-b"]);
        assert_eq!(parse_model_selection("all", &listed).unwrap(), listed);
        assert_eq!(parse_model_selection("typed-model", &listed).unwrap(), ["typed-model"]);
        assert!(parse_model_selection("9", &listed).unwrap_err().contains("1 to 3"));
        assert!(parse_model_selection("two words", &listed).unwrap_err().contains("cannot contain spaces"));
    }

    #[test]
    fn provider_answers_accept_keys_or_variable_names_and_fix_deepseek_urls() {
        for key in ["sk-170b23aa867d47839ee0a01eddf73ff5", "123456"] {
            match parse_credential(key) {
                Ok(Credential::Key(value)) => assert_eq!(value, key),
                other => panic!("{key} was not read as a key: {other:?}"),
            }
        }
        match parse_credential("DEEPSEEK_API_KEY") {
            Ok(Credential::Env(name)) => assert_eq!(name, "DEEPSEEK_API_KEY"),
            other => panic!("an environment name was not recognised: {other:?}"),
        }
        assert!(matches!(parse_credential("env:DEEPSEEK_API_KEY"), Ok(Credential::Env(_))));
        assert!(matches!(parse_credential("key:DEEPSEEK_API_KEY"), Ok(Credential::Key(_))));
        assert!(parse_credential("two words").unwrap_err().contains("no spaces"));
        assert!(parse_credential("env:1KEY").unwrap_err().contains("letters, digits"));
        assert!(parse_credential("").unwrap_err().contains("Enter an API key"));
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
    async fn wizard_preserves_other_providers_and_stores_environment_references() {
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
    async fn wizard_saves_responses_transport_and_edit_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().to_path_buf()).unwrap();
        wizard(&paths, &mut answers(&["responses-test", "4", "http://localhost:8317/v1", "UNSET_RESPONSES_TEST_KEY", "custom-model"]), None).await.unwrap();
        let saved = paths.read_config().unwrap();
        let provider = saved.providers.iter().find(|p| p.id == "responses-test").unwrap();
        assert_eq!(provider.provider_type, "openai");
        assert_eq!(provider.settings["builtinId"], "openai");
        assert_eq!(provider.models[0].api.as_deref(), Some("openai-responses"));
        let runtime = deepseek_code_core::runtime::provider_from_config(provider);
        assert_eq!(runtime.builtin_id.as_deref(), Some("openai"));
        assert_eq!(runtime.models[0].api.as_deref(), Some("openai-responses"));
        wizard(&paths, &mut answers(&["1", "http://localhost:8317/v1", "UNSET_RESPONSES_TEST_KEY", "custom-model"]), Some("responses-test")).await.unwrap();
        let saved = paths.read_config().unwrap();
        let provider = saved.providers.iter().find(|p| p.id == "responses-test").unwrap();
        assert!(!provider.settings.contains_key("builtinId"));
        assert!(provider.models[0].api.is_none());
    }

    #[tokio::test]
    async fn wizard_lists_the_endpoint_models_and_accepts_numbers() {
        use axum::{routing::get, Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/v1/models",
            get(|headers: axum::http::HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer 123456");
                Json(json!({"data": [{"id": "model-a"}, {"id": "model-b"}]}))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().to_path_buf()).unwrap();
        let url = format!("{base}/v1");
        wizard(
            &paths,
            &mut answers(&["listed", "1", &url, "123456", "2"]),
            None,
        )
        .await
        .unwrap();
        let config = paths.read_config().unwrap();
        let provider = config.providers.iter().find(|p| p.id == "listed").unwrap();
        assert_eq!(provider.models.len(), 1);
        assert_eq!(provider.models[0].id, "model-b");
    }

    #[tokio::test]
    async fn wizard_accepts_a_pasted_key_and_can_switch_back_to_a_variable() {
        use axum::{routing::{get, post}, Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/v1/models",
                get(|| async { Json(json!({"data": [{"id": "model-a"}]})) }),
            )
            .route(
                "/v1/chat/completions",
                post(|| async {
                    Json(json!({"id":"test","object":"chat.completion","created":0,"model":"model-a","choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}))
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().to_path_buf()).unwrap();
        wizard(
            &paths,
            &mut answers(&["local", "openai", &url, "123456", "model-a"]),
            None,
        )
        .await
        .unwrap();
        let config = paths.read_config().unwrap();
        let provider = config.providers.iter().find(|p| p.id == "local").unwrap();
        assert_eq!(provider.api_key.as_deref(), Some("123456"));
        assert!(!provider.settings.contains_key("apiKeyEnv"));
        wizard(
            &paths,
            &mut answers(&["openai", &url, "DEEPSEEK_API_KEY", "model-a"]),
            Some("local"),
        )
        .await
        .unwrap();
        let config = paths.read_config().unwrap();
        let provider = config.providers.iter().find(|p| p.id == "local").unwrap();
        assert!(provider.api_key.is_none());
        assert_eq!(provider.settings["apiKeyEnv"], "DEEPSEEK_API_KEY");
        wizard(
            &paths,
            &mut answers(&["openai", &url, "key:ABC123", "model-a"]),
            Some("local"),
        )
        .await
        .unwrap();
        let config = paths.read_config().unwrap();
        let provider = config.providers.iter().find(|p| p.id == "local").unwrap();
        assert_eq!(provider.api_key.as_deref(), Some("ABC123"));
        assert!(!provider.settings.contains_key("apiKeyEnv"));
        select(&paths, "local", "model-a").unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn wizard_keeps_a_provider_saved_when_the_connection_test_fails() {
        use axum::{routing::{get, post}, Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/v1/models",
                get(|| async { Json(json!({"data": [{"id": "model-a"}]})) }),
            )
            .route(
                "/v1/chat/completions",
                post(|| async {
                    (
                        axum::http::StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "invalid key"})),
                    )
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().to_path_buf()).unwrap();
        let id = wizard(
            &paths,
            &mut answers(&["broken", "openai", &url, "123456", "model-a"]),
            None,
        )
        .await
        .unwrap();
        assert_eq!(id, "broken");
        let provider = paths
            .read_config()
            .unwrap()
            .providers
            .into_iter()
            .find(|provider| provider.id == "broken")
            .expect("the provider stays saved after a failed check");
        assert_eq!(provider.api_key.as_deref(), Some("123456"));
        assert_eq!(provider.models[0].id, "model-a");
        server.abort();
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
            command_eprintln!("Settings | {}\nProvider: {} | Model: {}\nPermissions: {} | Theme: {} | Color: {} | Focus: {} | Renderer: {}\nprovider | model | permissions | theme | color | focus | output-style | statusline | tui | effort | autocompact\nChoose a setting, or enter done to return.", paths.data_dir.display(), session.provider_id, session.model, session.permission_mode(), display.theme, display.color, display.focus, display.tui);
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
                command_eprintln!("Permission mode: {}", session.permission_mode());
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
