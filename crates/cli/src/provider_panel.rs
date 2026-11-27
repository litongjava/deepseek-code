//! Interactive `/provider` management panel with Provider, Accounts and Models
//! pages, connection add/edit forms, live model fetching with search and
//! multi-select, and model confirmation / connection testing.
use crate::config;
use crate::interactive_config::{self, Credential};
use crate::session::Session;
use crate::terminal::input::editor::{clipped, DisplayLine, Screen, TextStyle};
use crate::terminal::paste_input::Reader;
use anyhow::{bail, ensure, Context, Result};
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal,
};
use deepseek_code_core::{
    config::{ConfigManager, ModelConfig, ProviderConfig},
    native_engine::model_listing::{self, ListedModel},
    runtime::RuntimePaths,
    services::anthropic_auth,
};
use serde_json::{json, Value};
use std::io;
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

/// Restore alternate screen, bracketed paste and raw mode on every exit.
struct PanelScreen {
    was_raw: bool,
}

impl PanelScreen {
    fn enter() -> io::Result<Self> {
        crate::terminal::release_handoff();
        let was_raw = terminal::is_raw_mode_enabled()?;
        if !was_raw {
            terminal::enable_raw_mode()?;
        }
        crossterm::execute!(
            crate::terminal_output::stderr(),
            terminal::EnterAlternateScreen,
            terminal::Clear(terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0),
            EnableBracketedPaste
        )?;
        Ok(Self { was_raw })
    }
}

impl Drop for PanelScreen {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            crate::terminal_output::stderr(),
            DisableBracketedPaste,
            terminal::LeaveAlternateScreen
        );
        if !self.was_raw {
            let _ = terminal::disable_raw_mode();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Provider,
    Accounts,
    Models,
}

impl Tab {
    fn next(self) -> Self {
        match self {
            Tab::Provider => Tab::Accounts,
            Tab::Accounts => Tab::Models,
            Tab::Models => Tab::Provider,
        }
    }

    fn prev(self) -> Self {
        match self {
            Tab::Provider => Tab::Models,
            Tab::Accounts => Tab::Provider,
            Tab::Models => Tab::Accounts,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    OpenAI,
    OpenAIResponses,
    Anthropic,
    Gemini,
}

const ALL_PROTOCOLS: [Protocol; 4] = [
    Protocol::OpenAI,
    Protocol::OpenAIResponses,
    Protocol::Anthropic,
    Protocol::Gemini,
];

impl Protocol {
    fn label(self) -> &'static str {
        match self {
            Protocol::OpenAI => "OpenAI",
            Protocol::OpenAIResponses => "OpenAI Responses",
            Protocol::Anthropic => "Anthropic",
            Protocol::Gemini => "Gemini",
        }
    }

    fn code(self) -> &'static str {
        match self {
            Protocol::OpenAI => "openai",
            Protocol::OpenAIResponses => "responses",
            Protocol::Anthropic => "anthropic",
            Protocol::Gemini => "gemini",
        }
    }

    fn default_url(self) -> &'static str {
        match self {
            Protocol::OpenAI | Protocol::OpenAIResponses => "https://api.openai.com/v1",
            Protocol::Anthropic => "https://api.anthropic.com",
            Protocol::Gemini => "https://generativelanguage.googleapis.com/v1beta",
        }
    }

    fn step(self, delta: isize) -> Self {
        let index = ALL_PROTOCOLS
            .iter()
            .position(|p| *p == self)
            .unwrap_or(0) as isize;
        let next = (index + delta).rem_euclid(ALL_PROTOCOLS.len() as isize) as usize;
        ALL_PROTOCOLS[next]
    }

    fn from_provider(provider: &ProviderConfig) -> Self {
        if provider.models.iter().any(|m| {
            matches!(
                m.api.as_deref(),
                Some("openai-responses" | "openai-codex-responses")
            )
        }) || provider.provider_type == "responses"
        {
            return Protocol::OpenAIResponses;
        }
        if provider.provider_type == "anthropic"
            || provider.provider_type == "claude"
            || provider
                .models
                .iter()
                .any(|m| m.api.as_deref() == Some("anthropic-messages"))
        {
            return Protocol::Anthropic;
        }
        if provider.provider_type == "gemini"
            || provider.settings.get("builtinId").and_then(Value::as_str) == Some("google")
            || provider
                .models
                .iter()
                .any(|m| m.api.as_deref() == Some("google-generative-ai"))
        {
            return Protocol::Gemini;
        }
        Protocol::OpenAI
    }
}

fn is_default_protocol_url(url: &str) -> bool {
    let trimmed = url.trim();
    trimmed.is_empty()
        || ALL_PROTOCOLS
            .iter()
            .any(|p| p.default_url().eq_ignore_ascii_case(trimmed))
}

#[derive(Clone, Copy)]
struct ConnectionPreset {
    id: &'static str,
    name: &'static str,
    protocols_display: &'static str,
    host: &'static str,
    default_protocol: Protocol,
    default_url: &'static str,
    key_env: &'static str,
    key_prompt: &'static str,
}

const CONNECTION_PRESETS: &[ConnectionPreset] = &[
    ConnectionPreset {
        id: "opencode",
        name: "OpenCode",
        protocols_display: "OpenAI Responses",
        host: "opencode.ai",
        default_protocol: Protocol::OpenAIResponses,
        default_url: "https://opencode.ai/inference/go/openai/v1",
        key_env: "OPENCODE_API_KEY",
        key_prompt: "Create an OpenAI API key for OpenCode at https://opencode.ai/auth",
    },
    ConnectionPreset {
        id: "deepseek",
        name: "DeepSeek",
        protocols_display: "OpenAI",
        host: "api.deepseek.com",
        default_protocol: Protocol::OpenAI,
        default_url: "https://api.deepseek.com",
        key_env: "DEEPSEEK_API_KEY",
        key_prompt: "Get an API key at https://platform.deepseek.com/api_keys",
    },
    ConnectionPreset {
        id: "zai",
        name: "Zhipu AI",
        protocols_display: "OpenAI / Anthropic",
        host: "open.bigmodel.cn",
        default_protocol: Protocol::OpenAI,
        default_url: "https://open.bigmodel.cn/api/paas/v4",
        key_env: "ZAI_API_KEY",
        key_prompt: "Get an API key at https://open.bigmodel.cn/usercenter/apikeys",
    },
    ConnectionPreset {
        id: "zai-coding-cn",
        name: "Zhipu AI Coding Plan",
        protocols_display: "OpenAI / Anthropic",
        host: "open.bigmodel.cn",
        default_protocol: Protocol::OpenAI,
        default_url: "https://open.bigmodel.cn/api/coding/paas/v4",
        key_env: "ZAI_API_KEY",
        key_prompt: "Get an API key at https://open.bigmodel.cn/usercenter/apikeys",
    },
    ConnectionPreset {
        id: "openrouter",
        name: "OpenRouter",
        protocols_display: "OpenAI / Anthropic",
        host: "openrouter.ai",
        default_protocol: Protocol::OpenAI,
        default_url: "https://openrouter.ai/api/v1",
        key_env: "OPENROUTER_API_KEY",
        key_prompt: "Get an API key at https://openrouter.ai/keys",
    },
    ConnectionPreset {
        id: "taotoken",
        name: "TaoToken",
        protocols_display: "OpenAI / Anthropic",
        host: "taotoken.net",
        default_protocol: Protocol::OpenAI,
        default_url: "https://taotoken.net/v1",
        key_env: "TAOTOKEN_API_KEY",
        key_prompt: "Get an API key at https://taotoken.net",
    },
    ConnectionPreset {
        id: "minimax-cn",
        name: "MiniMax",
        protocols_display: "OpenAI / Anthropic",
        host: "api.minimaxi.com",
        default_protocol: Protocol::Anthropic,
        default_url: "https://api.minimaxi.com/anthropic",
        key_env: "MINIMAX_CN_API_KEY",
        key_prompt: "Get an API key at https://platform.minimaxi.com/user-center/basic-information/interface-key",
    },
    ConnectionPreset {
        id: "minimax-token-plan",
        name: "MiniMax Token Plan",
        protocols_display: "OpenAI / Anthropic",
        host: "api.minimax.cn",
        default_protocol: Protocol::Anthropic,
        default_url: "https://api.minimax.cn/anthropic",
        key_env: "MINIMAX_CN_API_KEY",
        key_prompt: "Get an API key at https://platform.minimaxi.com/user-center/basic-information/interface-key",
    },
    ConnectionPreset {
        id: "anthropic",
        name: "Anthropic",
        protocols_display: "Anthropic",
        host: "api.anthropic.com",
        default_protocol: Protocol::Anthropic,
        default_url: "https://api.anthropic.com",
        key_env: "ANTHROPIC_API_KEY",
        key_prompt: "Get an API key at https://console.anthropic.com/settings/keys",
    },
    ConnectionPreset {
        id: "openai",
        name: "OpenAI",
        protocols_display: "OpenAI Responses",
        host: "api.openai.com",
        default_protocol: Protocol::OpenAIResponses,
        default_url: "https://api.openai.com/v1",
        key_env: "OPENAI_API_KEY",
        key_prompt: "Get an API key at https://platform.openai.com/api-keys",
    },
    ConnectionPreset {
        id: "moonshotai-cn",
        name: "Moonshot Kimi",
        protocols_display: "OpenAI",
        host: "api.moonshot.cn",
        default_protocol: Protocol::OpenAI,
        default_url: "https://api.moonshot.cn/v1",
        key_env: "MOONSHOT_API_KEY",
        key_prompt: "Get an API key at https://platform.moonshot.cn/console/api-keys",
    },
    ConnectionPreset {
        id: "qwen-token-plan-cn",
        name: "Alibaba Qwen Token Plan",
        protocols_display: "OpenAI",
        host: "bailian.console.aliyun.com",
        default_protocol: Protocol::OpenAI,
        default_url: "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
        key_env: "QWEN_TOKEN_PLAN_API_KEY",
        key_prompt: "Get an API key at https://bailian.console.aliyun.com/",
    },
    ConnectionPreset {
        id: "google",
        name: "Google Gemini",
        protocols_display: "Gemini",
        host: "generativelanguage.googleapis.com",
        default_protocol: Protocol::Gemini,
        default_url: "https://generativelanguage.googleapis.com/v1beta",
        key_env: "GEMINI_API_KEY",
        key_prompt: "Get an API key at https://aistudio.google.com/apikey",
    },
];

#[derive(Clone)]
struct ConnectionDraft {
    /// Existing provider ID when editing or adding models to a connected provider.
    existing_id: Option<String>,
    name: String,
    protocol: Protocol,
    address: String,
    /// Raw user input for key (literal API key or env var name). Empty while
    /// editing means keep the existing stored key.
    key_input: String,
    /// Preserved key when editing a provider without re-typing its secret.
    existing_key: Option<String>,
    existing_key_env: Option<String>,
    key_hint: Option<String>,
    manual_model: bool,
}

impl ConnectionDraft {
    fn blank(manual_model: bool) -> Self {
        Self {
            existing_id: None,
            name: String::new(),
            protocol: Protocol::OpenAI,
            address: String::new(),
            key_input: String::new(),
            existing_key: None,
            existing_key_env: None,
            key_hint: None,
            manual_model,
        }
    }

    fn from_preset(preset: &ConnectionPreset, manual_model: bool) -> Self {
        let key_input = if std::env::var_os(preset.key_env).is_some_and(|v| !v.is_empty()) {
            preset.key_env.to_owned()
        } else {
            String::new()
        };
        Self {
            existing_id: None,
            name: preset.id.to_owned(),
            protocol: preset.default_protocol,
            address: preset.default_url.to_owned(),
            key_input,
            existing_key: None,
            existing_key_env: None,
            key_hint: Some(preset.key_prompt.to_owned()),
            manual_model,
        }
    }

    fn from_provider(provider: &ProviderConfig) -> Self {
        let key_env = provider
            .settings
            .get("apiKeyEnv")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let key_input = key_env.clone().unwrap_or_default();
        let address = if provider.base_url.trim().is_empty()
            && provider.settings.get("builtinId").and_then(Value::as_str) == Some("opencode-go")
        {
            "https://opencode.ai/inference/go/openai/v1".into()
        } else {
            provider.base_url.clone()
        };
        Self {
            existing_id: Some(provider.id.clone()),
            name: provider.name.clone(),
            protocol: Protocol::from_provider(provider),
            address,
            key_input,
            existing_key: provider.api_key.clone(),
            existing_key_env: key_env,
            key_hint: None,
            manual_model: false,
        }
    }

