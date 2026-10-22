//! Isolated, bounded classifier. It never owns an executor or exposes tools.
use super::{
    anthropic_client::{AnthropicClient, AnthropicContent, AnthropicMessage, ContentBlock},
    openai_client::{OpenAIClient, OpenAIContent, OpenAIMessage},
    provider_manager::{ApiFormat, ResolvedProvider},
};
use crate::permissions::auto_config::AutoConfig;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const POLICY: &str = include_str!("auto_permission_policy.txt");

/// Classifier responses are tiny. Bound both successful and error bodies before
/// parsing, including chunked responses without a Content-Length header.
pub(crate) async fn bounded_response<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> anyhow::Result<T> {
    const MAX_BYTES: usize = 16 * 1024;
    if !response.status().is_success() {
        anyhow::bail!("Classifier HTTP failure");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BYTES as u64)
    {
        anyhow::bail!("Classifier response exceeds size limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
            anyhow::bail!("Classifier response exceeds size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Authorized,
    OutsideScope,
    Destructive,
    Disclosure,
    ExternalEffect,
    PolicyChange,
    Ambiguous,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub decision: Decision,
    pub reason_code: Reason,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    InputLimit,
    Transport,
    Timeout,
    InvalidResponse,
}
#[derive(Clone, Debug, Serialize, Default)]
pub struct ClassifierUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}
pub struct Classification {
    pub verdict: Result<Verdict, Failure>,
    pub usage: Option<ClassifierUsage>,
    pub elapsed_ms: u64,
}

pub struct AutoClassifier {
    config: AutoConfig,
    provider: ResolvedProvider,
}

/// User-authored text and assistant tool uses only. Never include assistant
/// claims, hidden reasoning, tool results, system prompts or image bytes.
pub fn transcript(messages: &[Value]) -> Vec<Value> {
    let mut result = Vec::new();
    for message in messages {
        match message["role"].as_str() {
            Some("user") => {
                if let Some(text) = message["content"].as_str() {
                    result.push(json!({"role":"user","text":text}));
                } else if let Some(parts) = message["content"].as_array() {
                    for part in parts {
                        if part["type"] == "text" {
                            if let Some(text) = part["text"].as_str() {
                                result.push(json!({"role":"user","text":text}));
                            }
                        }
                    }
                }
            }
            Some("assistant") => {
                if let Some(calls) = message["tool_calls"].as_array() {
                    for call in calls {
                        result.push(json!({"role":"assistant_tool_use","name":call["function"]["name"],"arguments":call["function"]["arguments"]}));
                    }
                }
                if let Some(parts) = message["content"].as_array() {
                    for part in parts {
                        if part["type"] == "tool_use" {
                            result.push(json!({"role":"assistant_tool_use","name":part["name"],"input":part["input"]}));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    result
}

fn schema() -> Value {
    json!({"type":"object","properties":{"decision":{"type":"string","enum":["allow","deny"]},"reason_code":{"type":"string","enum":["authorized","outside_scope","destructive","disclosure","external_effect","policy_change","ambiguous"]}},"required":["decision","reason_code"],"additionalProperties":false})
}

fn parse(text: &str) -> Result<Verdict, Failure> {
    let verdict: Verdict = serde_json::from_str(crate::structured_output::strip_code_fence(text))
        .map_err(|_| Failure::InvalidResponse)?;
    if (verdict.decision == Decision::Allow) != (verdict.reason_code == Reason::Authorized) {
        return Err(Failure::InvalidResponse);
    }
    Ok(verdict)
}

/// Resolve only the explicitly configured classifier. Current references keep
/// the exact runtime credentials/model; named references use their own provider.
pub fn configured(
    context: &crate::services::ServiceContext,
    current: &ResolvedProvider,
) -> anyhow::Result<(AutoConfig, ResolvedProvider)> {
    let config = crate::permissions::auto_config::load(context)?;
    let selected = config.resolve(&context.paths.providers()?, current)?;
    Ok((config, selected))
}

/// Provenance exists only for a valid, current classifier rejection.
#[derive(Clone, Debug)]
pub struct ClassifierDenial {
    reason: Reason,
}

pub struct AutoAuthorization {
    pub result: crate::permissions::PermissionResult,
    pub telemetry: Option<Value>,
    pub classifier_denial: Option<ClassifierDenial>,
}
impl AutoAuthorization {
    fn plain(result: crate::permissions::PermissionResult, telemetry: Option<Value>) -> Self {
        Self {
            result,
            telemetry,
            classifier_denial: None,
        }
    }

    /// Consumes one outcome, so one invocation cannot dispatch its denial twice.
    /// Retry is model-visible advice only; the original permission stays denied.
    pub async fn with_denial_hook(
        mut self,
        hooks: Option<&crate::services::tool_hooks::HookRunner>,
        tool: &crate::permissions::PermissionContext,
        tool_use_id: &str,
    ) -> Self {
        use crate::services::tool_hooks::HookEvent;
        let (Some(denial), Some(hooks)) = (self.classifier_denial.take(), hooks) else {
            return self;
        };
        let crate::permissions::PermissionResult::Denied(message) = &mut self.result else {
            return self;
        };
        let reason = serde_json::to_value(denial.reason)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "ambiguous".into());
        let payload = json!({"tool_name":tool.tool_name,"tool_input":tool.tool_input,"tool_use_id":tool_use_id,"reason":reason,"permission_mode":"auto"});
        match hooks
            .run(HookEvent::PermissionDenied, &tool.conversation_id, payload)
            .await
        {
            Ok(outcome) => {
                for warning in outcome.warnings {
                    tracing::warn!("PermissionDenied hook: {warning}");
                }
                if outcome.retry {
                    message.push_str("\nThe configured PermissionDenied hook suggests trying this action again. Any new tool call still requires normal permission checks.");
                }
            }
            Err(error) => tracing::warn!("PermissionDenied hook failed: {error:#}"),
        }
        self
    }
}

pub async fn authorize(
    context: &crate::services::ServiceContext,
    manager: &crate::permissions::PermissionManager,
    tool: &crate::permissions::PermissionContext,
    current: &ResolvedProvider,
    messages: &[Value],
    interactive: bool,
) -> AutoAuthorization {
    use crate::permissions::auto_config::FailurePolicy;
    use crate::permissions::{PermissionMode, PermissionResult};
    let denied = |message: &str| PermissionResult::Denied(message.into());
    let Some((decision, expected)) = manager.evaluate_stable(tool) else {
        return AutoAuthorization::plain(
            denied("Permission policy changed repeatedly; retry the action"),
            None,
        );
    };
    if expected.mode.mode != PermissionMode::Auto || !decision.classifier_eligible {
        return AutoAuthorization::plain(decision.result, None);
    }
    let (config, provider) = match configured(context, current) {
        Ok(config) => config,
        Err(_) => return AutoAuthorization::plain(denied("Auto classifier configuration or selected provider is unavailable; review /permissions auto-config"), None),
    };
    let fallback = |message: &str| {
        if interactive && config.failure_policy == FailurePolicy::Manual {
            PermissionResult::RequiresConfirmation(message.into())
        } else {
            denied(message)
        }
    };
    if manager.auto_stats(&tool.conversation_id).denial_limit() {
        return AutoAuthorization::plain(
            fallback("Auto permission denial limit reached; manual review is required"),
            None,
        );
    }
    // Protocol role=user also carries peer messages, summaries and tool data.
    // Positive persisted provenance is the only source of user authorization.
    let mut authorized_messages = match crate::services::permission_authority::history(&context.db, &tool.conversation_id, config.max_input_tokens as usize) {
        Ok(history) if !history.is_empty() => history,
        Ok(_) => return AutoAuthorization::plain(denied("No verified user authorization is recorded for this session; provide a new explicit user request before using Auto"),None),
        Err(error) => return AutoAuthorization::plain(denied(&format!("Verified user authorization could not be loaded: {error}")),None),
    };
    let expected_authority = authorized_messages.clone();
    authorized_messages.extend(
        messages
            .iter()
            .filter(|message| message["role"] == "assistant")
            .cloned(),
    );
    let expected_provider = serde_json::to_value((&provider.provider, &provider.model)).ok();
    let classifier = AutoClassifier::new(config.clone(), provider);
    let outcome = classifier
        .classify_started(
            &authorized_messages,
            &tool.tool_name,
            &tool.tool_input,
            tool.workspace_path.as_deref().unwrap_or(""),
            || {
                manager.update_auto_stats(&tool.conversation_id, |stats| {
                    stats.requests += 1;
                    // A cancelled or interrupted request has unknown usage, never zero.
                    stats.unknown_usage_requests += 1;
                });
            },
        )
        .await;
    let allowed = outcome
        .verdict
        .as_ref()
        .is_ok_and(|v| v.decision == Decision::Allow);
    let stats = manager.update_auto_stats(&tool.conversation_id, |stats| {
        if !allowed {
            stats.denied_consecutive += 1;
            stats.denied_total += 1;
        }
        if let Some(usage) = &outcome.usage {
            stats.unknown_usage_requests = stats.unknown_usage_requests.saturating_sub(1);
            stats.input_tokens += usage.input_tokens;
            stats.output_tokens += usage.output_tokens;
            stats.cache_read_tokens += usage.cache_read_tokens;
            stats.cache_write_tokens += usage.cache_write_tokens;
        }
    });
    let telemetry = Some(
        json!({"auto_permission":{"provider":classifier.provider_id(),"model":classifier.model_id(),"stats":stats,"elapsed_ms":outcome.elapsed_ms,"decision":outcome.verdict.as_ref().map(|v|v.decision).ok(),"failure":outcome.verdict.as_ref().err()}}),
    );
    // A config disable, rule mutation, changed callback or away/back mode change
    // invalidates this exact one-call result. Never turn it into a saved grant.
    let selection_unchanged =
        configured(context, current)
            .ok()
            .is_some_and(|(latest_config, latest)| {
                latest_config == config
                    && serde_json::to_value((&latest.provider, &latest.model)).ok()
                        == expected_provider
            });
    if !selection_unchanged
        || crate::services::permission_authority::history(
            &context.db,
            &tool.conversation_id,
            config.max_input_tokens as usize,
        )
        .ok()
        .as_ref()
            != Some(&expected_authority)
        || manager
            .evaluate_stable(tool)
            .is_none_or(|(latest, snapshot)| snapshot != expected || !latest.classifier_eligible)
    {
        return AutoAuthorization::plain(
            denied(
                "Auto decision discarded because permission policy changed during classification",
            ),
            telemetry,
        );
    }
    let classifier_denial = outcome
        .verdict
        .as_ref()
        .ok()
        .filter(|verdict| verdict.decision == Decision::Deny)
        .map(|verdict| ClassifierDenial {
            reason: verdict.reason_code,
        });
    let result = match outcome.verdict {
        Ok(verdict) if verdict.decision == Decision::Allow => PermissionResult::Granted,
        Ok(verdict) => denied(&format!(
            "Auto permission denied: {:?}",
            verdict.reason_code
        )),
        Err(failure) => fallback(&format!(
            "Auto classifier unavailable ({failure:?}); action requires manual permission"
        )),
    };
    AutoAuthorization {
        result,
        telemetry,
        classifier_denial,
    }
}

impl AutoClassifier {
    pub fn new(config: AutoConfig, provider: ResolvedProvider) -> Self {
        Self { config, provider }
    }
    pub fn provider_id(&self) -> &str {
        &self.provider.provider.id
    }
    pub fn model_id(&self) -> &str {
        &self.provider.model.id
    }

    pub async fn classify(
        &self,
        messages: &[Value],
        tool: &str,
        input: &Value,
        workspace: &str,
    ) -> Classification {
        self.classify_started(messages, tool, input, workspace, || {})
            .await
    }

    async fn classify_started(
        &self,
        messages: &[Value],
        tool: &str,
        input: &Value,
        workspace: &str,
        started: impl FnOnce(),
    ) -> Classification {
        let start = Instant::now();
        let payload = json!({"policy_version":1,"workspace":workspace,"transcript":transcript(messages),"proposed_call":{"name":tool,"input":input}}).to_string();
        // A UTF-8 byte ceiling is intentionally more conservative than typical
        // tokenization. No silent truncation can remove an earlier user limit.
        let limit = self.config.max_input_tokens as usize;
        let model_limit = self
            .provider
            .model
            .context_window
            .unwrap_or(32_768)
            .saturating_sub(1024) as usize;
        if payload.len().saturating_add(POLICY.len()) > limit.min(model_limit) {
            return Classification {
                verdict: Err(Failure::InputLimit),
                usage: None,
                elapsed_ms: 0,
            };
        }
        started();
        let outcome = tokio::time::timeout(
            Duration::from_millis(self.config.timeout_ms),
            self.request(payload),
        )
        .await;
        let (verdict, usage) = match outcome {
            Err(_) => (Err(Failure::Timeout), None),
            Ok(Err(_)) => (Err(Failure::Transport), None),
            Ok(Ok(result)) => result,
        };
        Classification {
            verdict,
            usage,
            elapsed_ms: start.elapsed().as_millis().min(u64::MAX as u128) as u64,
        }
    }

    async fn request(
        &self,
        payload: String,
    ) -> anyhow::Result<(Result<Verdict, Failure>, Option<ClassifierUsage>)> {
        match self.provider.provider.api_format {
            ApiFormat::OpenAI => {
                let response = OpenAIClient::new()
                    .without_retries()
                    .with_response_schema(Some(schema()))
                    .send_message(
                        &self.provider,
                        vec![OpenAIMessage {
                            role: "user".into(),
                            content: OpenAIContent::Text(payload),
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: None,
                            provider_content: None,
                        }],
                        Some(POLICY),
                        vec![],
                        256,
                    )
                    .await?;
                let usage = response.usage.map(|u| ClassifierUsage {
                    input_tokens: u.prompt_tokens as u64,
                    output_tokens: u.completion_tokens as u64,
                    ..Default::default()
                });
                if response.choices.len() != 1 {
                    return Ok((Err(Failure::InvalidResponse), usage));
                }
                let choice = &response.choices[0];
                if choice.finish_reason.as_deref() != Some("stop")
                    || choice
                        .message
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty())
                {
                    return Ok((Err(Failure::InvalidResponse), usage));
                }
                let verdict = match &choice.message.content {
                    OpenAIContent::Text(text) => parse(text),
                    _ => Err(Failure::InvalidResponse),
                };
                Ok((verdict, usage))
            }
            ApiFormat::Anthropic => {
                let response = AnthropicClient::new()
                    .without_retries()
                    .with_response_schema(Some(schema()))
                    .send_message(
                        &self.provider,
                        vec![AnthropicMessage {
                            role: "user".into(),
                            content: AnthropicContent::Text(payload),
                        }],
                        Some(POLICY),
                        vec![],
                        256,
                    )
                    .await?;
                let usage = response.usage.map(|u| ClassifierUsage {
                    input_tokens: u.input_tokens as u64,
                    output_tokens: u.output_tokens as u64,
                    cache_read_tokens: u.cache_read_input_tokens.unwrap_or(0) as u64,
                    cache_write_tokens: u.cache_creation_input_tokens.unwrap_or(0) as u64,
                });
                if response.stop_reason.as_deref() != Some("end_turn") {
                    return Ok((Err(Failure::InvalidResponse), usage));
                }
                // Reasoning models (e.g. DeepSeek) emit thinking before the answer.
                let mut answer = response.content.iter().filter(|block| {
                    !matches!(
                        block,
                        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                    )
                });
                let verdict = match (answer.next(), answer.next()) {
                    (Some(ContentBlock::Text { text }), None) => parse(text),
                    _ => Err(Failure::InvalidResponse),
                };
                Ok((verdict, usage))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn isolated_protocol_requests_have_no_tools_or_untrusted_transcript_and_account_usage() {
        for format in ["openai", "anthropic"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new().route(if format == "openai" { "/v1/chat/completions" } else { "/v1/messages" }, post(move |Json(body): Json<Value>| async move {
                assert!(body.get("tools").is_none());
                assert!(!body.to_string().contains("PRIVATE_SENTINEL"));
                assert!(body.to_string().contains("user authorized fixture"));
                assert!(body.to_string().contains("auto_permission_fixture"));
                assert_eq!(body["model"], "classifier-fixture");
                let text = r#"{"decision":"allow","reason_code":"authorized"}"#;
                Json(if format == "openai" {
                    assert_eq!(body["response_format"]["type"], "json_schema");
                    json!({"id":"fixture","object":"chat.completion","created":0,"model":"classifier-fixture","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":text}}],"usage":{"prompt_tokens":12,"completion_tokens":4,"total_tokens":16}})
                } else {
                    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
                    json!({"id":"fixture","type":"message","role":"assistant","model":"classifier-fixture","stop_reason":"end_turn","content":[{"type":"thinking","thinking":"weighing scope","signature":"sig"},{"type":"text","text":text}],"usage":{"input_tokens":12,"output_tokens":4,"cache_read_input_tokens":2}})
                })
            }));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let provider = ResolvedProvider { provider: serde_json::from_value(json!({"id":"classifier","name":"Classifier","base_url":endpoint,"api_key":"fixture-only","api_format":format,"enabled":true,"models":[]})).unwrap(), model: serde_json::from_value(json!({"id":"classifier-fixture","name":"Fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap() };
            let config = AutoConfig {
                enabled: true,
                classifier: Some(crate::permissions::auto_config::ClassifierSelection::Current),
                ..Default::default()
            };
            let selected = config.resolve(&[], &provider).unwrap();
            let classifier = AutoClassifier::new(config, selected);
            let history = vec![
                json!({"role":"user","content":"user authorized fixture"}),
                json!({"role":"assistant","content":"PRIVATE_SENTINEL"}),
                json!({"role":"tool","content":"PRIVATE_SENTINEL"}),
            ];
            let result = classifier
                .classify(
                    &history,
                    "Bash",
                    &json!({"command":"echo auto_permission_fixture","shell":"powershell"}),
                    "fixture",
                )
                .await;
            assert_eq!(result.verdict.unwrap().decision, Decision::Allow);
            let usage = result.usage.unwrap();
            assert_eq!(usage.input_tokens, 12);
            assert_eq!(usage.output_tokens, 4);
            server.abort();
        }
    }

    #[tokio::test]
    async fn classifier_transport_is_one_attempt_and_input_limit_makes_no_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let requests = requests.clone();
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::TOO_MANY_REQUESTS
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider = ResolvedProvider { provider: serde_json::from_value(json!({"id":"classifier","name":"Classifier","base_url":endpoint,"api_key":"fixture-only","api_format":"openai","enabled":true,"models":[]})).unwrap(), model: serde_json::from_value(json!({"id":"fixture","name":"Fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap() };
        let classifier = AutoClassifier::new(AutoConfig::default(), provider);
        let result = classifier
            .classify(&[], "Bash", &json!({"command":"echo fixture"}), "fixture")
            .await;
        assert_eq!(result.verdict.unwrap_err(), Failure::Transport);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let result = classifier
            .classify(
                &[json!({"role":"user","content":"X".repeat(20_000)})],
                "Bash",
                &json!({}),
                "fixture",
            )
            .await;
        assert_eq!(result.verdict.unwrap_err(), Failure::InputLimit);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        server.abort();
    }
    #[test]
    fn transcript_excludes_assistant_claims_tool_results_and_system_secrets() {
        let input = vec![
            json!({"role":"system","content":"SYSTEM_SECRET"}),
            json!({"role":"user","content":[{"type":"text","text":"User scope"},{"type":"tool_result","content":"RESULT_SECRET"}]}),
            json!({"role":"assistant","content":[{"type":"text","text":"CLAIM_SECRET"},{"type":"tool_use","name":"Read","input":{"file_path":"test"}},{"type":"thinking","thinking":"THINKING_SECRET"}]}),
            json!({"role":"tool","content":"TOOL_SECRET"}),
        ];
        let result = serde_json::to_string(&transcript(&input)).unwrap();
        assert!(result.contains("User scope") && result.contains("Read"));
        assert!(!result.contains("SECRET"));
    }
    #[test]
    fn malformed_and_contradictory_outputs_never_allow() {
        for text in [
            "allow",
            r#"{"decision":"allow"}"#,
            r#"{"decision":"allow","reason_code":"ambiguous"}"#,
            r#"{"decision":"deny","reason_code":"authorized"}"#,
            r#"{"decision":"allow","reason_code":"authorized","extra":true}"#,
        ] {
            assert!(parse(text).is_err());
        }
        assert_eq!(
            parse(r#"{"decision":"allow","reason_code":"authorized"}"#)
                .unwrap()
                .decision,
            Decision::Allow
        );
    }
}
