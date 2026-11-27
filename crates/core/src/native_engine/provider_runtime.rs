//! Native built-in provider transport. No child process or JavaScript runtime is used.
use super::{openai_client::OpenAIMessage, provider_manager::ResolvedProvider};
use crate::{
    performance::{checked_response, retry_with_backoff},
    tools::ToolDefinition,
};
use anyhow::{bail, Context, Result};
use base64::Engine;
use futures::{Stream, StreamExt};
use serde_json::Value;
use std::{pin::Pin, sync::LazyLock};

mod bedrock;
mod cloud_auth;
mod events;
pub(super) mod messages;
#[cfg(test)]
mod tests;

static CATALOG: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../../src/data/providerCatalog.json"))
        .expect("embedded provider catalog")
});
/// A provider entry of the embedded catalog shared with Desktop.
pub fn catalog_entry(id: &str) -> Option<&'static Value> {
    CATALOG["providers"]
        .as_array()?
        .iter()
        .find(|entry| entry["id"] == id)
}

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

pub(crate) fn compaction_request(
    provider: &ResolvedProvider,
    messages: Vec<OpenAIMessage>,
    instructions: &str,
    session: Option<&str>,
) -> Result<Value> {
    let body = messages::request(
        "openai-responses",
        provider,
        &Value::Null,
        messages,
        Some(instructions),
        vec![],
        0,
    )?;
    let mut compact = serde_json::json!({"model":provider.model.id,"input":body["input"],"instructions":body["instructions"]});
    if let Some(key) = super::request_identity::prompt_cache_key(session) {
        compact["prompt_cache_key"] = Value::String(key);
        if supports_prompt_cache_options(&provider.model.id) {
            compact["prompt_cache_options"] = serde_json::json!({"mode":"implicit"});
        }
    }
    Ok(compact)
}

// The compact API accepts prompt_cache_options starting with GPT-5.6.
fn supports_prompt_cache_options(model: &str) -> bool {
    let Some(version) = model
        .rsplit('/')
        .next()
        .and_then(|id| id.strip_prefix("gpt-"))
        .and_then(|s| s.split('-').next())
    else {
        return false;
    };
    let (major, minor) = version.split_once('.').unwrap_or((version, "0"));
    major.parse::<u32>().ok().zip(minor.parse::<u32>().ok())
        .is_some_and(|version| version >= (5, 6))
}

pub(super) fn metadata(provider: &ResolvedProvider) -> Result<(&'static Value, &'static Value)> {
    let id = provider
        .provider
        .builtin_id
        .as_deref()
        .context("Built-in provider ID is missing")?;
    let entry = CATALOG["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .with_context(|| format!("Unknown built-in provider: {id}"))?;
    let model = entry["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == provider.model.id)
        .unwrap_or(&Value::Null);
    Ok((entry, model))
}

/// A provider without its own base URL uses the model's catalog URL, so one
/// built-in entry can serve several protocol endpoints (OpenCode Go).
fn resolved_base<'a>(configured: &'a str, entry: &'a Value, model: &'a Value) -> &'a str {
    let base = if configured.is_empty()
        || Some(configured) == entry["baseUrl"].as_str().map(|s| s.trim_end_matches('/'))
    {
        model["baseUrl"]
            .as_str()
            .or_else(|| entry["baseUrl"].as_str())
            .unwrap_or(configured)
    } else {
        configured
    };
    base.trim_end_matches('/')
}

/// The catalog protocol and the API the shared transport dials for it. The
/// Chat alias (`mistral-conversations`) is the same normalized protocol.
pub(crate) fn resolved_transport(
    provider: &ResolvedProvider,
) -> Result<(&'static str, &'static str)> {
    let (_, model) = metadata(provider)?;
    let protocol = provider
        .model
        .api
        .as_deref()
        .or_else(|| model["api"].as_str())
        .context("Select a catalog model or specify its protocol")?;
    match protocol {
        "mistral-conversations" => Ok(("mistral-conversations", "openai-completions")),
        "azure-openai-responses" => Ok(("azure-openai-responses", "openai-responses")),
        "openai-codex-responses" => Ok(("openai-codex-responses", "openai-responses")),
        "google-vertex" => Ok(("google-vertex", "google-generative-ai")),
        "bedrock-converse-stream" => Ok(("bedrock-converse-stream", "bedrock-converse-stream")),
        "openai-completions" => Ok(("openai-completions", "openai-completions")),
        "anthropic-messages" => Ok(("anthropic-messages", "anthropic-messages")),
        "openai-responses" => Ok(("openai-responses", "openai-responses")),
        "google-generative-ai" => Ok(("google-generative-ai", "google-generative-ai")),
        _ => bail!("Built-in protocol '{protocol}' is not implemented by the native Rust transport; no fallback protocol or Node process was used"),
    }
}