    fn resolved_id(&self) -> String {
        if let Some(id) = &self.existing_id {
            return id.clone();
        }
        slugify_provider_id(&self.name)
    }

    fn normalized_url(&self) -> Result<String> {
        let raw = self.address.trim();
        let url = if raw.is_empty() {
            self.protocol.default_url().to_owned()
        } else {
            raw.to_owned()
        };
        ensure!(
            url.starts_with("http://") || url.starts_with("https://"),
            "Address must start with http:// or https://"
        );
        ensure!(
            !url.contains('@') && !url.contains('?') && !url.contains('#'),
            "Leave credentials, query strings and fragments out of the address"
        );
        let protocol = self.protocol.code();
        let url = interactive_config::deepseek_base_url(protocol, &url).unwrap_or(url);
        let url = interactive_config::opencode_base_url(protocol, &url).unwrap_or(url);
        Ok(url)
    }

    fn resolved_credential(&self) -> Result<(Option<String>, Option<String>)> {
        let trimmed = self.key_input.trim();
        if trimmed.is_empty() {
            if self.existing_key.is_some() || self.existing_key_env.is_some() {
                return Ok((self.existing_key.clone(), self.existing_key_env.clone()));
            }
            bail!("Enter an API key or environment variable name");
        }
        match interactive_config::parse_credential(trimmed) {
            Ok(Credential::Key(key)) => Ok((Some(key), None)),
            Ok(Credential::Env(name)) => Ok((None, Some(name))),
            Err(reason) => bail!("{reason}"),
        }
    }

    fn effective_api_key(&self) -> Result<Option<String>> {
        let (stored, env_name) = self.resolved_credential()?;
        Ok(match (stored, env_name) {
            (Some(key), _) => Some(key),
            (None, Some(name)) => std::env::var(&name).ok().filter(|v| !v.is_empty()),
            (None, None) => None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FormField {
    Name,
    Protocol,
    Address,
    Key,
}

impl FormField {
    fn next(self) -> Self {
        match self {
            FormField::Name => FormField::Protocol,
            FormField::Protocol => FormField::Address,
            FormField::Address => FormField::Key,
            FormField::Key => FormField::Name,
        }
    }

    fn prev(self) -> Self {
        match self {
            FormField::Name => FormField::Key,
            FormField::Protocol => FormField::Name,
            FormField::Address => FormField::Protocol,
            FormField::Key => FormField::Address,
        }
    }
}

struct ConnectionFormState {
    draft: ConnectionDraft,
    field: FormField,
    editing: bool,
    error: Option<String>,
}

struct ProviderModelsState {
    provider_id: String,
    filter: String,
    selected: usize,
    error: Option<String>,
}

struct FetchedModelsState {
    draft: ConnectionDraft,
    models: Vec<ListedModel>,
    selected_ids: Vec<String>,
    filter: String,
    cursor: usize,
    from_existing_provider: bool,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfirmField {
    Model,
    ContextBudget,
    MaxOutput,
    SeesImages,
}

struct ModelConfirmState {
    draft: ConnectionDraft,
    /// Primary model ID (editable when `extra_models` is empty).
    model_id: String,
    extra_models: Vec<ListedModel>,
    context_input: String,
    max_output_input: String,
    sees_images: Option<bool>,
    capabilities: Vec<String>,
    advanced_open: bool,
    field: ConfirmField,
    /// Previous fetched list so `Esc` can return to it.
    fetched: Option<FetchedModelsState>,
    from_existing_provider: bool,
    error: Option<String>,
}

impl ModelConfirmState {
    fn from_selected(
        draft: ConnectionDraft,
        chosen: Vec<ListedModel>,
        fetched: Option<FetchedModelsState>,
        from_existing_provider: bool,
    ) -> Self {
        let mut iter = chosen.into_iter();
        let primary = iter.next().unwrap_or(ListedModel {
            id: String::new(),
            name: None,
            context_window: None,
            max_tokens: None,
            supports_vision: None,
            capabilities: Vec::new(),
        });
        let extra_models: Vec<ListedModel> = iter.collect();
        let context_input = primary
            .context_window
            .map(|n| n.to_string())
            .unwrap_or_default();
        let max_output_input = primary
            .max_tokens
            .map(|n| n.to_string())
            .unwrap_or_default();
        Self {
            draft,
            model_id: primary.id,
            extra_models,
            context_input,
            max_output_input,
            sees_images: primary.supports_vision,
            capabilities: primary.capabilities,
            advanced_open: false,
            field: ConfirmField::Model,
            fetched,
            from_existing_provider,
            error: None,
        }
    }

    fn manual(
        draft: ConnectionDraft,
        fetched: Option<FetchedModelsState>,
        from_existing_provider: bool,
        error: Option<String>,
    ) -> Self {
        Self {
            draft,
            model_id: String::new(),
            extra_models: Vec::new(),
            context_input: String::new(),
            max_output_input: String::new(),
            sees_images: None,
            capabilities: Vec::new(),
            advanced_open: false,
            field: ConfirmField::Model,
            fetched,
            from_existing_provider,
            error,
        }
    }

    fn next_field(&mut self) {
        if !self.advanced_open {
            return;
        }
        self.field = match self.field {
            ConfirmField::Model => ConfirmField::ContextBudget,
            ConfirmField::ContextBudget => ConfirmField::MaxOutput,
            ConfirmField::MaxOutput => ConfirmField::SeesImages,
            ConfirmField::SeesImages => ConfirmField::Model,
        };
    }

    fn prev_field(&mut self) {
        if !self.advanced_open {
            return;
        }
        self.field = match self.field {
            ConfirmField::Model => ConfirmField::SeesImages,
            ConfirmField::ContextBudget => ConfirmField::Model,
            ConfirmField::MaxOutput => ConfirmField::ContextBudget,
            ConfirmField::SeesImages => ConfirmField::MaxOutput,
        };
    }

    fn parsed_context(&self) -> Result<Option<u32>> {
        let trimmed = self.context_input.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let value: u32 = trimmed
            .parse()
            .context("Context budget must be a positive integer")?;
        ensure!(value > 0, "Context budget must be positive");
        Ok(Some(value))
    }

    fn parsed_max_output(&self) -> Result<Option<u32>> {
        let trimmed = self.max_output_input.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let value: u32 = trimmed
            .parse()
            .context("Max output must be a positive integer")?;
        ensure!(value > 0, "Max output must be positive");
        Ok(Some(value))
    }
}

enum DeleteTarget {
    Provider { id: String, name: String },
    Model { provider_id: String, model_id: String },
}

enum View {
    Overview,
    ConnectionForm(ConnectionFormState),
    ProviderModels(ProviderModelsState),
    FetchedModels(FetchedModelsState),
    ModelConfirm(ModelConfirmState),
    ConfirmDelete(DeleteTarget),
}

enum OverviewItem<'a> {
    Connected(&'a ProviderConfig),
    Preset(&'static ConnectionPreset),
}

struct PanelState {
    tab: Tab,
    view: View,
    providers: Vec<ProviderConfig>,
    active_provider_id: String,
    active_model_id: String,
    filter: String,
    selected: usize,
    models_filter: String,
    models_selected: usize,
    account_summary: String,
    notice: Option<String>,
}

impl PanelState {
    fn load(paths: &RuntimePaths, session: &Session) -> Result<Self> {
        let providers = load_enabled_providers(paths)?;
        let binding = session.binding_state();
        let account_summary = account_status_line(paths);
        let selected = providers
            .iter()
            .position(|p| p.id == binding.provider_id)
            .unwrap_or(0);
        Ok(Self {
            tab: Tab::Provider,
            view: View::Overview,
            providers,
            active_provider_id: binding.provider_id.clone(),
            active_model_id: binding.model.clone(),
            filter: String::new(),
            selected,
            models_filter: String::new(),
            models_selected: 0,
            account_summary,
            notice: None,
        })
    }

    fn reload(&mut self, paths: &RuntimePaths, session: &Session) -> Result<()> {
        self.providers = load_enabled_providers(paths)?;
        let binding = session.binding_state();
        self.active_provider_id = binding.provider_id.clone();
        self.active_model_id = binding.model.clone();
        self.account_summary = account_status_line(paths);
        Ok(())
    }

    fn select_provider_by_id(&mut self, id: &str) {
        self.filter.clear();
        if let Some(index) = self.providers.iter().position(|p| p.id == id) {
            self.selected = index;
        }
    }

    fn filtered_overview(&self) -> (Vec<&ProviderConfig>, Vec<&'static ConnectionPreset>) {
        let q = self.filter.trim().to_ascii_lowercase();
        let connected: Vec<&ProviderConfig> = self
            .providers
            .iter()
            .filter(|p| {
                q.is_empty()
                    || p.name.to_ascii_lowercase().contains(&q)
                    || p.id.to_ascii_lowercase().contains(&q)
                    || provider_host(p).to_ascii_lowercase().contains(&q)
                    || provider_protocol_label(p).to_ascii_lowercase().contains(&q)
            })
            .collect();
        let presets: Vec<&'static ConnectionPreset> = CONNECTION_PRESETS
            .iter()
            .filter(|preset| {
                q.is_empty()
                    || preset.name.to_ascii_lowercase().contains(&q)
                    || preset.id.to_ascii_lowercase().contains(&q)
                    || preset.host.to_ascii_lowercase().contains(&q)
                    || preset.protocols_display.to_ascii_lowercase().contains(&q)
            })
            .collect();
        (connected, presets)
    }

    fn overview_items(&self) -> Vec<OverviewItem<'_>> {
        let (connected, presets) = self.filtered_overview();
        let mut items = Vec::with_capacity(connected.len() + presets.len());
        for p in connected {
            items.push(OverviewItem::Connected(p));
        }
        for preset in presets {
            items.push(OverviewItem::Preset(preset));
        }
        items
    }

    fn filtered_all_models(&self) -> Vec<(&ProviderConfig, &ModelConfig)> {
        let q = self.models_filter.trim().to_ascii_lowercase();
        let mut out = Vec::new();
        for provider in &self.providers {
            for model in provider.models.iter().filter(|m| m.enabled) {
                if q.is_empty()
                    || model.id.to_ascii_lowercase().contains(&q)
                    || model.name.to_ascii_lowercase().contains(&q)
                    || provider.name.to_ascii_lowercase().contains(&q)
                    || provider.id.to_ascii_lowercase().contains(&q)
                {
                    out.push((provider, model));
                }
            }
        }
        out
    }
}

fn load_enabled_providers(paths: &RuntimePaths) -> Result<Vec<ProviderConfig>> {
    Ok(paths
        .read_config()?
        .providers
        .into_iter()
        .filter(|p| p.enabled)
        .collect())
}

fn account_status_line(paths: &RuntimePaths) -> String {
    match anthropic_auth::status(&paths.data_dir) {
        Ok(status) if status.authenticated => "Signed in (Anthropic account)".into(),
        _ => "Not signed in \u{b7} run /login to sign in with an Anthropic account".into(),
    }
}

pub(crate) fn slugify_provider_id(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if ch == '-' || ch.is_whitespace() {
            if !slug.is_empty() && !last_dash {
                slug.push('-');
                last_dash = true;
            }
        }
    }
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        "custom-provider".into()
    } else {
        slug
    }
}

fn is_managed_by_sign_in(provider: &ProviderConfig) -> bool {
    provider
        .api_key
        .as_deref()
        .is_some_and(anthropic_auth::is_oauth_marker)
        || provider
            .settings
            .get("managedBySignIn")
            .and_then(Value::as_bool)
            == Some(true)
}

fn provider_protocol_label(provider: &ProviderConfig) -> &'static str {
    Protocol::from_provider(provider).label()
}

fn provider_host(provider: &ProviderConfig) -> String {
    let raw = provider.base_url.trim();
    if raw.is_empty() {
        if let Some(builtin) = provider.settings.get("builtinId").and_then(Value::as_str) {
            if matches!(builtin, "opencode" | "opencode-go") {
                return "opencode.ai".into();
            }
        }
        return "built-in".into();
    }
    let without_scheme = raw
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(raw);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    if host_port.is_empty() {
        raw.to_owned()
    } else {
        host_port.to_owned()
    }
}

fn provider_key_status(provider: &ProviderConfig) -> String {
    if is_managed_by_sign_in(provider) {
        return "has a key".into();
    }
    if let Some(env_name) = provider
        .settings
        .get("apiKeyEnv")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        if std::env::var_os(env_name).is_some_and(|v| !v.is_empty()) {
            return "has a key".into();
        }
        return format!("env: {env_name}");
    }
    if provider
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
    {
        return "has a key".into();
    }
    if let Some(builtin) = provider.settings.get("builtinId").and_then(Value::as_str) {
        let env_name = deepseek_code_core::native_engine::provider_runtime::credential_env(builtin);
        if std::env::var_os(&env_name).is_some_and(|v| !v.is_empty()) {
            return "has a key".into();
        }
    }
    "no key".into()
}

fn provider_summary(provider: &ProviderConfig) -> String {
    let protocol = provider_protocol_label(provider);
    let host = provider_host(provider);
    let count = provider.models.iter().filter(|m| m.enabled).count();
    let key = provider_key_status(provider);
    if is_managed_by_sign_in(provider) {
        format!("{protocol} \u{b7} {host} \u{b7} {count} models \u{b7} {key} \u{b7} managed by signing in")
    } else {
        format!("{protocol} \u{b7} {host} \u{b7} {count} models \u{b7} {key}")
    }
}

fn format_k(tokens: u32) -> String {
    format!("{:.1}K", tokens as f64 / 1000.0)
}

fn listed_model_meta(model: &ListedModel) -> String {
    match (model.context_window, model.supports_vision) {
        (Some(ctx), Some(vision)) => format!(
            "context budget {} \u{b7} sees images {}",
            format_k(ctx),
            if vision { "yes" } else { "no" }
        ),
        (Some(ctx), None) => format!("context budget {}", format_k(ctx)),
        (None, Some(vision)) => {
            format!("sees images {}", if vision { "yes" } else { "no" })
        }
        (None, None) => "capabilities not declared".into(),
    }
}

fn saved_model_meta(model: &ModelConfig) -> String {
    let mut parts = Vec::new();
    if let Some(ctx) = model.context_window {
        parts.push(format!("context budget {}", format_k(ctx)));
    }
    if let Some(max) = model.max_tokens {
        parts.push(format!("max output {}", format_k(max)));
    }
    parts.push(format!(
        "sees images {}",
        if model.supports_vision { "yes" } else { "no" }
    ));
    parts.join(" \u{b7} ")
}

fn mask_key_display(value: &str, has_existing: bool) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return if has_existing {
            "(saved key kept; type to replace)".into()
        } else {
            String::new()
        };
    }
    // Keep env var names readable; mask literal secrets.
    let is_env = !trimmed.is_empty()
        && !trimmed.as_bytes()[0].is_ascii_digit()
        && trimmed
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if is_env || trimmed.to_ascii_lowercase().starts_with("env:") {
        trimmed.to_owned()
    } else {
        let len = trimmed.chars().count();
        "\u{2022}".repeat(len.min(32))
    }
}

