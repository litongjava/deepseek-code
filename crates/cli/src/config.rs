use crate::{
    args::{ApiFormatArg, Cli, ConfigCommands},
    legacy_api_config::ApiConfig,
};
use anyhow::{bail, ensure, Context, Result};
use deepseek_code_core::{
    config::{ConfigManager, ModelConfig, ProviderConfig},
    native_engine::provider_manager::{ApiFormat, Provider},
    runtime::{provider_from_config, RuntimePaths},
};
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path};

pub struct Selection {
    pub providers: Vec<Provider>,
    pub model: String,
    pub provider_id: String,
}

fn preferences(paths: &RuntimePaths) -> Result<Value> {
    let path = paths.data_dir.join("cli.json");
    if !path.exists() {
        return Ok(json!({}));
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// The model new sessions start with, until `--model` or `/model` says otherwise.
pub fn save_default_model(paths: &RuntimePaths, provider: &str, model: &str) -> Result<()> {
    let mut prefs = preferences(paths)?;
    prefs["model"] = json!(format!("{provider}::{model}"));
    save_preferences(paths, &prefs)
}

fn save_preferences(paths: &RuntimePaths, value: &Value) -> Result<()> {
    std::fs::create_dir_all(&paths.data_dir)?;
    std::fs::write(
        paths.data_dir.join("cli.json"),
        serde_json::to_vec_pretty(value)?,
    )?;
    Ok(())
}

/// Default output budget per model. Claude models follow the reference CLI's
/// table; DeepSeek counts thinking against the budget and accepts far more
/// than 8K. Other OpenAI-compatible endpoints often reject larger values.
pub(crate) fn default_max_tokens(model: &str) -> u32 {
    let model = model.to_ascii_lowercase();
    if model.starts_with("deepseek") {
        return 32_000;
    }
    if !model.contains("claude") {
        return 8192;
    }
    if model.contains("opus-4-6") {
        64_000
    } else if model.contains("claude-3-opus") || model.contains("claude-3-haiku") {
        4096
    } else if model.contains("claude-3-sonnet") || model.contains("3-5-sonnet") || model.contains("3-5-haiku") {
        8192
    } else {
        32_000
    }
}

/// Context windows reported by the providers' model APIs, when known.
pub(crate) fn default_context_window(model: &str) -> Option<u32> {
    let model = model.to_ascii_lowercase();
    if model.starts_with("deepseek-flash") || model.starts_with("deepseek-v4") {
        Some(1_048_576)
    } else if model.contains("claude") {
        Some(200_000)
    } else {
        None
    }
}

/// Models that accept image input (DeepSeek-V4.1-Flash and Claude).
pub(crate) fn default_supports_vision(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("deepseek-flash") || model.contains("claude")
}

pub(crate) fn provider_template(model: &str) -> ProviderConfig {
    let model = ApiConfig::resolve_model_alias(model);
    let metadata = ApiConfig::get_provider_metadata(&model);
    let anthropic = model.starts_with("claude");
    let max_tokens = default_max_tokens(&model);
    let supports_vision = default_supports_vision(&model);
    let context_window = default_context_window(&model);
    ProviderConfig {
        id: "cli".into(),
        name: "CLI".into(),
        provider_type: if anthropic { "anthropic" } else { "openai" }.into(),
        api_key: None,
        base_url: metadata
            .map(|m| m.default_base_url)
            .unwrap_or(if model.starts_with("deepseek") {
                "https://api.deepseek.com"
            } else {
                "https://api.openai.com/v1"
            })
            .into(),
        models: vec![ModelConfig {
            id: model.clone(),
            name: model,
            enabled: true,
            max_tokens: Some(max_tokens),
            supports_vision,
            supports_tools: true,
            supports_streaming: true,
            context_window,
            supports_text: true,
            api: None,
            cost_per_1k_input: None,
            cost_per_1k_output: None,
        }],
        enabled: true,
        is_default: false,
        settings: HashMap::new(),
        supports_web_search: false,
        web_search_strategy: None,
        web_search_tested_at: None,
        web_search_test_reason: None,
    }
}

pub fn select(
    cli: &Cli,
    paths: &RuntimePaths,
    resumed_model: Option<&str>,
    resumed_provider: Option<&str>,
) -> Result<Selection> {
    let prefs = preferences(paths)?;
    let config = paths.read_config()?;
    let mut providers = paths.providers()?;
    let default_provider = config.default_provider_id.as_deref().or_else(|| {
        config
            .providers
            .iter()
            .find(|p| p.is_default)
            .map(|p| p.id.as_str())
    });
    let selected_provider = cli
        .provider
        .as_deref()
        .or(resumed_provider)
        .or(default_provider);
    let first_model = providers
        .iter()
        .filter(|p| p.enabled && selected_provider.map_or(true, |id| p.id == id))
        .flat_map(|p| &p.models)
        .find(|m| m.enabled)
        .map(|m| m.id.as_str());
    let model = cli
        .model
        .as_deref()
        .or(resumed_model)
        .or(prefs["model"].as_str())
        .or(first_model)
        .unwrap_or(if std::env::var_os("DEEPSEEK_API_KEY").is_some() {
            "deepseek-flash"
        } else {
            "sonnet"
        });
    let (qualified_provider, raw_model) = model
        .split_once("::")
        .map(|(p, m)| (Some(p), m))
        .unwrap_or((None, model));
    if let (Some(explicit), Some(qualified)) = (cli.provider.as_deref(), qualified_provider) {
        ensure!(
            explicit == qualified,
            "--provider conflicts with qualified model ID"
        );
    }
    let selected_provider = qualified_provider.or(selected_provider);
    let model = if providers
        .iter()
        .any(|p| p.enabled && selected_provider.is_none_or(|id|id==p.id) && p.models.iter().any(|m| m.enabled && m.id == raw_model))
    {
        raw_model.to_owned()
    } else {
        if let Some(base)=deepseek_code_core::native_engine::provider_manager::one_million_context_base(raw_model){
            let known=providers.iter().any(|p|p.enabled&&selected_provider.is_none_or(|id|id==p.id)&&p.models.iter().any(|m|m.enabled&&m.id==base));
            format!("{}[1m]",if known {base.to_owned()}else{ApiConfig::resolve_model_alias(base)})
        }else{ApiConfig::resolve_model_alias(raw_model)}
    };
    let exact=providers.iter().any(|p|p.enabled&&selected_provider.is_none_or(|id|id==p.id)&&p.models.iter().any(|m|m.enabled&&m.id==model));
    let matching: Vec<usize> = providers
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            p.enabled
                && selected_provider.map_or(true, |id| p.id == id)
                && if exact {p.models.iter().any(|m|m.enabled&&m.id==model)}else{deepseek_code_core::native_engine::provider_manager::resolve_model_selection(&p.models,&model).is_some()}
        })
        .map(|(i, _)| i)
        .collect();
    ensure!(
        matching.len() <= 1,
        "Model exists in several providers; select --provider or provider::model"
    );
    let metadata = ApiConfig::get_provider_metadata(&model);
    let builtin_key_env = matching
        .first()
        .and_then(|index| providers[*index].builtin_id.as_deref())
        .map(deepseek_code_core::native_engine::provider_runtime::credential_env);
    let configured_key_env = matching
        .first()
        .and_then(|index| {
            config
                .providers
                .iter()
                .find(|p| p.id == providers[*index].id)
        })
        .and_then(|p| p.settings.get("apiKeyEnv"))
        .and_then(Value::as_str);
    let key_env = cli
        .api_key_env
        .as_deref()
        .or(configured_key_env)
        .or(builtin_key_env.as_deref())
        .unwrap_or_else(|| {
            if model.starts_with("deepseek") {
                "DEEPSEEK_API_KEY"
            } else {
                metadata.map(|m| m.auth_env).unwrap_or("OPENAI_API_KEY")
            }
        });
    let key = std::env::var(key_env).ok().filter(|s| !s.is_empty());
    if cli.api_key_env.is_some() || configured_key_env.is_some() {
        ensure!(
            key.is_some(),
            "API key environment variable {key_env} is empty or unset"
        );
    }
    let env_url = if model.starts_with("deepseek") {
        std::env::var("DEEPSEEK_BASE_URL").ok()
    } else {
        metadata.and_then(|m| std::env::var(m.base_url_env).ok())
    };
    let base_url = cli.base_url.clone().or(env_url);
    let index = if let Some(index) = matching.first() {
        *index
    } else {
        ensure!(
            selected_provider.is_none() || selected_provider == Some("cli-runtime"),
            "Selected provider does not contain enabled model {model}"
        );
        let base=deepseek_code_core::native_engine::provider_manager::one_million_context_base(&model).unwrap_or(&model);
        let mut p = provider_from_config(&provider_template(base));
        p.id = "cli-runtime".into();
        providers.push(p);
        providers.len() - 1
    };
    let p = &mut providers[index];
    if let Some(key) = key {
        p.api_key = key;
    }
    if let Some(url) = base_url {
        p.base_url = url;
        p.builtin_id = None;
    }
    if let Some(format) = cli.api_format {
        p.api_format = match format {
            ApiFormatArg::Anthropic => ApiFormat::Anthropic,
            ApiFormatArg::Openai => ApiFormat::OpenAI,
        };
        p.builtin_id = None;
    }
    ensure!(!p.api_key.trim().is_empty() || matches!(p.builtin_id.as_deref(),Some("amazon-bedrock"|"google-vertex")), "No API key configured; set {key_env}, use --api-key-env, or configure a provider in Desktop");
    if let Some(limit) = cli.max_tokens {
        p.models
            .iter_mut()
            .filter(|m| m.id == model || (!exact && Some(m.id.as_str())==deepseek_code_core::native_engine::provider_manager::one_million_context_base(&model)))
            .for_each(|m| m.max_tokens = Some(limit));
    }
    let provider_id = p.id.clone();
    Ok(Selection {
        providers,
        model,
        provider_id,
    })
}