/// The endpoint base for the selected catalog model, including per-model overrides.
pub(crate) fn catalog_base(provider: &ResolvedProvider) -> Result<&str> {
    let (entry, model) = metadata(provider)?;
    Ok(resolved_base(
        provider.provider.base_url.trim_end_matches('/'),
        entry,
        model,
    ))
}

/// Whether this provider's model stream uses the shared Chat Completions
/// transport that post-content timeout recovery can stage. The answer reuses
/// the sender's route/protocol resolution, so the two cannot disagree.
pub(crate) fn chat_completions_eligible(provider: &ResolvedProvider) -> bool {
    if provider.provider.builtin_id.is_none() {
        // The direct client always dials Chat Completions when the OpenAI loop
        // selects it.
        return matches!(
            provider.provider.api_format,
            super::provider_manager::ApiFormat::OpenAI
        );
    }
    resolved_transport(provider).is_ok_and(|(_, api)| api == "openai-completions")
}

/// Whether the transport serializes user text as Responses `input_text` parts.
/// Direct (non-catalog) providers always dial Chat Completions, whose
/// compatible servers may reject content arrays.
pub(crate) fn responses_api(provider: &ResolvedProvider) -> bool {
    provider.provider.builtin_id.is_some()
        && resolved_transport(provider).is_ok_and(|(_, api)| api == "openai-responses")
}

pub async fn stream(
    provider: &ResolvedProvider,
    messages: Vec<OpenAIMessage>,
    system: Option<&str>,
    tools: Vec<ToolDefinition>,
    max_tokens: u32,
) -> Result<ProviderStream> {
    stream_with_schema(provider, messages, system, tools, max_tokens, None).await
}

pub async fn stream_with_schema(
    provider: &ResolvedProvider,
    messages: Vec<OpenAIMessage>,
    system: Option<&str>,
    tools: Vec<ToolDefinition>,
    max_tokens: u32,
    response_schema: Option<&Value>,
) -> Result<ProviderStream> {
    stream_with_schema_observed(
        provider,
        messages,
        system,
        tools,
        max_tokens,
        response_schema,
        None,
    )
    .await
}

pub async fn stream_with_schema_observed(
    provider: &ResolvedProvider,
    messages: Vec<OpenAIMessage>,
    system: Option<&str>,
    tools: Vec<ToolDefinition>,
    max_tokens: u32,
    response_schema: Option<&Value>,
    context_capture: Option<&super::request_context::ContextCapture>,
) -> Result<ProviderStream> {
    stream_with_options(
        provider,
        messages,
        system,
        tools,
        max_tokens,
        response_schema,
        context_capture,
        &Default::default(),
    )
    .await
}

pub async fn stream_with_options(
    provider: &ResolvedProvider,
    messages: Vec<OpenAIMessage>,
    system: Option<&str>,
    tools: Vec<ToolDefinition>,
    max_tokens: u32,
    response_schema: Option<&Value>,
    context_capture: Option<&super::request_context::ContextCapture>,
    options: &super::request_options::RequestOptions,
) -> Result<ProviderStream> {
    Ok(stream_with_options_counted(
        provider,
        messages,
        system,
        tools,
        max_tokens,
        response_schema,
        context_capture,
        options,
    )
    .await?
    .0)
}

/// Same transport, but a protocol that shares one retry budget with its body
/// also returns the HTTP attempts it really made.
pub async fn stream_with_options_counted(
    provider: &ResolvedProvider,
    messages: Vec<OpenAIMessage>,
    system: Option<&str>,
    tools: Vec<ToolDefinition>,
    max_tokens: u32,
    response_schema: Option<&Value>,
    context_capture: Option<&super::request_context::ContextCapture>,
    options: &super::request_options::RequestOptions,
) -> Result<(ProviderStream, crate::performance::AttemptCounter)> {
    // A model stream is bounded per read, not in total; a malformed or zero
    // configuration fails here before any request is built.
    let client = super::model_http::streaming_client(super::model_http::Redirect::None)?;
    stream_with_options_client(
        provider,
        messages,
        system,
        tools,
        max_tokens,
        response_schema,
        context_capture,
        options,
        client,
    )
    .await
}