fn format_aligned_row(prefix: &str, left: &str, right: &str, width: usize) -> String {
    let left_full = format!("{prefix}{left}");
    if right.is_empty() {
        return clipped(&left_full, width);
    }
    let left_w = UnicodeWidthStr::width(left_full.as_str());
    let right_w = UnicodeWidthStr::width(right);
    if left_w + 2 + right_w <= width {
        let gap = width - left_w - right_w;
        format!("{left_full}{}{right}", " ".repeat(gap))
    } else {
        clipped(&format!("{left_full}  {right}"), width)
    }
}

fn format_column_row(prefix: &str, left: &str, col_width: usize, right: &str, width: usize) -> String {
    let left_full = format!("{prefix}{left}");
    let left_w = UnicodeWidthStr::width(left_full.as_str());
    let target = col_width.min(width / 2).max(left_w + 2);
    let pad = target.saturating_sub(left_w).max(2);
    clipped(&format!("{left_full}{}{right}", " ".repeat(pad)), width)
}

fn header_lines(tab: Tab, width: usize) -> Vec<DisplayLine> {
    let rule = "\u{2500}".repeat(width);
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let label = |t: Tab, text: &str| {
        if no_color {
            if t == tab {
                format!("[{text}]")
            } else {
                text.to_owned()
            }
        } else if t == tab {
            format!("\x1b[1;38;2;215;119;87m{text}\x1b[0m")
        } else {
            format!("\x1b[38;2;153;153;153m{text}\x1b[0m")
        }
    };
    let bar = format!(
        "  {}    {}    {}",
        label(Tab::Provider, "provider"),
        label(Tab::Accounts, "Accounts"),
        label(Tab::Models, "Models")
    );
    vec![(rule, TextStyle::Border), (bar, TextStyle::Plain)]
}

fn search_box_lines(query: &str, width: usize) -> (Vec<DisplayLine>, usize, usize) {
    let inner = width.saturating_sub(2).max(4);
    let top = format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(inner));
    let body = clipped(&format!("\u{2502} {query}"), width);
    let bottom = format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(inner));
    let cursor_col = (2 + UnicodeWidthStr::width(query)).min(width.saturating_sub(1));
    (
        vec![
            (top, TextStyle::Border),
            (body, TextStyle::Plain),
            (bottom, TextStyle::Border),
        ],
        1,
        cursor_col,
    )
}

fn render(state: &PanelState, cols: usize, rows: usize) -> (Vec<DisplayLine>, usize, usize) {
    let width = cols.saturating_sub(1).max(40);
    let height = rows.max(12);
    match &state.view {
        View::Overview => match state.tab {
            Tab::Provider => render_overview(state, width, height),
            Tab::Accounts => render_accounts(state, width),
            Tab::Models => render_all_models(state, width, height),
        },
        View::ConnectionForm(form) => render_connection_form(form, width),
        View::ProviderModels(models) => render_provider_models(state, models, width, height),
        View::FetchedModels(fetched) => render_fetched_models(fetched, width, height),
        View::ModelConfirm(confirm) => render_model_confirm(confirm, width),
        View::ConfirmDelete(target) => render_confirm_delete(target, width),
    }
}

fn render_overview(state: &PanelState, width: usize, height: usize) -> (Vec<DisplayLine>, usize, usize) {
    let mut lines = header_lines(Tab::Provider, width);
    let (box_lines, box_row_offset, cursor_col) = search_box_lines(&state.filter, width);
    let cursor_row = lines.len() + box_row_offset;
    lines.extend(box_lines);

    let (connected, presets) = state.filtered_overview();
    let total_selectable = connected.len() + presets.len();
    let selected = state.selected.min(total_selectable.saturating_sub(1));

    // Build flat display rows with a marker indicating which rows are selectable.
    enum RowKind {
        Header(String),
        Connected(usize, String, bool),
        Preset(usize, String, bool),
    }
    let mut all_rows: Vec<RowKind> = Vec::new();
    let col_width = connected
        .iter()
        .map(|p| UnicodeWidthStr::width(p.name.as_str()) + 6)
        .chain(presets.iter().map(|p| UnicodeWidthStr::width(p.name) + 6))
        .max()
        .unwrap_or(28)
        .clamp(24, 38);

    let mut idx = 0;
    if !connected.is_empty() {
        all_rows.push(RowKind::Header("  connected".into()));
        for p in &connected {
            let is_sel = idx == selected;
            let is_active = p.id == state.active_provider_id;
            let prefix = match (is_sel, is_active) {
                (true, true) => "> \u{2022} ",
                (true, false) => ">   ",
                (false, true) => "  \u{2022} ",
                (false, false) => "    ",
            };
            let line = format_column_row(prefix, &p.name, col_width, &provider_summary(p), width);
            all_rows.push(RowKind::Connected(idx, line, is_sel));
            idx += 1;
        }
    }
    if !presets.is_empty() {
        all_rows.push(RowKind::Header("  add a connection".into()));
        for preset in &presets {
            let is_sel = idx == selected;
            let prefix = if is_sel { ">   " } else { "    " };
            let meta = format!("{} \u{b7} {}", preset.protocols_display, preset.host);
            let line = format_column_row(prefix, preset.name, col_width, &meta, width);
            all_rows.push(RowKind::Preset(idx, line, is_sel));
            idx += 1;
        }
    }

    let selected_row_pos = all_rows
        .iter()
        .position(|r| match r {
            RowKind::Connected(i, _, _) | RowKind::Preset(i, _, _) => *i == selected,
            RowKind::Header(_) => false,
        })
        .unwrap_or(0);

    let reserve = 4 + usize::from(state.notice.is_some());
    let viewport = height.saturating_sub(lines.len() + reserve).max(3);
    let max_start = all_rows.len().saturating_sub(viewport);
    let start = if selected_row_pos < viewport {
        0
    } else {
        (selected_row_pos + 1).saturating_sub(viewport).min(max_start)
    };
    let end = (start + viewport).min(all_rows.len());

    let above = all_rows[..start]
        .iter()
        .filter(|r| !matches!(r, RowKind::Header(_)))
        .count();
    let below = all_rows[end..]
        .iter()
        .filter(|r| !matches!(r, RowKind::Header(_)))
        .count();

    lines.push((
        clipped(&format!("  \u{2191}{above} \u{2193}{below}"), width),
        TextStyle::Muted,
    ));

    if all_rows.is_empty() {
        lines.push((clipped("  No matching connections", width), TextStyle::Muted));
    } else {
        for row in &all_rows[start..end] {
            match row {
                RowKind::Header(text) => lines.push((clipped(text, width), TextStyle::Muted)),
                RowKind::Connected(_, text, is_sel) | RowKind::Preset(_, text, is_sel) => {
                    lines.push((
                        text.clone(),
                        if *is_sel {
                            TextStyle::Selected
                        } else {
                            TextStyle::Plain
                        },
                    ));
                }
            }
        }
    }

    lines.push((String::new(), TextStyle::Plain));
    if let Some(notice) = &state.notice {
        lines.push((clipped(&format!("  {notice}"), width), TextStyle::Success));
    }

    let editable = connected
        .get(selected)
        .is_some_and(|p| !is_managed_by_sign_in(p));
    let footer = if editable {
        "  [ ^a Add ] \u{b7} [ ^e Edit ] \u{b7} [ ^n Manual add ] \u{b7} [ ^d Delete ] \u{b7} up/dn select \u{b7} enter see its models \u{b7} tab change page \u{b7} esc close"
    } else {
        "  [ ^a Add ] \u{b7} ^e Edit \u{b7} [ ^n Manual add ] \u{b7} ^d Delete \u{b7} up/dn select \u{b7} enter see its models \u{b7} tab change page \u{b7} esc close"
    };
    lines.push((clipped(footer, width), TextStyle::Muted));
    (lines, cursor_row, cursor_col)
}

fn render_accounts(state: &PanelState, width: usize) -> (Vec<DisplayLine>, usize, usize) {
    let mut lines = header_lines(Tab::Accounts, width);
    lines.push((String::new(), TextStyle::Plain));
    lines.push((clipped("  Accounts", width), TextStyle::Accent));
    lines.push((
        clipped(&format!("  Anthropic OAuth: {}", state.account_summary), width),
        TextStyle::Plain,
    ));
    let managed: Vec<&ProviderConfig> = state
        .providers
        .iter()
        .filter(|p| is_managed_by_sign_in(p))
        .collect();
    if !managed.is_empty() {
        lines.push((String::new(), TextStyle::Plain));
        lines.push((clipped("  signed-in providers", width), TextStyle::Muted));
        for provider in managed {
            lines.push((
                clipped(&format!("  \u{2022} {}  {}", provider.name, provider_summary(provider)), width),
                TextStyle::Plain,
            ));
        }
    }
    lines.push((String::new(), TextStyle::Plain));
    lines.push((
        clipped(
            "  Use /login or /logout in the prompt to manage account sign-in.",
            width,
        ),
        TextStyle::Muted,
    ));
    lines.push((String::new(), TextStyle::Plain));
    lines.push((
        clipped("  tab change page \u{b7} esc close", width),
        TextStyle::Muted,
    ));
    (lines, 1, 2)
}