pub fn redact(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                let key = key.to_ascii_lowercase();
                if key.contains("key")
                    || key.ends_with("_token")
                    || key == "token"
                    || key.contains("secret")
                    || key.contains("password")
                    || key == "env"
                {
                    if !value.is_null() {
                        *value = json!("[redacted]");
                    }
                } else {
                    redact(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact),
        _ => {}
    }
}

pub fn run(paths: &RuntimePaths, action: &ConfigCommands) -> Result<()> {
    paths.read_config()?; // Never overwrite a malformed Desktop config with defaults.
    match action {
        ConfigCommands::Show => {
            let mut value = json!({"data_dir":paths.data_dir,"preferences":preferences(paths)?,"config":paths.read_config()?,"permissions.auto":deepseek_code_core::permissions::auto_config::load(&crate::services::context(paths)?)?});
            redact(&mut value);
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        ConfigCommands::Set { key, value } => {
            if key == deepseek_code_core::permissions::auto_config::KEY {
                let config: deepseek_code_core::permissions::auto_config::AutoConfig = serde_json::from_str(value)?;
                deepseek_code_core::permissions::auto_config::save(&crate::services::context(paths)?, &config)?;
                return crate::services::print(&config);
            }
            if let Some(field) = key.strip_prefix("search.") {
                // The independent DeepSeek search service, as in Desktop's web search settings.
                let mut manager = ConfigManager::new(paths.data_dir.clone());
                let mut search = manager.get_config().deepseek_search.clone();
                match field {
                    "api_key" => search.api_key = value.trim().into(),
                    "base_url" => search.base_url = value.trim().into(),
                    "model" => search.model = value.trim().into(),
                    _ => bail!("Unsupported setting: {key}"),
                }
                search.validate()?;
                manager.update_config(|c| c.deepseek_search = search)?;
                eprintln!("Updated {key}");
                return Ok(());
            }
            ensure!(
                ["model", "base_url", "api_format", "api_key", "max_tokens"]
                    .contains(&key.as_str()),
                "Unsupported setting: {key}"
            );
            let mut prefs = preferences(paths)?;
            let mut manager = ConfigManager::new(paths.data_dir.clone());
            let model = if key == "model" {
                value.as_str()
            } else {
                prefs["model"].as_str().unwrap_or("sonnet")
            };
            let mut p = manager
                .get_provider("cli")
                .cloned()
                .unwrap_or_else(|| provider_template(model));
            match key.as_str() {
                "model" => {
                    let model = ApiConfig::resolve_model_alias(value);
                    if !p.models.iter().any(|m| m.id == model) {
                        p.models.push(provider_template(&model).models.remove(0));
                    }
                    prefs["model"] = json!(format!("cli::{model}"));
                }
                "base_url" => {
                    ensure!(
                        value.starts_with("https://") || value.starts_with("http://"),
                        "API URL must start with http:// or https://"
                    );
                    p.base_url = value.clone();
                }
                "api_format" => {
                    ensure!(
                        ["openai", "anthropic"].contains(&value.as_str()),
                        "Expected openai or anthropic"
                    );
                    p.provider_type = value.clone();
                }
                "api_key" => p.api_key = Some(value.clone()),
                "max_tokens" => {
                    let n: u32 = value
                        .parse()
                        .context("max_tokens must be a positive integer")?;
                    ensure!(n > 0, "max_tokens must be positive");
                    p.models.iter_mut().for_each(|m| m.max_tokens = Some(n));
                }
                _ => unreachable!(),
            }
            manager.update_config(|c| {
                c.providers.retain(|p| p.id != "cli");
                c.providers.push(p);
            })?;
            save_preferences(paths, &prefs)?;
            eprintln!("Updated {key}");
        }
        ConfigCommands::Reset => {
            ConfigManager::new(paths.data_dir.clone()).remove_provider("cli")?;
            save_preferences(paths, &json!({}))?;
            eprintln!("CLI settings reset; other Desktop providers preserved");
        }
        ConfigCommands::ImportLegacy { source } => import_legacy(paths, source)?,
    }
    Ok(())
}

fn import_legacy(paths: &RuntimePaths, source: &Path) -> Result<()> {
    let text = std::fs::read_to_string(source).context("Cannot read legacy settings")?;
    let old: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .context("Invalid legacy settings")?;
    let model = old["model"]
        .as_str()
        .context("Legacy settings must include model")?;
    let mut provider = provider_template(model);
    provider.id = "legacy-cli".into();
    provider.name = "Imported CLI".into();
    if let Some(url) = old["api"]["base_url"].as_str() {
        provider.base_url = url.into();
    }
    provider.api_key = old["api"]["api_key"].as_str().map(String::from);
    if let Some(limit) = old["api"]["max_tokens"].as_u64() {
        let limit = u32::try_from(limit).context("Invalid legacy max_tokens")?;
        ensure!(limit > 0, "Invalid legacy max_tokens");
        provider.models[0].max_tokens = Some(limit);
    }
    // The old REPL used an OpenAI-compatible endpoint for non-Anthropic hosts.
    if !provider.base_url.contains("api.anthropic.com") {
        provider.provider_type = "openai".into();
    }
    let mut manager = ConfigManager::new(paths.data_dir.clone());
    if manager.get_provider("legacy-cli").is_some() {
        bail!("legacy-cli provider already exists; refusing to overwrite it");
    }
    manager.add_provider(provider)?;
    let mut prefs = preferences(paths)?;
    prefs["model"] = json!(format!(
        "legacy-cli::{}",
        ApiConfig::resolve_model_alias(model)
    ));
    save_preferences(paths, &prefs)?;
    eprintln!("Imported legacy provider. Source settings were not modified.");
    Ok(())
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn output_budgets_follow_the_model_family() {
        assert_eq!(default_max_tokens("deepseek-flash"), 32_000);
        assert_eq!(default_max_tokens("claude-opus-4-6"), 64_000);
        assert_eq!(default_max_tokens("claude-sonnet-4-5"), 32_000);
        assert_eq!(default_max_tokens("claude-3-5-haiku-latest"), 8192);
        assert_eq!(default_max_tokens("claude-3-opus-20240229"), 4096);
        assert_eq!(default_max_tokens("gpt-4o"), 8192);
        assert_eq!(default_context_window("deepseek-flash"), Some(1_048_576));
        assert_eq!(default_context_window("gpt-4o"), None);
        assert!(default_supports_vision("deepseek-flash") && !default_supports_vision("deepseek-v4-pro"));
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn search_settings_are_saved_where_web_search_reads_them() {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let set = |key: &str, value: &str| run(&paths, &ConfigCommands::Set { key: key.into(), value: value.into() });
        set("search.api_key", " search-key ").unwrap();
        set("search.model", "deepseek-pro").unwrap();
        assert!(set("search.base_url", "ftp://example.com").is_err());
        assert!(set("search.other", "x").is_err());
        let saved: deepseek_code_core::config::AppConfig =
            serde_json::from_slice(&std::fs::read(paths.data_dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(saved.deepseek_search.api_key, "search-key");
        assert_eq!(saved.deepseek_search.model, "deepseek-pro");
        assert_eq!(saved.deepseek_search.base_url, "https://api.deepseek.com/anthropic/v1");
        let mut shown = json!({"config": saved});
        redact(&mut shown);
        assert!(!shown.to_string().contains("search-key"));
    }
}