/// Internal entry that supplies the attempt budget to the shared Chat
/// transport. A single-post budget uses a client with no redirects and no
/// internal retries, so one authorized replacement really is one POST.
#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_with_options_counted_budget(
    provider: &ResolvedProvider,
    messages: Vec<OpenAIMessage>,
    system: Option<&str>,
    tools: Vec<ToolDefinition>,
    max_tokens: u32,
    response_schema: Option<&Value>,
    context_capture: Option<&super::request_context::ContextCapture>,
    options: &super::request_options::RequestOptions,
    budget: crate::performance::AttemptBudget,
) -> Result<(ProviderStream, crate::performance::AttemptCounter)> {
    let client = if budget.is_single_post() {
        super::model_http::single_post_client()?
    } else {
        super::model_http::streaming_client(super::model_http::Redirect::None)?
    };
    stream_with_options_client_budget(
        provider,
        messages,
        system,
        tools,
        max_tokens,
        response_schema,
        context_capture,
        options,
        client,
        budget,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn stream_with_options_client(
    provider: &ResolvedProvider,
    messages: Vec<OpenAIMessage>,
    system: Option<&str>,
    tools: Vec<ToolDefinition>,
    max_tokens: u32,
    response_schema: Option<&Value>,
    context_capture: Option<&super::request_context::ContextCapture>,
    options: &super::request_options::RequestOptions,
    client: reqwest::Client,
) -> Result<(ProviderStream, crate::performance::AttemptCounter)> {
    // A standalone caller must see a malformed configuration as an error, and
    // the budget getter must not run before that check.
    crate::config::OrchestrationConfig::init_global()?;
    stream_with_options_client_budget(
        provider,
        messages,
        system,
        tools,
        max_tokens,
        response_schema,
        context_capture,
        options,
        client,
        crate::performance::AttemptBudget::configured(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn stream_with_options_client_budget(
    provider: &ResolvedProvider,
    messages: Vec<OpenAIMessage>,
    system: Option<&str>,
    tools: Vec<ToolDefinition>,
    max_tokens: u32,
    response_schema: Option<&Value>,
    context_capture: Option<&super::request_context::ContextCapture>,
    options: &super::request_options::RequestOptions,
    client: reqwest::Client,
    budget: crate::performance::AttemptBudget,
) -> Result<(ProviderStream, crate::performance::AttemptCounter)> {
    let provider = &provider.current()?;
    let initiator = if messages.last().is_some_and(|m| m.role != "user") {
        "agent"
    } else {
        "user"
    };
    let wire = request_body(
        provider,
        messages,
        system,
        tools,
        max_tokens,
        response_schema,
        options.session_id.as_deref(),
    )?;
    stream_body(
        provider,
        wire.body,
        initiator,
        wire.builtin_vision.unwrap_or(false),
        max_tokens,
        context_capture,
        options,
        client,
        budget,
    )
    .await
}

pub(super) fn request_body(
    provider: &ResolvedProvider,
    messages: Vec<OpenAIMessage>,
    system: Option<&str>,
    tools: Vec<ToolDefinition>,
    max_tokens: u32,
    response_schema: Option<&Value>,
    session: Option<&str>,
) -> Result<super::followup::WireRequest> {
    let (entry, model) = metadata(provider)?;
    let (protocol, api) = resolved_transport(provider)?;
    let vision = messages.iter().any(|m| matches!(&m.content, super::openai_client::OpenAIContent::Multi(parts) if parts.iter().any(|p| matches!(p,super::openai_client::OpenAIContentPart::Image{..}))));
    let mut body = messages::request(api, provider, model, messages, system, tools, max_tokens)?;
    // Responses endpoints route requests to a prompt cache by this key, so the
    // same conversation keeps reusing one cached prefix.
    if api == "openai-responses" {
        if let Some(key) = super::request_identity::prompt_cache_key(session) {
            body["prompt_cache_key"] = serde_json::json!(key);
        }
    }
    if let Some(schema) = response_schema.filter(|_| {
        !crate::structured_output::provider_enforces_schema(&provider.provider.base_url)
    }) {
        match api {
            "openai-completions" => {
                crate::structured_output::append_openai_instruction(&mut body, schema)
            }
            "anthropic-messages" => {
                crate::structured_output::append_anthropic_instruction(&mut body, schema)
            }
            _ => bail!(
                "Provider protocol '{protocol}' cannot enforce a JSON schema for this endpoint"
            ),
        }
    } else if let Some(schema) = response_schema {
        match api {
            "openai-completions" => {
                body["response_format"] = serde_json::json!({"type":"json_schema","json_schema":{"name":"structured_response","strict":true,"schema":schema}})
            }
            "openai-responses" => {
                body["text"]["format"] = serde_json::json!({"type":"json_schema","name":"structured_response","strict":true,"schema":schema})
            }
            "anthropic-messages" => {
                body["output_config"]["format"] =
                    serde_json::json!({"type":"json_schema","schema":schema})
            }
            _ => bail!(
                "Strict JSON Schema output is not implemented for provider protocol '{protocol}'"
            ),
        }
    }
    if protocol == "openai-codex-responses" {
        body.as_object_mut().unwrap().remove("max_output_tokens");
    }
    if protocol == "mistral-conversations" {
        body.as_object_mut().unwrap().remove("stream_options");
        if let Some(messages) = body["messages"].as_array_mut() {
            for message in messages {
                if let Some(reasoning) = message
                    .as_object_mut()
                    .and_then(|m| m.remove("reasoning_content"))
                    .and_then(|v| v.as_str().map(str::to_string))
                    .filter(|s| !s.is_empty())
                {
                    let mut parts = vec![
                        serde_json::json!({"type":"thinking","thinking":[{"type":"text","text":reasoning}]}),
                    ];
                    match &message["content"] {
                        Value::String(text) if !text.is_empty() => {
                            parts.push(serde_json::json!({"type":"text","text":text}))
                        }
                        Value::Array(content) => parts.extend(content.clone()),
                        _ => {}
                    }
                    message["content"] = serde_json::json!(parts);
                }
            }
        }
        if provider.model.effort.as_deref() == Some("off") {
            body.as_object_mut().unwrap().remove("reasoning_effort");
        }
        if provider.model.effort.as_deref().is_some_and(|e| e != "off")
            && !matches!(
                provider.model.id.as_str(),
                "mistral-small-2603" | "mistral-small-latest" | "mistral-medium-3.5"
            )
        {
            body.as_object_mut().unwrap().remove("reasoning_effort");
            body["prompt_mode"] = serde_json::json!("reasoning");
        }
    }
    if api == "anthropic-messages" {
        crate::services::anthropic_auth::apply_fast(
            &mut body,
            provider.model.fast,
            &provider.model.id,
            &provider.provider.base_url,
        )?;
    } else {
        anyhow::ensure!(
            !crate::services::anthropic_auth::is_oauth_marker(&credential(
                provider,
                entry["id"].as_str().unwrap_or_default()
            )),
            "Anthropic OAuth credentials require the Anthropic messages API"
        );
        anyhow::ensure!(
            !provider.model.fast,
            "Fast mode requires the Anthropic messages API"
        );
    }
    Ok(super::followup::WireRequest {
        api: api.to_owned(),
        body,
        builtin_vision: Some(vision),
    })
}

fn resolve_endpoint(
    provider: &ResolvedProvider,
    protocol: &str,
    api: &str,
    key: &str,
) -> Result<(String, String)> {
    let account_state = provider.binding_snapshot();
    let env = |name: &str| super::session_binding::account_var(account_state.as_deref(), name);
    let (entry, model) = metadata(provider)?;
    let configured = provider.provider.base_url.trim_end_matches('/');
    let region = if protocol == "bedrock-converse-stream" {
        provider
            .model
            .id
            .strip_prefix("arn:aws:bedrock:")
            .and_then(|s| s.split(':').next())
            .map(str::to_string)
            .or_else(|| env("AWS_REGION").ok())
            .or_else(|| env("AWS_DEFAULT_REGION").ok())
            .or_else(|| {
                reqwest::Url::parse(if configured.is_empty() {
                    model["baseUrl"].as_str().unwrap_or_default()
                } else {
                    configured
                })
                .ok()
                .and_then(|u| {
                    u.host_str()
                        .and_then(|h| h.strip_prefix("bedrock-runtime."))
                        .and_then(|h| h.split('.').next())
                        .map(str::to_string)
                })
            })
            .unwrap_or_else(|| "us-east-1".into())
    } else {
        String::new()
    };
    let base = catalog_base(provider)?;
    let base = if protocol == "bedrock-converse-stream"
        && (configured.is_empty() || configured.contains(".amazonaws.com"))
    {
        format!(
            "https://bedrock-runtime.{region}.amazonaws.com{}",
            if region.starts_with("cn-") { ".cn" } else { "" }
        )
    } else if protocol == "google-vertex" && (base.is_empty() || base.contains("{location}")) {
        if !key.is_empty() && key != "gcp-vertex-credentials" {
            "https://aiplatform.googleapis.com/v1/publishers/google".to_string()
        } else {
            let project = env("GOOGLE_CLOUD_PROJECT")
                .or_else(|_| env("GCLOUD_PROJECT"))
                .context("Vertex ADC requires GOOGLE_CLOUD_PROJECT")?;
            let location = env("GOOGLE_CLOUD_LOCATION")
                .context("Vertex ADC requires GOOGLE_CLOUD_LOCATION")?;
            let host = if location == "global" {
                "aiplatform.googleapis.com".to_string()
            } else {
                format!("{location}-aiplatform.googleapis.com")
            };
            format!(
                "https://{host}/v1/projects/{}/locations/{}/publishers/google",
                urlencoding::encode(&project),
                urlencoding::encode(&location)
            )
        }
    } else if protocol == "azure-openai-responses" && base.is_empty() {
        env("AZURE_OPENAI_BASE_URL")
            .or_else(|_| {
                env("AZURE_OPENAI_RESOURCE_NAME")
                    .map(|r| format!("https://{r}.openai.azure.com/openai/v1"))
            })
            .unwrap_or_default()
    } else {
        base.to_string()
    };
    if base.is_empty() {
        bail!("Provider {} requires an explicit base URL", entry["id"]);
    }
    let mut base = base;
    for name in ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_GATEWAY_ID"] {
        let placeholder = format!("{{{name}}}");
        if base.contains(&placeholder) {
            base = base.replace(
                &placeholder,
                &urlencoding::encode(
                    &env(name)
                        .with_context(|| format!("Provider endpoint requires {name}"))?,
                ),
            );
        }
    }
    let base = base.trim_end_matches('/');
    let url = match api {
        "openai-completions" => {
            if protocol == "mistral-conversations" && !base.ends_with("/v1") {
                endpoint(base, "/v1/chat/completions")
            } else {
                endpoint(base, "/chat/completions")
            }
        }
        "anthropic-messages" => {
            if base.ends_with("/messages") {
                base.to_owned()
            } else if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            }
        }
        "openai-responses" if protocol == "openai-codex-responses" => {
            if base.ends_with("/codex") || base.ends_with("/codex/responses") {
                endpoint(base, "/responses")
            } else {
                endpoint(base, "/codex/responses")
            }
        }
        "openai-responses" if protocol == "azure-openai-responses" => {
            let parsed = reqwest::Url::parse(base).context("Invalid Azure base URL")?;
            let base = if parsed.path().trim_matches('/').is_empty() {
                format!("{base}/openai/v1")
            } else if parsed.path() == "/openai" {
                format!("{base}/v1")
            } else {
                base.to_string()
            };
            format!(
                "{}?api-version={}",
                endpoint(&base, "/responses"),
                urlencoding::encode(
                    &env("AZURE_OPENAI_API_VERSION").unwrap_or_else(|_| "v1".into())
                )
            )
        }
        "openai-responses" => endpoint(base, "/responses"),
        "bedrock-converse-stream" => format!(
            "{base}/model/{}/converse-stream",
            urlencoding::encode(&provider.model.id)
        ),
        _ => {
            let parsed = reqwest::Url::parse(base).context("Invalid Gemini base URL")?;
            let base = if protocol == "google-generative-ai"
                && parsed.path().trim_matches('/').is_empty()
            {
                format!("{base}/v1beta")
            } else {
                base.to_string()
            };
            format!(
                "{base}/models/{}:streamGenerateContent?alt=sse",
                urlencoding::encode(&provider.model.id)
            )
        }
    };
    Ok((url, region))
}

fn catalog_headers(
    entry: &Value,
    protocol: &str,
    api: &str,
    key: &str,
    initiator: &str,
    vision: bool,
) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
    let sensitive = |value: &str| -> Result<HeaderValue> {
        let mut header = HeaderValue::from_str(value)?;
        header.set_sensitive(true);
        Ok(header)
    };
    let mut headers = HeaderMap::new();
    if entry["id"] == "github-copilot" {
        headers.insert("x-initiator", HeaderValue::from_str(initiator)?);
        headers.insert(
            "openai-intent",
            HeaderValue::from_static("conversation-edits"),
        );
        if vision {
            headers.insert("copilot-vision-request", HeaderValue::from_static("true"));
        }
    }
    if protocol == "openai-codex-responses" {
        headers.insert(
            "chatgpt-account-id",
            HeaderValue::from_str(&codex_account(key)?)?,
        );
        headers.insert(
            "openai-beta",
            HeaderValue::from_static("responses=experimental"),
        );
    }
    if entry["id"] == "cloudflare-ai-gateway" {
        headers.insert(
            "cf-aig-authorization",
            sensitive(&format!("Bearer {key}"))?,
        );
    } else if protocol == "azure-openai-responses" {
        headers.insert("api-key", sensitive(key)?);
    } else if !key.is_empty() && matches!(api, "openai-completions" | "openai-responses") {
        headers.insert(
            AUTHORIZATION,
            sensitive(&format!("Bearer {key}"))?,
        );
    }
    Ok(headers)
}

pub(crate) fn compact_endpoint(
    provider: &ResolvedProvider,
) -> Result<(reqwest::Url, reqwest::header::HeaderMap)> {
    let (entry, _) = metadata(provider)?;
    let key = credential(provider, entry["id"].as_str().unwrap_or_default());
    // Explicit native callers can select Responses for a model whose ordinary
    // protocol is Chat. A Responses alias retains its own URL and credentials.
    let protocol = match resolved_transport(provider) {
        Ok((configured, "openai-responses")) => configured,
        _ => "openai-responses",
    };
    let (url, _) = resolve_endpoint(provider, protocol, "openai-responses", &key)?;
    let mut url = reqwest::Url::parse(&url).context("Native compaction requires a gateway URL")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && (url.query().is_none() || protocol == "azure-openai-responses"),
        "Invalid native compaction gateway URL"
    );
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid native compaction path"))?
        .push("compact");
    Ok((
        url,
        catalog_headers(entry, protocol, "openai-responses", &key, "user", false)?,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_body(
    provider: &ResolvedProvider,
    body: Value,
    initiator: &'static str,
    vision: bool,
    max_tokens: u32,
    context_capture: Option<&super::request_context::ContextCapture>,
    options: &super::request_options::RequestOptions,
    client: reqwest::Client,
    budget: crate::performance::AttemptBudget,
) -> Result<(ProviderStream, crate::performance::AttemptCounter)> {
    let account_state = provider.binding_snapshot();
    let (entry, _) = metadata(provider)?;
    let (protocol, api) = resolved_transport(provider)?;
    let key = credential(provider, entry["id"].as_str().unwrap_or_default());
    let (url, region) = resolve_endpoint(provider, protocol, api, &key)?;
    let transport_headers = catalog_headers(entry, protocol, api, &key, initiator, vision)?;
    let google_token =
        if protocol == "google-vertex" && (key.is_empty() || key == "gcp-vertex-credentials") {
            // A credential exchange is a buffered request, so it keeps the
            // total-deadline policy even though the model body is idle-bounded.
            let buffered = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            Some(cloud_auth::google_token(&buffered, provider).await?)
        } else {
            None
        };
    let encoded_body = serde_json::to_string(&body)?;
    let harness_headers = options.headers(provider)?;
    let identity = super::request_identity::RequestIdentity::from_options(
        context_capture,
        provider,
        &options,
    );
    let api = api.to_owned();
    let opened = {
        let client = client.clone();
        let url = url.clone();
        let harness_headers = harness_headers.clone();
        let encoded_body = encoded_body.clone();
        let body = body.clone();
        let key = key.clone();
        let transport_headers = transport_headers.clone();
        let provider = provider.clone();
        let options = options.clone();
        let capture = context_capture.cloned();
        let google_token = google_token.clone();
        let account_state = account_state.clone();
        let api_owned = api.clone();
        move |identity: &super::request_identity::RequestIdentity| -> super::stream_retry::OpenFuture {
            let client = client.clone();
            let url = url.clone();
            let harness_headers = harness_headers.clone();
            let encoded_body = encoded_body.clone();
            let body = body.clone();
            let key = key.clone();
            let transport_headers = transport_headers.clone();
            let identity_headers = identity.headers();
            let provider = provider.clone();
            let options = options.clone();
            let capture = capture.clone();
            let google_token = google_token.clone();
        let account_state = account_state.clone();
            let api = api_owned.clone();
            let region = region.clone();
            Box::pin(async move {
                let mut request = client
                    .post(&url)
                    .headers(transport_headers)
                    .headers(identity_headers)
                    .headers(harness_headers)
                    .header("content-type", "application/json")
                    .body(encoded_body.clone());
                match api.as_str() {
                    "anthropic-messages" => {
                        request = request.header("anthropic-version", "2023-06-01");
                        if crate::services::anthropic_auth::is_oauth_marker(&key) || provider.model.fast {
                            request = crate::services::anthropic_auth::authorize(request, &key, &provider.provider.base_url, provider.model.fast).await?;
                        } else if entry["id"] == "github-copilot" || key.starts_with("sk-ant-oat") {
                            request = request.bearer_auth(&key);
                        } else if entry["id"] != "cloudflare-ai-gateway" {
                            request = request.header("x-api-key", &key);
                        }
                    }
                    "google-generative-ai" => {
                        if let Some(token) = &google_token {
                            request = request.bearer_auth(token);
                        } else {
                            request = request.header("x-goog-api-key", &key);
                        }
                    }
                    "bedrock-converse-stream" => {
                        if key.is_empty() {
                            request =
                                request.headers(cloud_auth::aws_headers(&url, &region, &encoded_body, account_state.as_deref())?);
                        } else {
                            request = request.bearer_auth(&key);
                        }
                    }
                    _ => {}
                }
                if let Some(capture) = &capture {
                    capture
                        .record_transport(&api, &body, provider.model.context_window, max_tokens, Some(vision), &url, &provider.model.id)
                        .await;
                }
                checked_response(
                    super::request_recording::send(
                        request,
                        &options.clone().observed(capture.as_ref()),
                        &provider,
                        &api,
                        true,
                    )
                    .await?,
                )
                .await
            })
        }
    };
    if api == "openai-completions" {
        // A fallible initialization keeps a malformed configuration an error
        // instead of caching its defaults for the rest of the process.
        crate::config::OrchestrationConfig::init_global()?;
        return super::stream_retry::attempts_with_budget(identity, opened, chat_decoder, budget)
            .await;
    }
    let response = retry_with_backoff(|| {
        let response = opened(&identity);
        async move { response.await }
    })
    .await?;
    let mut bytes = response.bytes_stream();
    let stream: ProviderStream = Box::pin(async_stream::try_stream! {
        let mut pending = Vec::new();
        let mut state = events::State::default();
        while let Some(part) = bytes.next().await {
            pending.extend_from_slice(&part?);
            if pending.len() > 16 * 1024 * 1024 { Err(anyhow::anyhow!("Provider SSE frame exceeds 16 MiB"))?; }
            if api == "bedrock-converse-stream" {
                while let Some(value)=bedrock::next(&mut pending)? {for chunk in state.convert(&api,value)? {yield format!("data: {}\n\n",chunk);}}
                continue;
            }
            while let Some((end, separator)) = frame_end(&pending) {
                let frame = String::from_utf8(pending[..end].to_vec()).context("Provider returned invalid UTF-8")?;
                pending.drain(..end + separator);
                let data = frame.lines().filter_map(|line| line.strip_prefix("data:").map(|v| v.strip_prefix(' ').unwrap_or(v))).collect::<Vec<_>>().join("\n");
                if data.is_empty() { continue; }
                if data == "[DONE]" { state.finished = true; continue; }
                let value: Value = serde_json::from_str(&data).context("Invalid provider SSE JSON")?;
                for chunk in state.convert(&api, value)? { yield format!("data: {}\n\n", chunk); }
            }
        }
        if !pending.iter().all(u8::is_ascii_whitespace) { Err(anyhow::anyhow!("Provider stream ended inside an SSE frame"))?; }
        if !state.finished { Err(anyhow::anyhow!("Provider stream ended before completion"))?; }
        if let Some(chunk)=state.complete(&api) {yield format!("data: {}\n\n",chunk);}
        yield "data: [DONE]\n\n".to_string();
    });
    Ok((stream, crate::performance::AttemptCounter::default()))
}

/// Decode one Chat Completions response. Parser state is rebuilt for every
/// attempt, and a clean EOF after a wholly empty prelude is left to the shared
/// prelude gate so it can reopen the request. Raw provider data closes that
/// window immediately: an unfinished frame, a terminal marker, content of any
/// kind, an opaque part, unknown data or a provider error.
pub(super) fn chat_decoder(response: reqwest::Response) -> super::stream_retry::DecodedStream {
    Box::pin(async_stream::try_stream! {
        let api = "openai-completions";
        let mut bytes = response.bytes_stream();
        let mut pending = Vec::new();
        let mut state = events::State::default();
        // Closed as soon as a second attempt could duplicate received data.
        let mut barrier = false;
        // The raw provider terminal marker, independent of a JSON finish reason.
        let mut terminal = false;
        while let Some(part) = bytes.next().await {
            let part = match part {
                Ok(part) => part,
                Err(error) => {
                    // A read failure is retryable only while nothing received so
                    // far would be duplicated by a new request.
                    let failure = if barrier || !pending.iter().all(u8::is_ascii_whitespace) {
                        anyhow::Error::new(super::stream_retry::NoReplay::with_terminal_marker(
                            error, terminal,
                        ))
                    } else {
                        anyhow::Error::new(error)
                    };
                    Err::<bytes::Bytes, _>(failure)?
                }
            };
            pending.extend_from_slice(&part);
            if pending.len() > 16 * 1024 * 1024 { Err(anyhow::anyhow!("Provider SSE frame exceeds 16 MiB"))?; }
            while let Some((end, separator)) = frame_end(&pending) {
                let frame = String::from_utf8(pending[..end].to_vec()).context("Provider returned invalid UTF-8")?;
                pending.drain(..end + separator);
                let data = frame.lines().filter_map(|line| line.strip_prefix("data:").map(|v| v.strip_prefix(' ').unwrap_or(v))).collect::<Vec<_>>().join("\n");
                if data.is_empty() { continue; }
                if data == "[DONE]" {
                    // Latch the terminal marker before the next body read.
                    state.finished = true;
                    barrier = true;
                    terminal = true;
                    continue;
                }
                let value: Value = serde_json::from_str(&data).context("Invalid provider SSE JSON")?;
                if let Some(error) = chat_error(&value) {
                    // Remote text is reduced here, before any diagnostic exists.
                    let failure = crate::performance::RequestFailure::remote_event(error, None, None, barrier);
                    Err(anyhow::Error::new(failure))?;
                }
                // Raw eligibility is decided before normalization can drop data.
                if !super::stream_retry::empty_prelude_value(&value) { barrier = true; }
                for chunk in state.convert(api, value)? {
                    yield format!("data: {}\n\n", chunk);
                }
                // A finish_reason is as terminal as a raw [DONE] marker.
                if state.finished { barrier = true; }
            }
        }
        if !pending.iter().all(u8::is_ascii_whitespace) { Err(anyhow::anyhow!("Provider stream ended inside an SSE frame"))?; }
        if !state.finished {
            if barrier {
                Err(anyhow::anyhow!("Provider stream ended before completion"))?;
            }
        } else {
            if let Some(chunk)=state.complete(api) {yield format!("data: {}\n\n",chunk);}
            yield "data: [DONE]\n\n".to_string();
        }
    })
}

/// The Chat provider error forms `events::State::convert` rejects. The nested
/// payload is used when present; a top-level error event is its own payload.
/// The provider's error payload, whichever shape the event uses: a non-null
/// nested `error` member, or a top-level `{"type":"error"}` event. Shared with
/// the trace so a direct Chat error vetoes recovery like a built-in one.
pub(super) fn chat_error(value: &Value) -> Option<&Value> {
    if !value["error"].is_null() {
        return Some(&value["error"]);
    }
    (value["type"] == "error").then_some(value)
}

/// Observe binary completion without exposing the decoded/normalized stream to
/// the recorder. Parsing errors remain the original consumer's responsibility.
pub(crate) fn recorded_binary_completion(pending: &mut Vec<u8>) -> bool {
    let mut completed = false;
    loop {
        match bedrock::next(pending) {
            Ok(Some(event)) => completed |= event["type"] == "messageStop",
            Ok(None) => break,
            Err(_) => {
                pending.clear();
                break;
            }
        }
    }
    completed
}

fn endpoint(base: &str, suffix: &str) -> String {
    if base.ends_with(suffix) {
        base.to_owned()
    } else {
        format!("{base}{suffix}")
    }
}

fn frame_end(bytes: &[u8]) -> Option<(usize, usize)> {
    (0..bytes.len()).find_map(|i| {
        if bytes[i..].starts_with(b"\r\n\r\n") {
            Some((i, 4))
        } else if bytes[i..].starts_with(b"\n\n") {
            Some((i, 2))
        } else {
            None
        }
    })
}

fn credential(provider: &ResolvedProvider, id: &str) -> String {
    if !provider.provider.api_key.is_empty() {
        return provider.provider.api_key.clone();
    }
    super::session_binding::account_var(provider.binding_snapshot().as_deref(), &credential_env(id)).unwrap_or_default()
}

pub fn credential_env(id: &str) -> String {
    match id {
        "google" => "GEMINI_API_KEY".to_string(),
        "google-vertex" => "GOOGLE_CLOUD_API_KEY".to_string(),
        "amazon-bedrock" => "AWS_BEARER_TOKEN_BEDROCK".to_string(),
        "zai" | "zai-coding-cn" => "ZAI_API_KEY".to_string(),
        "moonshotai" | "moonshotai-cn" => "MOONSHOT_API_KEY".to_string(),
        "minimax-cn" => "MINIMAX_CN_API_KEY".to_string(),
        "kimi-coding" => "KIMI_API_KEY".to_string(),
        "huggingface" => "HF_TOKEN".to_string(),
        "opencode-go" => "OPENCODE_API_KEY".to_string(),
        "vercel-ai-gateway" => "AI_GATEWAY_API_KEY".to_string(),
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => "CLOUDFLARE_API_KEY".to_string(),
        "qwen-token-plan-cn" | "qwen-token-plan-individual" => {
            "QWEN_TOKEN_PLAN_API_KEY".to_string()
        }
        "github-copilot" => "COPILOT_GITHUB_TOKEN".to_string(),
        "azure-openai-responses" => "AZURE_OPENAI_API_KEY".to_string(),
        "openai-codex" => "OPENAI_CODEX_TOKEN".to_string(),
        _ => format!("{}_API_KEY", id.replace('-', "_").to_uppercase()),
    }
}

fn codex_account(token: &str) -> Result<String> {
    let payload = token
        .split('.')
        .nth(1)
        .context("Codex requires an OAuth access token with an account claim")?;
    let data = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("Invalid Codex token encoding")?;
    let payload: Value = serde_json::from_slice(&data).context("Invalid Codex token payload")?;
    payload["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .map(str::to_string)
        .context("Codex token has no account claim")
}