fn render_all_models(state: &PanelState, width: usize, height: usize) -> (Vec<DisplayLine>, usize, usize) {
    let mut lines = header_lines(Tab::Models, width);
    let (box_lines, box_row_offset, cursor_col) = search_box_lines(&state.models_filter, width);
    let cursor_row = lines.len() + box_row_offset;
    lines.extend(box_lines);

    let items = state.filtered_all_models();
    let selected = state.models_selected.min(items.len().saturating_sub(1));
    let reserve = 4 + usize::from(state.notice.is_some());
    let viewport = height.saturating_sub(lines.len() + reserve).max(3);
    let start = if selected < viewport {
        0
    } else {
        (selected + 1)
            .saturating_sub(viewport)
            .min(items.len().saturating_sub(viewport))
    };
    let end = (start + viewport).min(items.len());
    let above = start;
    let below = items.len().saturating_sub(end);

    lines.push((
        clipped(&format!("  \u{2191}{above} \u{2193}{below}"), width),
        TextStyle::Muted,
    ));

    if items.is_empty() {
        lines.push((clipped("  No models configured", width), TextStyle::Muted));
    } else {
        for (offset, (provider, model)) in items[start..end].iter().enumerate() {
            let index = start + offset;
            let is_sel = index == selected;
            let is_active =
                provider.id == state.active_provider_id && model.id == state.active_model_id;
            let prefix = match (is_sel, is_active) {
                (true, true) => "> \u{2022} ",
                (true, false) => ">   ",
                (false, true) => "  \u{2022} ",
                (false, false) => "    ",
            };
            let left = format!("{} :: {}", provider.name, model.id);
            let right = saved_model_meta(model);
            let row = format_aligned_row(prefix, &left, &right, width);
            lines.push((
                row,
                if is_sel {
                    TextStyle::Selected
                } else {
                    TextStyle::Plain
                },
            ));
        }
    }

    lines.push((String::new(), TextStyle::Plain));
    if let Some(notice) = &state.notice {
        lines.push((clipped(&format!("  {notice}"), width), TextStyle::Success));
    }
    lines.push((
        clipped("  up/dn select \u{b7} enter use model \u{b7} tab change page \u{b7} esc close \u{b7} type to filter", width),
        TextStyle::Muted,
    ));
    (lines, cursor_row, cursor_col)
}

fn render_connection_form(form: &ConnectionFormState, width: usize) -> (Vec<DisplayLine>, usize, usize) {
    let mut lines = header_lines(Tab::Provider, width);
    lines.push((String::new(), TextStyle::Plain));

    let label_col = 10;
    let mut cursor_row = lines.len();
    let mut cursor_col = label_col + 2;

    let name_sel = form.field == FormField::Name;
    let name_line = format!(
        "{} {:<width$}{}",
        if name_sel { ">" } else { " " },
        "name",
        form.draft.name,
        width = label_col
    );
    if name_sel {
        cursor_row = lines.len();
        cursor_col = (2 + label_col + UnicodeWidthStr::width(form.draft.name.as_str()))
            .min(width.saturating_sub(1));
    }
    lines.push((
        clipped(&name_line, width),
        if name_sel {
            TextStyle::Selected
        } else {
            TextStyle::Plain
        },
    ));

    let proto_sel = form.field == FormField::Protocol;
    let proto_val = if proto_sel {
        format!("\u{2039} {} \u{203a}", form.draft.protocol.label())
    } else {
        form.draft.protocol.label().to_owned()
    };
    let proto_line = format!(
        "{} {:<width$}{}",
        if proto_sel { ">" } else { " " },
        "protocol",
        proto_val,
        width = label_col
    );
    if proto_sel {
        cursor_row = lines.len();
        cursor_col = 2 + label_col;
    }
    lines.push((
        clipped(&proto_line, width),
        if proto_sel {
            TextStyle::Selected
        } else {
            TextStyle::Plain
        },
    ));

    let addr_sel = form.field == FormField::Address;
    let addr_line = format!(
        "{} {:<width$}{}",
        if addr_sel { ">" } else { " " },
        "address",
        form.draft.address,
        width = label_col
    );
    if addr_sel {
        cursor_row = lines.len();
        cursor_col = (2 + label_col + UnicodeWidthStr::width(form.draft.address.as_str()))
            .min(width.saturating_sub(1));
    }
    lines.push((
        clipped(&addr_line, width),
        if addr_sel {
            TextStyle::Selected
        } else {
            TextStyle::Plain
        },
    ));

    let key_sel = form.field == FormField::Key;
    let masked = mask_key_display(
        &form.draft.key_input,
        form.editing && (form.draft.existing_key.is_some() || form.draft.existing_key_env.is_some()),
    );
    let key_line = format!(
        "{} {:<width$}{}",
        if key_sel { ">" } else { " " },
        "key",
        masked,
        width = label_col
    );
    if key_sel {
        cursor_row = lines.len();
        let display_len = if form.draft.key_input.is_empty() {
            0
        } else {
            UnicodeWidthStr::width(masked.as_str())
        };
        cursor_col = (2 + label_col + display_len).min(width.saturating_sub(1));
    }
    lines.push((
        clipped(&key_line, width),
        if key_sel {
            TextStyle::Selected
        } else {
            TextStyle::Plain
        },
    ));

    if let Some(hint) = &form.draft.key_hint {
        lines.push((String::new(), TextStyle::Plain));
        lines.push((clipped(&format!("  {hint}"), width), TextStyle::Muted));
    }
    if let Some(error) = &form.error {
        lines.push((String::new(), TextStyle::Plain));
        lines.push((clipped(&format!("  {error}"), width), TextStyle::Accent));
    }

    lines.push((String::new(), TextStyle::Plain));
    lines.push((
        clipped(
            "  tab next field \u{b7} left/right change \u{b7} [ enter save ] \u{b7} [ esc cancel ]",
            width,
        ),
        TextStyle::Muted,
    ));
    (lines, cursor_row, cursor_col)
}

fn render_provider_models(
    state: &PanelState,
    view: &ProviderModelsState,
    width: usize,
    height: usize,
) -> (Vec<DisplayLine>, usize, usize) {
    let mut lines = header_lines(Tab::Provider, width);
    let provider = state.providers.iter().find(|p| p.id == view.provider_id);
    let title = provider
        .map(|p| format!("  models of {} ({})", p.name, provider_protocol_label(p)))
        .unwrap_or_else(|| format!("  models of {}", view.provider_id));
    lines.push((clipped(&title, width), TextStyle::Muted));
    lines.push((String::new(), TextStyle::Plain));
    let filter_row = lines.len();
    let filter_line = format!("  filter: {}", view.filter);
    let cursor_col = UnicodeWidthStr::width(filter_line.as_str()).min(width.saturating_sub(1));
    lines.push((clipped(&filter_line, width), TextStyle::Plain));

    let q = view.filter.trim().to_ascii_lowercase();
    let models: Vec<&ModelConfig> = provider
        .map(|p| {
            p.models
                .iter()
                .filter(|m| {
                    m.enabled
                        && (q.is_empty()
                            || m.id.to_ascii_lowercase().contains(&q)
                            || m.name.to_ascii_lowercase().contains(&q))
                })
                .collect()
        })
        .unwrap_or_default();

    let selected = view.selected.min(models.len().saturating_sub(1));
    let reserve = 4 + usize::from(view.error.is_some());
    let viewport = height.saturating_sub(lines.len() + reserve).max(3);
    let start = if selected < viewport {
        0
    } else {
        (selected + 1)
            .saturating_sub(viewport)
            .min(models.len().saturating_sub(viewport))
    };
    let end = (start + viewport).min(models.len());

    if start > 0 || end < models.len() {
        lines.push((
            clipped(
                &format!("  \u{2191}{start} \u{2193}{}", models.len().saturating_sub(end)),
                width,
            ),
            TextStyle::Muted,
        ));
    }

    if models.is_empty() {
        lines.push((clipped("  No models match", width), TextStyle::Muted));
    } else {
        for (offset, model) in models[start..end].iter().enumerate() {
            let is_sel = start + offset == selected;
            let is_active =
                view.provider_id == state.active_provider_id && model.id == state.active_model_id;
            let prefix = match (is_sel, is_active) {
                (true, true) => "  \u{203a} \u{2022} ",
                (true, false) => "  \u{203a}   ",
                (false, true) => "    \u{2022} ",
                (false, false) => "      ",
            };
            let row = format_aligned_row(prefix, &model.id, &saved_model_meta(model), width);
            lines.push((
                row,
                if is_sel {
                    TextStyle::Selected
                } else {
                    TextStyle::Plain
                },
            ));
        }
    }

    if let Some(error) = &view.error {
        lines.push((String::new(), TextStyle::Plain));
        lines.push((clipped(&format!("  {error}"), width), TextStyle::Accent));
    }
    lines.push((String::new(), TextStyle::Plain));
    lines.push((
        clipped(
            "  [ enter use ] \u{b7} [ ^a fetch models ] \u{b7} [ ^n Manual add ] \u{b7} [ ^d Delete ] \u{b7} [ esc back ] \u{b7} type to filter",
            width,
        ),
        TextStyle::Muted,
    ));
    (lines, filter_row, cursor_col)
}

fn filtered_fetched_indices(fetched: &FetchedModelsState) -> Vec<usize> {
    let q = fetched.filter.trim().to_ascii_lowercase();
    fetched
        .models
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            q.is_empty()
                || m.id.to_ascii_lowercase().contains(&q)
                || m.name
                    .as_deref()
                    .is_some_and(|n| n.to_ascii_lowercase().contains(&q))
        })
        .map(|(i, _)| i)
        .collect()
}

fn render_fetched_models(
    fetched: &FetchedModelsState,
    width: usize,
    height: usize,
) -> (Vec<DisplayLine>, usize, usize) {
    let mut lines = header_lines(Tab::Provider, width);
    lines.push((
        clipped(&format!("  add a model to {}", fetched.draft.name), width),
        TextStyle::Muted,
    ));
    lines.push((String::new(), TextStyle::Plain));
    let filter_row = lines.len();
    let status = format!("  [x] {} \u{b7} filter: {}", fetched.selected_ids.len(), fetched.filter);
    let cursor_col = UnicodeWidthStr::width(status.as_str()).min(width.saturating_sub(1));
    lines.push((clipped(&status, width), TextStyle::Plain));

    let shown = filtered_fetched_indices(fetched);
    let cursor = fetched.cursor.min(shown.len().saturating_sub(1));
    let reserve = 4 + usize::from(fetched.error.is_some());
    let viewport = height.saturating_sub(lines.len() + reserve).max(3);
    let start = if cursor < viewport {
        0
    } else {
        (cursor + 1)
            .saturating_sub(viewport)
            .min(shown.len().saturating_sub(viewport))
    };
    let end = (start + viewport).min(shown.len());

    if start > 0 || end < shown.len() {
        lines.push((
            clipped(
                &format!("  \u{2191}{start} \u{2193}{}", shown.len().saturating_sub(end)),
                width,
            ),
            TextStyle::Muted,
        ));
    }

    if shown.is_empty() {
        lines.push((clipped("  No matching models", width), TextStyle::Muted));
    } else {
        for (offset, &model_index) in shown[start..end].iter().enumerate() {
            let model = &fetched.models[model_index];
            let is_cur = start + offset == cursor;
            let checked = fetched.selected_ids.iter().any(|id| id == &model.id);
            let prefix = format!(
                "  {} [{}] ",
                if is_cur { "\u{203a}" } else { " " },
                if checked { "x" } else { " " }
            );
            let meta = listed_model_meta(model);
            let row = format_aligned_row(&prefix, &model.id, &meta, width);
            lines.push((
                row,
                if is_cur {
                    TextStyle::Selected
                } else {
                    TextStyle::Plain
                },
            ));
        }
    }

    if let Some(error) = &fetched.error {
        lines.push((String::new(), TextStyle::Plain));
        lines.push((clipped(&format!("  {error}"), width), TextStyle::Accent));
    }
    lines.push((String::new(), TextStyle::Plain));
    lines.push((
        clipped(
            "  [ enter continue ] \u{b7} [ space select ] \u{b7} [ ^n Manual add ] \u{b7} [ esc back ] \u{b7} \u{2191}\u{2193} pick \u{b7} Space select \u{b7} Enter confirm (current first) \u{b7} Esc back \u{b7} type to filter",
            width,
        ),
        TextStyle::Muted,
    ));
    (lines, filter_row, cursor_col)
}

fn render_model_confirm(confirm: &ModelConfirmState, width: usize) -> (Vec<DisplayLine>, usize, usize) {
    let mut lines = header_lines(Tab::Provider, width);
    lines.push((
        clipped(&format!("  add a model to {}", confirm.draft.name), width),
        TextStyle::Muted,
    ));
    lines.push((String::new(), TextStyle::Plain));

    let label_col = 10;
    let model_sel = !confirm.advanced_open || confirm.field == ConfirmField::Model;
    let model_display = if confirm.extra_models.is_empty() {
        confirm.model_id.clone()
    } else {
        format!(
            "{} (+{} more)",
            confirm.model_id,
            confirm.extra_models.len()
        )
    };
    let mut cursor_row = lines.len();
    let mut cursor_col = (2 + label_col + UnicodeWidthStr::width(model_display.as_str()))
        .min(width.saturating_sub(1));
    let model_line = format!(
        "{} {:<width$}{}",
        if model_sel { ">" } else { " " },
        "Model",
        model_display,
        width = label_col
    );
    lines.push((
        clipped(&model_line, width),
        if model_sel {
            TextStyle::Selected
        } else {
            TextStyle::Plain
        },
    ));

    let ctx_str = confirm
        .context_input
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&n| n > 0)
        .map(format_k)
        .unwrap_or_else(|| "default".into());
    let out_str = confirm
        .max_output_input
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&n| n > 0)
        .map(format_k)
        .unwrap_or_else(|| "default".into());
    lines.push((
        clipped(
            &format!("  context budget {ctx_str} \u{b7} max output {out_str}"),
            width,
        ),
        TextStyle::Muted,
    ));
    let images_str = match confirm.sees_images {
        Some(true) => "yes",
        Some(false) => "no",
        None => "default",
    };
    lines.push((
        clipped(&format!("  sees images \u{b7} {images_str}"), width),
        TextStyle::Muted,
    ));
    let caps_str = if confirm.capabilities.is_empty() {
        "capabilities not declared".to_owned()
    } else {
        confirm.capabilities.join(" \u{b7} ")
    };
    lines.push((clipped(&format!("  {caps_str}"), width), TextStyle::Muted));

    if confirm.advanced_open {
        lines.push((String::new(), TextStyle::Plain));
        let fields = [
            (
                ConfirmField::ContextBudget,
                "context",
                confirm.context_input.clone(),
            ),
            (
                ConfirmField::MaxOutput,
                "max out",
                confirm.max_output_input.clone(),
            ),
            (
                ConfirmField::SeesImages,
                "images",
                format!("\u{2039} {images_str} \u{203a}"),
            ),
        ];
        for (field, label, val) in fields {
            let sel = confirm.field == field;
            if sel {
                cursor_row = lines.len();
                cursor_col = (2 + label_col + UnicodeWidthStr::width(val.as_str()))
                    .min(width.saturating_sub(1));
            }
            let row = format!(
                "{} {:<width$}{}",
                if sel { ">" } else { " " },
                label,
                val,
                width = label_col
            );
            lines.push((
                clipped(&row, width),
                if sel {
                    TextStyle::Selected
                } else {
                    TextStyle::Plain
                },
            ));
        }
    }

    if let Some(error) = &confirm.error {
        lines.push((String::new(), TextStyle::Plain));
        lines.push((clipped(&format!("  {error}"), width), TextStyle::Accent));
    }

    lines.push((String::new(), TextStyle::Plain));
    lines.push((
        clipped(
            "  [ enter connect and use ] \u{b7} [ ^s save only ] \u{b7} [ ^o advanced settings ] \u{b7} [ ^f fetch from the endpoint ] \u{b7} [ esc back ]",
            width,
        ),
        TextStyle::Muted,
    ));
    (lines, cursor_row, cursor_col)
}

fn render_confirm_delete(target: &DeleteTarget, width: usize) -> (Vec<DisplayLine>, usize, usize) {
    let mut lines = header_lines(Tab::Provider, width);
    lines.push((String::new(), TextStyle::Plain));
    let prompt = match target {
        DeleteTarget::Provider { name, .. } => format!("  Delete provider '{name}'?"),
        DeleteTarget::Model {
            provider_id,
            model_id,
        } => format!("  Remove model '{model_id}' from '{provider_id}'?"),
    };
    lines.push((clipped(&prompt, width), TextStyle::Accent));
    lines.push((String::new(), TextStyle::Plain));
    lines.push((
        clipped("  [ enter confirm delete ] \u{b7} [ esc cancel ]", width),
        TextStyle::Muted,
    ));
    (lines, 3, 2)
}

fn render_status_screen(
    screen: &mut Screen,
    title: &str,
    message: &str,
) -> io::Result<()> {
    let width = (crate::terminal::size().0 as usize).saturating_sub(1).max(40);
    let mut lines = header_lines(Tab::Provider, width);
    lines.push((clipped(&format!("  {title}"), width), TextStyle::Muted));
    lines.push((String::new(), TextStyle::Plain));
    lines.push((clipped(&format!("  {message}"), width), TextStyle::Plain));
    screen.draw(&lines, 3, 2)
}

fn build_model_config(
    id: &str,
    name: Option<&str>,
    protocol: Protocol,
    context_window: Option<u32>,
    max_tokens: Option<u32>,
    supports_vision: Option<bool>,
) -> ModelConfig {
    let mut model = config::provider_template(id).models.remove(0);
    model.id = id.to_owned();
    model.name = name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(id)
        .to_owned();
    model.enabled = true;
    model.api = match protocol {
        Protocol::OpenAIResponses => Some("openai-responses".into()),
        Protocol::Gemini => Some("google-generative-ai".into()),
        Protocol::OpenAI | Protocol::Anthropic => None,
    };
    if let Some(ctx) = context_window {
        model.context_window = Some(ctx);
    }
    if let Some(max) = max_tokens {
        model.max_tokens = Some(max);
    }
    if let Some(vision) = supports_vision {
        model.supports_vision = vision;
    }
    model
}

fn apply_protocol_to_provider(provider: &mut ProviderConfig, protocol: Protocol) {
    provider.provider_type = match protocol {
        Protocol::OpenAI | Protocol::OpenAIResponses => "openai".into(),
        Protocol::Anthropic => "anthropic".into(),
        Protocol::Gemini => "gemini".into(),
    };
    provider.settings.remove("builtinId");
    match protocol {
        Protocol::OpenAIResponses => {
            let builtin = if interactive_config::is_opencode_url(&provider.base_url) {
                "opencode-go"
            } else {
                "openai"
            };
            provider.settings.insert("builtinId".into(), json!(builtin));
        }
        Protocol::Gemini => {
            provider.settings.insert("builtinId".into(), json!("google"));
        }
        Protocol::OpenAI | Protocol::Anthropic => {}
    }
    for model in &mut provider.models {
        model.api = match protocol {
            Protocol::OpenAIResponses => Some("openai-responses".into()),
            Protocol::Gemini => Some("google-generative-ai".into()),
            Protocol::OpenAI | Protocol::Anthropic => None,
        };
    }
}

fn save_connection_edit(paths: &RuntimePaths, draft: &ConnectionDraft) -> Result<String> {
    let name = draft.name.trim();
    ensure!(!name.is_empty(), "Enter a provider name");
    let url = draft.normalized_url()?;
    let (stored_key, key_env) = draft.resolved_credential()?;
    let target_id = draft.resolved_id();

    let mut manager = ConfigManager::new(paths.data_dir.clone());
    manager.reload()?;
    let mut provider = manager
        .get_provider(&target_id)
        .cloned()
        .context("Provider not found")?;
    provider.name = name.to_owned();
    provider.base_url = url;
    provider.api_key = stored_key;
    provider.settings.remove("apiKeyEnv");
    if let Some(env_name) = key_env {
        provider.settings.insert("apiKeyEnv".into(), json!(env_name));
    }
    apply_protocol_to_provider(&mut provider, draft.protocol);
    manager.update_config(|c| {
        if let Some(slot) = c.providers.iter_mut().find(|p| p.id == target_id) {
            *slot = provider;
        }
    })?;
    Ok(target_id)
}

fn save_provider_and_models(
    paths: &RuntimePaths,
    confirm: &ModelConfirmState,
) -> Result<(String, String)> {
    let primary_id = confirm.model_id.trim();
    ensure!(
        !primary_id.is_empty()
            && !primary_id.contains("::")
            && !primary_id.contains(char::is_whitespace),
        "Enter a valid model ID (no spaces or ::)"
    );
    let name = confirm.draft.name.trim();
    ensure!(!name.is_empty(), "Enter a provider name");
    let url = confirm.draft.normalized_url()?;
    let (stored_key, key_env) = confirm.draft.resolved_credential()?;
    let context_window = confirm.parsed_context()?;
    let max_tokens = confirm.parsed_max_output()?;

    let mut new_models = vec![build_model_config(
        primary_id,
        None,
        confirm.draft.protocol,
        context_window,
        max_tokens,
        confirm.sees_images,
    )];
    for extra in &confirm.extra_models {
        if extra.id != primary_id {
            new_models.push(build_model_config(
                &extra.id,
                extra.name.as_deref(),
                confirm.draft.protocol,
                extra.context_window,
                extra.max_tokens,
                extra.supports_vision,
            ));
        }
    }

    let mut manager = ConfigManager::new(paths.data_dir.clone());
    manager.reload()?;
    let id = confirm.draft.resolved_id();
    let mut provider = manager
        .get_provider(&id)
        .cloned()
        .unwrap_or_else(|| {
            let mut p = config::provider_template(primary_id);
            p.id = id.clone();
            p.models.clear();
            p
        });
    provider.id = id.clone();
    provider.name = name.to_owned();
    provider.base_url = url;
    provider.api_key = stored_key;
    provider.enabled = true;
    provider.settings.remove("apiKeyEnv");
    if let Some(env_name) = key_env {
        provider.settings.insert("apiKeyEnv".into(), json!(env_name));
    }
    for model in new_models {
        if let Some(existing) = provider.models.iter_mut().find(|m| m.id == model.id) {
            *existing = model;
        } else {
            provider.models.push(model);
        }
    }
    apply_protocol_to_provider(&mut provider, confirm.draft.protocol);
    manager.update_config(|c| {
        if let Some(slot) = c.providers.iter_mut().find(|p| p.id == id) {
            *slot = provider;
        } else {
            c.providers.push(provider);
        }
    })?;
    Ok((id, primary_id.to_owned()))
}

async fn fetch_models_for_draft(draft: &ConnectionDraft) -> Result<Vec<ListedModel>> {
    let url = draft.normalized_url()?;
    let key = draft
        .effective_api_key()?
        .context("API key is not set in this terminal; enter a model ID manually or set the environment variable")?;
    let protocol = draft.protocol.code();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        model_listing::list_models_detailed(protocol, &url, &key),
    )
    .await
    .context("Listing models timed out after 20 seconds")?;
    result.map_err(|error| {
        anyhow::anyhow!(
            "{}",
            model_listing::redact_secrets(&format!("{error:#}"), &key)
        )
    })
}

async fn switch_session_model(
    paths: &RuntimePaths,
    session: &mut Session,
    provider_id: &str,
    model_id: &str,
) -> Result<()> {
    let selection = interactive_config::select(paths, provider_id, model_id)?;
    session.switch(paths, selection).await?;
    config::save_default_model(paths, provider_id, model_id)?;
    Ok(())
}

fn sanitize_paste(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .collect()
}

pub async fn run(paths: &RuntimePaths, session: &mut Session) -> Result<()> {
    let _guard = PanelScreen::enter()?;
    let mut reader = Reader::new()?;
    let mut screen = Screen::default();
    let mut state = PanelState::load(paths, session)?;

    loop {
        let (cols, rows) = crate::terminal::size();
        let (lines, cursor_row, cursor_col) = render(&state, cols as usize, rows as usize);
        screen.draw(&lines, cursor_row, cursor_col)?;

        let Some(event) = reader.next(Duration::from_millis(100))? else {
            continue;
        };
        match event {
            Event::Resize(_, rows) => {
                screen.resized(rows)?;
                screen.reflow();
                continue;
            }
            Event::Paste(text) => {
                let clean = sanitize_paste(&text);
                handle_paste(&mut state, &clean);
                continue;
            }
            Event::Key(key) if key.kind == KeyEventKind::Release => continue,
            Event::Key(key) => {
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                if control && key.code == KeyCode::Char('c') {
                    return Ok(());
                }
                match handle_key(paths, session, &mut state, &mut screen, key.code, key.modifiers)
                    .await?
                {
                    Action::Continue => {}
                    Action::Close => return Ok(()),
                }
            }
            _ => {}
        }
    }
}

enum Action {
    Continue,
    Close,
}

fn handle_paste(state: &mut PanelState, text: &str) {
    if text.is_empty() {
        return;
    }
    match &mut state.view {
        View::Overview => match state.tab {
            Tab::Provider => {
                state.filter.push_str(text);
                state.selected = 0;
            }
            Tab::Models => {
                state.models_filter.push_str(text);
                state.models_selected = 0;
            }
            Tab::Accounts => {}
        },
        View::ConnectionForm(form) => match form.field {
            FormField::Name => form.draft.name.push_str(text),
            FormField::Address => form.draft.address.push_str(text),
            FormField::Key => form.draft.key_input.push_str(text),
            FormField::Protocol => {}
        },
        View::ProviderModels(view) => {
            view.filter.push_str(text);
            view.selected = 0;
        }
        View::FetchedModels(fetched) => {
            fetched.filter.push_str(text);
            fetched.cursor = 0;
        }
        View::ModelConfirm(confirm) => match confirm.field {
            ConfirmField::Model if confirm.extra_models.is_empty() => {
                confirm.model_id.push_str(text)
            }
            ConfirmField::ContextBudget => {
                confirm
                    .context_input
                    .extend(text.chars().filter(|c| c.is_ascii_digit()));
            }
            ConfirmField::MaxOutput => {
                confirm
                    .max_output_input
                    .extend(text.chars().filter(|c| c.is_ascii_digit()));
            }
            _ => {}
        },
        View::ConfirmDelete(_) => {}
    }
}

async fn handle_key(
    paths: &RuntimePaths,
    session: &mut Session,
    state: &mut PanelState,
    screen: &mut Screen,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Result<Action> {
    let control = modifiers.contains(KeyModifiers::CONTROL);
    match &state.view {
        View::Overview => handle_overview_key(paths, session, state, code, control).await,
        View::ConnectionForm(_) => {
            handle_connection_form_key(paths, session, state, screen, code, control).await
        }
        View::ProviderModels(_) => {
            handle_provider_models_key(paths, session, state, screen, code, control).await
        }
        View::FetchedModels(_) => handle_fetched_models_key(state, code, control),
        View::ModelConfirm(_) => {
            handle_model_confirm_key(paths, session, state, screen, code, control).await
        }
        View::ConfirmDelete(_) => handle_confirm_delete_key(paths, session, state, code).await,
    }
}

async fn handle_overview_key(
    paths: &RuntimePaths,
    session: &mut Session,
    state: &mut PanelState,
    code: KeyCode,
    control: bool,
) -> Result<Action> {
    state.notice = None;
    match code {
        KeyCode::Esc => return Ok(Action::Close),
        KeyCode::Tab => {
            state.tab = state.tab.next();
            return Ok(Action::Continue);
        }
        KeyCode::BackTab => {
            state.tab = state.tab.prev();
            return Ok(Action::Continue);
        }
        _ => {}
    }

    match state.tab {
        Tab::Accounts => Ok(Action::Continue),
        Tab::Models => {
            let count = state.filtered_all_models().len();
            match code {
                KeyCode::Up => state.models_selected = state.models_selected.saturating_sub(1),
                KeyCode::Down => {
                    state.models_selected =
                        (state.models_selected + 1).min(count.saturating_sub(1));
                }
                KeyCode::Enter => {
                    let chosen = state
                        .filtered_all_models()
                        .get(state.models_selected.min(count.saturating_sub(1)))
                        .map(|(p, m)| (p.id.clone(), m.id.clone()));
                    if let Some((provider_id, model_id)) = chosen {
                        match switch_session_model(paths, session, &provider_id, &model_id).await {
                            Ok(()) => {
                                state.reload(paths, session)?;
                                state.notice =
                                    Some(format!("Switched to {provider_id}::{model_id}"));
                            }
                            Err(error) => {
                                state.notice = Some(format!("{error:#}"));
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    state.models_filter.pop();
                    state.models_selected = 0;
                }
                KeyCode::Char(ch) if !control => {
                    state.models_filter.push(ch);
                    state.models_selected = 0;
                }
                _ => {}
            }
            Ok(Action::Continue)
        }
        Tab::Provider => {
            let total = state.overview_items().len();
            let selected = state.selected.min(total.saturating_sub(1));
            state.selected = selected;

            if control {
                match code {
                    KeyCode::Char('a') => {
                        state.view = View::ConnectionForm(ConnectionFormState {
                            draft: ConnectionDraft::blank(false),
                            field: FormField::Name,
                            editing: false,
                            error: None,
                        });
                        return Ok(Action::Continue);
                    }
                    KeyCode::Char('n') => {
                        state.view = View::ConnectionForm(ConnectionFormState {
                            draft: ConnectionDraft::blank(true),
                            field: FormField::Name,
                            editing: false,
                            error: None,
                        });
                        return Ok(Action::Continue);
                    }
                    KeyCode::Char('e') => {
                        let target = match state.overview_items().get(selected) {
                            Some(OverviewItem::Connected(p)) if !is_managed_by_sign_in(p) => {
                                Some((*p).clone())
                            }
                            _ => None,
                        };
                        if let Some(provider) = target {
                            state.view = View::ConnectionForm(ConnectionFormState {
                                draft: ConnectionDraft::from_provider(&provider),
                                field: FormField::Name,
                                editing: true,
                                error: None,
                            });
                        }
                        return Ok(Action::Continue);
                    }
                    KeyCode::Char('d') => {
                        let target = match state.overview_items().get(selected) {
                            Some(OverviewItem::Connected(p)) if !is_managed_by_sign_in(p) => {
                                Some((p.id.clone(), p.name.clone()))
                            }
                            _ => None,
                        };
                        if let Some((id, name)) = target {
                            state.view = View::ConfirmDelete(DeleteTarget::Provider { id, name });
                        }
                        return Ok(Action::Continue);
                    }
                    _ => return Ok(Action::Continue),
                }
            }

            match code {
                KeyCode::Up => state.selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    state.selected = (selected + 1).min(total.saturating_sub(1));
                }
                KeyCode::Enter => {
                    let action = match state.overview_items().get(selected) {
                        Some(OverviewItem::Connected(p)) => Some(Ok(p.id.clone())),
                        Some(OverviewItem::Preset(preset)) => Some(Err(*preset)),
                        None => None,
                    };
                    match action {
                        Some(Ok(provider_id)) => {
                            state.view = View::ProviderModels(ProviderModelsState {
                                provider_id,
                                filter: String::new(),
                                selected: 0,
                                error: None,
                            });
                        }
                        Some(Err(preset)) => {
                            state.view = View::ConnectionForm(ConnectionFormState {
                                draft: ConnectionDraft::from_preset(preset, false),
                                field: FormField::Key,
                                editing: false,
                                error: None,
                            });
                        }
                        None => {}
                    }
                }
                KeyCode::Backspace => {
                    state.filter.pop();
                    state.selected = 0;
                }
                KeyCode::Char(ch) => {
                    state.filter.push(ch);
                    state.selected = 0;
                }
                _ => {}
            }
            Ok(Action::Continue)
        }
    }
}

async fn handle_connection_form_key(
    paths: &RuntimePaths,
    session: &mut Session,
    state: &mut PanelState,
    screen: &mut Screen,
    code: KeyCode,
    control: bool,
) -> Result<Action> {
    let View::ConnectionForm(form) = &mut state.view else {
        return Ok(Action::Continue);
    };
    form.error = None;
    match code {
        KeyCode::Esc => {
            state.view = View::Overview;
            return Ok(Action::Continue);
        }
        KeyCode::Tab | KeyCode::Down => {
            form.field = form.field.next();
            return Ok(Action::Continue);
        }
        KeyCode::BackTab | KeyCode::Up => {
            form.field = form.field.prev();
            return Ok(Action::Continue);
        }
        KeyCode::Left if form.field == FormField::Protocol => {
            let next = form.draft.protocol.step(-1);
            step_protocol(&mut form.draft, next);
            return Ok(Action::Continue);
        }
        KeyCode::Right if form.field == FormField::Protocol => {
            let next = form.draft.protocol.step(1);
            step_protocol(&mut form.draft, next);
            return Ok(Action::Continue);
        }
        KeyCode::Char(' ') if form.field == FormField::Protocol && !control => {
            let next = form.draft.protocol.step(1);
            step_protocol(&mut form.draft, next);
            return Ok(Action::Continue);
        }
        KeyCode::Backspace => {
            match form.field {
                FormField::Name => {
                    form.draft.name.pop();
                }
                FormField::Address => {
                    form.draft.address.pop();
                }
                FormField::Key => {
                    form.draft.key_input.pop();
                }
                FormField::Protocol => {}
            }
            return Ok(Action::Continue);
        }
        KeyCode::Char('u') if control => {
            match form.field {
                FormField::Name => form.draft.name.clear(),
                FormField::Address => form.draft.address.clear(),
                FormField::Key => form.draft.key_input.clear(),
                FormField::Protocol => {}
            }
            return Ok(Action::Continue);
        }
        KeyCode::Char(ch) if !control => {
            match form.field {
                FormField::Name => form.draft.name.push(ch),
                FormField::Address => form.draft.address.push(ch),
                FormField::Key => form.draft.key_input.push(ch),
                FormField::Protocol => {}
            }
            return Ok(Action::Continue);
        }
        KeyCode::Enter => {
            if form.draft.name.trim().is_empty() {
                form.field = FormField::Name;
                form.error = Some("Enter a provider name".into());
                return Ok(Action::Continue);
            }
            if form.draft.address.trim().is_empty() {
                form.draft.address = form.draft.protocol.default_url().to_owned();
            }
            if let Err(error) = form.draft.normalized_url() {
                form.field = FormField::Address;
                form.error = Some(format!("{error:#}"));
                return Ok(Action::Continue);
            }
            if let Err(error) = form.draft.resolved_credential() {
                form.field = FormField::Key;
                form.error = Some(format!("{error:#}"));
                return Ok(Action::Continue);
            }
            if form.editing {
                let draft = form.draft.clone();
                match save_connection_edit(paths, &draft) {
                    Ok(id) => {
                        state.reload(paths, session)?;
                        state.select_provider_by_id(&id);
                        state.notice = Some(format!("Updated {}", draft.name));
                        state.view = View::Overview;
                    }
                    Err(error) => {
                        form.error = Some(format!("{error:#}"));
                    }
                }
                return Ok(Action::Continue);
            }
            let draft = form.draft.clone();
            if draft.manual_model {
                state.view = View::ModelConfirm(ModelConfirmState::manual(draft, None, false, None));
                return Ok(Action::Continue);
            }
            // Fetch models from the endpoint and open Screen 4.
            let _ = render_status_screen(
                screen,
                &format!("add a model to {}", draft.name),
                &format!("Fetching models from {}…", draft.address),
            );
            match fetch_models_for_draft(&draft).await {
                Ok(models) => {
                    state.view = View::FetchedModels(FetchedModelsState {
                        draft,
                        models,
                        selected_ids: Vec::new(),
                        filter: String::new(),
                        cursor: 0,
                        from_existing_provider: false,
                        error: None,
                    });
                }
                Err(error) => {
                    state.view = View::ModelConfirm(ModelConfirmState::manual(
                        draft,
                        None,
                        false,
                        Some(format!("Could not fetch models: {error:#}")),
                    ));
                }
            }
            return Ok(Action::Continue);
        }
        _ => {}
    }
    Ok(Action::Continue)
}

fn step_protocol(draft: &mut ConnectionDraft, next: Protocol) {
    draft.protocol = next;
    if is_default_protocol_url(&draft.address) {
        draft.address = next.default_url().to_owned();
    } else if interactive_config::is_opencode_url(&draft.address) {
        let suffix = match next {
            Protocol::Anthropic => "/inference/go/anthropic/v1",
            Protocol::Gemini => "/inference/go/google/v1beta",
            Protocol::OpenAI | Protocol::OpenAIResponses => "/inference/go/openai/v1",
        };
        draft.address = format!("https://opencode.ai{suffix}");
    }
}

async fn handle_provider_models_key(
    paths: &RuntimePaths,
    session: &mut Session,
    state: &mut PanelState,
    screen: &mut Screen,
    code: KeyCode,
    control: bool,
) -> Result<Action> {
    let provider_id = match &state.view {
        View::ProviderModels(view) => view.provider_id.clone(),
        _ => return Ok(Action::Continue),
    };
    let Some(provider) = state.providers.iter().find(|p| p.id == provider_id).cloned() else {
        state.view = View::Overview;
        return Ok(Action::Continue);
    };

    if code == KeyCode::Esc {
        state.view = View::Overview;
        return Ok(Action::Continue);
    }

    if control {
        match code {
            KeyCode::Char('a') => {
                let draft = ConnectionDraft::from_provider(&provider);
                let _ = render_status_screen(
                    screen,
                    &format!("add a model to {}", provider.name),
                    &format!("Fetching models from {}…", draft.address),
                );
                match fetch_models_for_draft(&draft).await {
                    Ok(models) => {
                        state.view = View::FetchedModels(FetchedModelsState {
                            draft,
                            models,
                            selected_ids: Vec::new(),
                            filter: String::new(),
                            cursor: 0,
                            from_existing_provider: true,
                            error: None,
                        });
                    }
                    Err(error) => {
                        state.view = View::ModelConfirm(ModelConfirmState::manual(
                            draft,
                            None,
                            true,
                            Some(format!("Could not fetch models: {error:#}")),
                        ));
                    }
                }
                return Ok(Action::Continue);
            }
            KeyCode::Char('n') => {
                let mut draft = ConnectionDraft::from_provider(&provider);
                draft.manual_model = true;
                state.view = View::ModelConfirm(ModelConfirmState::manual(draft, None, true, None));
                return Ok(Action::Continue);
            }
            KeyCode::Char('d') => {
                if let View::ProviderModels(view) = &state.view {
                    let q = view.filter.trim().to_ascii_lowercase();
                    let models: Vec<&ModelConfig> = provider
                        .models
                        .iter()
                        .filter(|m| {
                            m.enabled
                                && (q.is_empty()
                                    || m.id.to_ascii_lowercase().contains(&q)
                                    || m.name.to_ascii_lowercase().contains(&q))
                        })
                        .collect();
                    if let Some(model) = models.get(view.selected.min(models.len().saturating_sub(1))) {
                        state.view = View::ConfirmDelete(DeleteTarget::Model {
                            provider_id: provider.id.clone(),
                            model_id: model.id.clone(),
                        });
                    }
                }
                return Ok(Action::Continue);
            }
            _ => return Ok(Action::Continue),
        }
    }

    let View::ProviderModels(view) = &mut state.view else {
        return Ok(Action::Continue);
    };
    view.error = None;
    let q = view.filter.trim().to_ascii_lowercase();
    let models: Vec<&ModelConfig> = provider
        .models
        .iter()
        .filter(|m| {
            m.enabled
                && (q.is_empty()
                    || m.id.to_ascii_lowercase().contains(&q)
                    || m.name.to_ascii_lowercase().contains(&q))
        })
        .collect();
    let selected = view.selected.min(models.len().saturating_sub(1));
    view.selected = selected;

    match code {
        KeyCode::Up => view.selected = selected.saturating_sub(1),
        KeyCode::Down => {
            view.selected = (selected + 1).min(models.len().saturating_sub(1));
        }
        KeyCode::Enter => {
            if let Some(model) = models.get(selected) {
                let model_id = model.id.clone();
                match switch_session_model(paths, session, &provider.id, &model_id).await {
                    Ok(()) => {
                        state.reload(paths, session)?;
                        state.select_provider_by_id(&provider.id);
                        state.notice = Some(format!("Switched to {}::{model_id}", provider.id));
                        state.view = View::Overview;
                    }
                    Err(error) => {
                        if let View::ProviderModels(view) = &mut state.view {
                            view.error = Some(format!("{error:#}"));
                        }
                    }
                }
            }
        }
        KeyCode::Backspace => {
            view.filter.pop();
            view.selected = 0;
        }
        KeyCode::Char(ch) => {
            view.filter.push(ch);
            view.selected = 0;
        }
        _ => {}
    }
    Ok(Action::Continue)
}

fn handle_fetched_models_key(
    state: &mut PanelState,
    code: KeyCode,
    control: bool,
) -> Result<Action> {
    let View::FetchedModels(fetched) = &mut state.view else {
        return Ok(Action::Continue);
    };
    fetched.error = None;
    let shown = filtered_fetched_indices(fetched);
    let cursor = fetched.cursor.min(shown.len().saturating_sub(1));
    fetched.cursor = cursor;

    if control {
        if code == KeyCode::Char('n') {
            let View::FetchedModels(fetched) = std::mem::replace(&mut state.view, View::Overview) else {
                unreachable!()
            };
            let draft = fetched.draft.clone();
            let from_existing = fetched.from_existing_provider;
            state.view = View::ModelConfirm(ModelConfirmState::manual(
                draft,
                Some(fetched),
                from_existing,
                None,
            ));
        }
        return Ok(Action::Continue);
    }

    match code {
        KeyCode::Esc => {
            let View::FetchedModels(fetched) = std::mem::replace(&mut state.view, View::Overview) else {
                unreachable!()
            };
            if fetched.from_existing_provider {
                let provider_id = fetched.draft.resolved_id();
                state.view = View::ProviderModels(ProviderModelsState {
                    provider_id,
                    filter: String::new(),
                    selected: 0,
                    error: None,
                });
            } else {
                state.view = View::ConnectionForm(ConnectionFormState {
                    draft: fetched.draft,
                    field: FormField::Key,
                    editing: false,
                    error: None,
                });
            }
        }
        KeyCode::Up => fetched.cursor = cursor.saturating_sub(1),
        KeyCode::Down => {
            fetched.cursor = (cursor + 1).min(shown.len().saturating_sub(1));
        }
        KeyCode::PageUp => fetched.cursor = cursor.saturating_sub(10),
        KeyCode::PageDown => {
            fetched.cursor = (cursor + 10).min(shown.len().saturating_sub(1));
        }
        KeyCode::Char(' ') => {
            if let Some(&model_idx) = shown.get(cursor) {
                let id = fetched.models[model_idx].id.clone();
                if let Some(pos) = fetched.selected_ids.iter().position(|existing| existing == &id) {
                    fetched.selected_ids.remove(pos);
                } else {
                    fetched.selected_ids.push(id);
                }
            }
        }
        KeyCode::Enter => {
            let current_model = shown.get(cursor).map(|&idx| fetched.models[idx].clone());
            let mut chosen: Vec<ListedModel> = Vec::new();
            if fetched.selected_ids.is_empty() {
                if let Some(model) = current_model {
                    chosen.push(model);
                }
            } else {
                // Current highlighted model comes first when it is also selected.
                if let Some(current) = current_model
                    .as_ref()
                    .filter(|m| fetched.selected_ids.iter().any(|id| id == &m.id))
                {
                    chosen.push(current.clone());
                }
                for id in &fetched.selected_ids {
                    if chosen.iter().any(|m| &m.id == id) {
                        continue;
                    }
                    if let Some(model) = fetched.models.iter().find(|m| &m.id == id) {
                        chosen.push(model.clone());
                    }
                }
            }
            if chosen.is_empty() {
                fetched.error = Some("No model selected".into());
                return Ok(Action::Continue);
            }
            let View::FetchedModels(fetched) = std::mem::replace(&mut state.view, View::Overview) else {
                unreachable!()
            };
            let draft = fetched.draft.clone();
            let from_existing = fetched.from_existing_provider;
            state.view = View::ModelConfirm(ModelConfirmState::from_selected(
                draft,
                chosen,
                Some(fetched),
                from_existing,
            ));
        }
        KeyCode::Backspace => {
            fetched.filter.pop();
            fetched.cursor = 0;
        }
        KeyCode::Char(ch) => {
            fetched.filter.push(ch);
            fetched.cursor = 0;
        }
        _ => {}
    }
    Ok(Action::Continue)
}

async fn handle_model_confirm_key(
    paths: &RuntimePaths,
    session: &mut Session,
    state: &mut PanelState,
    screen: &mut Screen,
    code: KeyCode,
    control: bool,
) -> Result<Action> {
    if control {
        match code {
            KeyCode::Char('o') => {
                if let View::ModelConfirm(confirm) = &mut state.view {
                    confirm.advanced_open = !confirm.advanced_open;
                    if !confirm.advanced_open {
                        confirm.field = ConfirmField::Model;
                    }
                }
                return Ok(Action::Continue);
            }
            KeyCode::Char('f') => {
                let (draft, from_existing) = match &state.view {
                    View::ModelConfirm(c) => (c.draft.clone(), c.from_existing_provider),
                    _ => return Ok(Action::Continue),
                };
                let _ = render_status_screen(
                    screen,
                    &format!("add a model to {}", draft.name),
                    &format!("Fetching models from {}…", draft.address),
                );
                match fetch_models_for_draft(&draft).await {
                    Ok(models) => {
                        state.view = View::FetchedModels(FetchedModelsState {
                            draft,
                            models,
                            selected_ids: Vec::new(),
                            filter: String::new(),
                            cursor: 0,
                            from_existing_provider: from_existing,
                            error: None,
                        });
                    }
                    Err(error) => {
                        if let View::ModelConfirm(confirm) = &mut state.view {
                            confirm.error = Some(format!("Could not fetch models: {error:#}"));
                        }
                    }
                }
                return Ok(Action::Continue);
            }
            KeyCode::Char('s') => {
                let result = match &state.view {
                    View::ModelConfirm(confirm) => save_provider_and_models(paths, confirm),
                    _ => return Ok(Action::Continue),
                };
                match result {
                    Ok((provider_id, model_id)) => {
                        state.reload(paths, session)?;
                        state.select_provider_by_id(&provider_id);
                        state.notice = Some(format!("Saved {provider_id}::{model_id}"));
                        state.view = View::Overview;
                    }
                    Err(error) => {
                        if let View::ModelConfirm(confirm) = &mut state.view {
                            confirm.error = Some(format!("{error:#}"));
                        }
                    }
                }
                return Ok(Action::Continue);
            }
            _ => return Ok(Action::Continue),
        }
    }

    match code {
        KeyCode::Esc => {
            let View::ModelConfirm(confirm) = std::mem::replace(&mut state.view, View::Overview) else {
                unreachable!()
            };
            if let Some(fetched) = confirm.fetched {
                state.view = View::FetchedModels(fetched);
            } else if confirm.from_existing_provider {
                state.view = View::ProviderModels(ProviderModelsState {
                    provider_id: confirm.draft.resolved_id(),
                    filter: String::new(),
                    selected: 0,
                    error: None,
                });
            } else {
                state.view = View::ConnectionForm(ConnectionFormState {
                    draft: confirm.draft,
                    field: FormField::Key,
                    editing: false,
                    error: None,
                });
            }
            return Ok(Action::Continue);
        }
        KeyCode::Enter => {
            let (saved, draft_name, has_key) = match &state.view {
                View::ModelConfirm(confirm) => (
                    save_provider_and_models(paths, confirm),
                    confirm.draft.name.clone(),
                    confirm.draft.effective_api_key().ok().flatten().is_some(),
                ),
                _ => return Ok(Action::Continue),
            };
            let (provider_id, model_id) = match saved {
                Ok(pair) => pair,
                Err(error) => {
                    if let View::ModelConfirm(confirm) = &mut state.view {
                        confirm.error = Some(format!("{error:#}"));
                    }
                    return Ok(Action::Continue);
                }
            };
            if has_key {
                let _ = render_status_screen(
                    screen,
                    &format!("add a model to {draft_name}"),
                    &format!("Checking the connection with {model_id}…"),
                );
                if let Err(error) = interactive_config::verify(paths, &provider_id, &model_id).await
                {
                    state.reload(paths, session)?;
                    if let View::ModelConfirm(confirm) = &mut state.view {
                        confirm.error = Some(format!(
                            "Connection test failed (saved): {error:#} · press ^s to keep or Esc to go back"
                        ));
                    }
                    return Ok(Action::Continue);
                }
                if let Err(error) =
                    switch_session_model(paths, session, &provider_id, &model_id).await
                {
                    if let View::ModelConfirm(confirm) = &mut state.view {
                        confirm.error = Some(format!("Saved, but could not switch: {error:#}"));
                    }
                    return Ok(Action::Continue);
                }
                state.reload(paths, session)?;
                state.select_provider_by_id(&provider_id);
                state.notice = Some(format!("Connected and using {provider_id}::{model_id}"));
                state.view = View::Overview;
            } else {
                state.reload(paths, session)?;
                state.select_provider_by_id(&provider_id);
                state.notice = Some(format!(
                    "Saved {provider_id}::{model_id} (set its API key env var before selecting)"
                ));
                state.view = View::Overview;
            }
            return Ok(Action::Continue);
        }
        KeyCode::Tab | KeyCode::Down => {
            if let View::ModelConfirm(confirm) = &mut state.view {
                confirm.next_field();
            }
        }
        KeyCode::BackTab | KeyCode::Up => {
            if let View::ModelConfirm(confirm) = &mut state.view {
                confirm.prev_field();
            }
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
            if let View::ModelConfirm(confirm) = &mut state.view {
                if confirm.advanced_open && confirm.field == ConfirmField::SeesImages {
                    confirm.sees_images = match confirm.sees_images {
                        Some(true) => Some(false),
                        Some(false) => Some(true),
                        None => Some(true),
                    };
                }
            }
        }
        KeyCode::Backspace => {
            if let View::ModelConfirm(confirm) = &mut state.view {
                confirm.error = None;
                match confirm.field {
                    ConfirmField::Model if confirm.extra_models.is_empty() => {
                        confirm.model_id.pop();
                    }
                    ConfirmField::ContextBudget => {
                        confirm.context_input.pop();
                    }
                    ConfirmField::MaxOutput => {
                        confirm.max_output_input.pop();
                    }
                    _ => {}
                }
            }
        }
        KeyCode::Char(ch) => {
            if let View::ModelConfirm(confirm) = &mut state.view {
                confirm.error = None;
                match confirm.field {
                    ConfirmField::Model if confirm.extra_models.is_empty() => {
                        confirm.model_id.push(ch);
                    }
                    ConfirmField::ContextBudget if ch.is_ascii_digit() => {
                        confirm.context_input.push(ch);
                    }
                    ConfirmField::MaxOutput if ch.is_ascii_digit() => {
                        confirm.max_output_input.push(ch);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(Action::Continue)
}

async fn handle_confirm_delete_key(
    paths: &RuntimePaths,
    session: &mut Session,
    state: &mut PanelState,
    code: KeyCode,
) -> Result<Action> {
    match code {
        KeyCode::Esc => {
            let prev = std::mem::replace(&mut state.view, View::Overview);
            if let View::ConfirmDelete(DeleteTarget::Model { provider_id, .. }) = prev {
                state.view = View::ProviderModels(ProviderModelsState {
                    provider_id,
                    filter: String::new(),
                    selected: 0,
                    error: None,
                });
            }
        }
        KeyCode::Enter => {
            let target = std::mem::replace(&mut state.view, View::Overview);
            if let View::ConfirmDelete(target) = target {
                match target {
                    DeleteTarget::Provider { id, name } => {
                        let mut manager = ConfigManager::new(paths.data_dir.clone());
                        manager.reload()?;
                        manager.update_config(|c| {
                            c.providers.retain(|p| p.id != id);
                            if c.default_provider_id.as_deref() == Some(id.as_str()) {
                                c.default_provider_id = None;
                            }
                        })?;
                        state.reload(paths, session)?;
                        state.notice = Some(format!("Deleted {name}"));
                    }
                    DeleteTarget::Model {
                        provider_id,
                        model_id,
                    } => {
                        let mut manager = ConfigManager::new(paths.data_dir.clone());
                        manager.reload()?;
                        manager.update_config(|c| {
                            if let Some(p) = c.providers.iter_mut().find(|p| p.id == provider_id) {
                                p.models.retain(|m| m.id != model_id);
                            }
                        })?;
                        state.reload(paths, session)?;
                        state.view = View::ProviderModels(ProviderModelsState {
                            provider_id,
                            filter: String::new(),
                            selected: 0,
                            error: None,
                        });
                    }
                }
            }
        }
        _ => {}
    }
    Ok(Action::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_provider(id: &str, name: &str, protocol: Protocol, url: &str) -> ProviderConfig {
        let mut p = config::provider_template("deepseek-v4-pro");
        p.id = id.into();
        p.name = name.into();
        p.base_url = url.into();
        p.api_key = Some("sk-test".into());
        apply_protocol_to_provider(&mut p, protocol);
        p
    }

    #[test]
    fn connection_presets_start_with_opencode_using_responses_protocol() {
        let first = &CONNECTION_PRESETS[0];
        assert_eq!(first.id, "opencode");
        assert_eq!(first.name, "OpenCode");
        assert_eq!(first.default_protocol, Protocol::OpenAIResponses);
        assert_eq!(first.default_url, "https://opencode.ai/inference/go/openai/v1");
        assert_eq!(first.key_env, "OPENCODE_API_KEY");
        assert!(first.key_prompt.contains("https://opencode.ai/auth"));
        assert_eq!(CONNECTION_PRESETS[1].id, "deepseek");
    }

    #[test]
    fn overview_renders_connected_and_add_a_connection_sections() {
        let state = PanelState {
            tab: Tab::Provider,
            view: View::Overview,
            providers: vec![sample_provider(
                "opencode",
                "opencode",
                Protocol::OpenAIResponses,
                "https://opencode.ai/inference/go/openai/v1",
            )],
            active_provider_id: "opencode".into(),
            active_model_id: "deepseek-v4-pro".into(),
            filter: String::new(),
            selected: 0,
            models_filter: String::new(),
            models_selected: 0,
            account_summary: "Not signed in".into(),
            notice: None,
        };
        let (lines, _, _) = render(&state, 120, 24);
        let text: String = lines.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("provider") && text.contains("Accounts") && text.contains("Models"));
        assert!(text.contains("connected"));
        assert!(text.contains("add a connection"));
        assert!(text.contains("OpenAI Responses \u{b7} opencode.ai \u{b7} 1 models \u{b7} has a key"));
        assert!(text.contains("[ ^a Add ]"));
        assert!(text.contains("[ ^e Edit ]"));
        assert!(text.contains("[ ^n Manual add ]"));
        assert!(text.contains("[ ^d Delete ]"));
    }

    #[test]
    fn protocol_cycling_updates_default_address_and_renders_chevrons() {
        let mut form = ConnectionFormState {
            draft: ConnectionDraft::blank(false),
            field: FormField::Protocol,
            editing: false,
            error: None,
        };
        form.draft.name = "opencode".into();
        step_protocol(&mut form.draft, Protocol::OpenAIResponses);
        assert_eq!(form.draft.protocol, Protocol::OpenAIResponses);
        assert_eq!(form.draft.address, "https://api.openai.com/v1");
        let (lines, _, _) = render_connection_form(&form, 100);
        let text: String = lines.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("name      opencode"));
        assert!(text.contains("> protocol  \u{2039} OpenAI Responses \u{203a}"));
        assert!(text.contains("address   https://api.openai.com/v1"));
    }

    #[test]
    fn fetched_models_filter_and_space_selection_work_together() {
        let fetched = FetchedModelsState {
            draft: ConnectionDraft::from_preset(&CONNECTION_PRESETS[0], false),
            models: vec![
                ListedModel {
                    id: "beam-free".into(),
                    name: None,
                    context_window: None,
                    max_tokens: None,
                    supports_vision: None,
                    capabilities: Vec::new(),
                },
                ListedModel {
                    id: "deepseek-v4-pro".into(),
                    name: None,
                    context_window: Some(1_000_000),
                    max_tokens: Some(384_000),
                    supports_vision: Some(false),
                    capabilities: Vec::new(),
                },
                ListedModel {
                    id: "deepseek-v4.1-flash".into(),
                    name: None,
                    context_window: Some(1_000_000),
                    max_tokens: Some(384_000),
                    supports_vision: Some(true),
                    capabilities: Vec::new(),
                },
            ],
            selected_ids: vec!["deepseek-v4-pro".into()],
            filter: "deepseek".into(),
            cursor: 0,
            from_existing_provider: false,
            error: None,
        };
        let shown = filtered_fetched_indices(&fetched);
        assert_eq!(shown, [1, 2]);
        let (lines, _, _) = render_fetched_models(&fetched, 120, 20);
        let text: String = lines.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("[x] 1 \u{b7} filter: deepseek"));
        assert!(text.contains("[x] deepseek-v4-pro"));
        assert!(text.contains("context budget 1000.0K \u{b7} sees images no"));
        assert!(text.contains("context budget 1000.0K \u{b7} sees images yes"));
        assert!(!text.contains("beam-free"));
    }

    #[test]
    fn model_confirm_saves_opencode_with_responses_protocol_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().to_path_buf()).unwrap();
        let mut draft = ConnectionDraft::blank(false);
        draft.name = "opencode".into();
        draft.protocol = Protocol::OpenAIResponses;
        draft.address = "https://opencode.ai/v1".into();
        draft.key_input = "oc_sk_fixture_key".into();
        let confirm = ModelConfirmState::from_selected(
            draft,
            vec![ListedModel {
                id: "deepseek-v4-pro".into(),
                name: None,
                context_window: Some(1_000_000),
                max_tokens: Some(384_000),
                supports_vision: Some(false),
                capabilities: Vec::new(),
            }],
            None,
            false,
        );
        let (lines, _, _) = render_model_confirm(&confirm, 120);
        let text: String = lines.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("> Model     deepseek-v4-pro"));
        assert!(text.contains("context budget 1000.0K \u{b7} max output 384.0K"));
        assert!(text.contains("sees images \u{b7} no"));
        assert!(text.contains("capabilities not declared"));
        assert!(text.contains("[ enter connect and use ]"));
        assert!(text.contains("[ ^s save only ]"));

        let (provider_id, model_id) = save_provider_and_models(&paths, &confirm).unwrap();
        assert_eq!(provider_id, "opencode");
        assert_eq!(model_id, "deepseek-v4-pro");
        let saved = paths.read_config().unwrap();
        let provider = saved.providers.iter().find(|p| p.id == "opencode").unwrap();
        assert_eq!(provider.base_url, "https://opencode.ai/inference/go/openai/v1");
        assert_eq!(provider.settings["builtinId"], "opencode-go");
        assert_eq!(provider.models[0].api.as_deref(), Some("openai-responses"));
        assert_eq!(provider.models[0].context_window, Some(1_000_000));
        assert_eq!(provider.models[0].max_tokens, Some(384_000));
        assert!(!provider.models[0].supports_vision);
    }

    #[test]
    fn fetched_models_key_handling_filters_selects_and_confirms() {
        let mut state = PanelState {
            tab: Tab::Provider,
            view: View::FetchedModels(FetchedModelsState {
                draft: ConnectionDraft::from_preset(&CONNECTION_PRESETS[0], false),
                models: vec![
                    ListedModel {
                        id: "beam-free".into(),
                        name: None,
                        context_window: None,
                        max_tokens: None,
                        supports_vision: None,
                        capabilities: Vec::new(),
                    },
                    ListedModel {
                        id: "deepseek-v4-pro".into(),
                        name: None,
                        context_window: Some(1_000_000),
                        max_tokens: Some(384_000),
                        supports_vision: Some(false),
                        capabilities: Vec::new(),
                    },
                    ListedModel {
                        id: "deepseek-v4.1-flash".into(),
                        name: None,
                        context_window: Some(1_000_000),
                        max_tokens: Some(384_000),
                        supports_vision: Some(true),
                        capabilities: Vec::new(),
                    },
                ],
                selected_ids: Vec::new(),
                filter: String::new(),
                cursor: 0,
                from_existing_provider: false,
                error: None,
            }),
            providers: Vec::new(),
            active_provider_id: String::new(),
            active_model_id: String::new(),
            filter: String::new(),
            selected: 0,
            models_filter: String::new(),
            models_selected: 0,
            account_summary: String::new(),
            notice: None,
        };
        for ch in "deepseek".chars() {
            handle_fetched_models_key(&mut state, KeyCode::Char(ch), false).unwrap();
        }
        handle_fetched_models_key(&mut state, KeyCode::Char(' '), false).unwrap();
        handle_fetched_models_key(&mut state, KeyCode::Enter, false).unwrap();
        match &state.view {
            View::ModelConfirm(confirm) => {
                assert_eq!(confirm.model_id, "deepseek-v4-pro");
                assert_eq!(confirm.context_input, "1000000");
                assert_eq!(confirm.max_output_input, "384000");
                assert_eq!(confirm.sees_images, Some(false));
            }
            _ => panic!("expected ModelConfirm view"),
        }
    }
}
