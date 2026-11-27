use super::*;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Url,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpHook {
    #[serde(rename = "type")]
    kind: String,
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default, rename = "allowedEnvVars")]
    allowed_env_vars: Vec<String>,
    #[serde(skip)]
    pub(super) allowed_urls: Option<Vec<String>>,
    #[serde(skip)]
    pub(super) policy_env_vars: Option<Vec<String>>,
}

impl HttpHook {
    pub(super) fn validate(&self) -> Result<()> {
        let url = Url::parse(&self.url).context("Invalid HTTP hook URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "HTTP hook requires an http(s) URL"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
            "HTTP hook URL cannot contain credentials or a fragment"
        );
        if let Ok(ip) = url
            .host_str()
            .unwrap()
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
        {
            ensure!(
                allowed_ip(ip),
                "HTTP hook target must be public or loopback"
            );
        }
        for (name, value) in &self.headers {
            HeaderName::from_bytes(name.as_bytes()).context("Invalid HTTP hook header name")?;
            HeaderValue::from_str(&sanitize(value)).context("Invalid HTTP hook header value")?;
        }
        Ok(())
    }

    pub(super) fn url(&self) -> &str {
        &self.url
    }

    fn request_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        for (name, value) in &self.headers {
            let value = interpolate(value, |name| {
                if self.allowed_env_vars.iter().any(|v| v == name)
                    && self
                        .policy_env_vars
                        .as_ref()
                        .is_none_or(|vars| vars.iter().any(|v| v == name))
                {
                    std::env::var(name).unwrap_or_default()
                } else {
                    String::new()
                }
            });
            headers.insert(
                HeaderName::from_bytes(name.as_bytes())?,
                HeaderValue::from_str(&value).context("Invalid interpolated HTTP hook header")?,
            );
        }
        Ok(headers)
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|c| !matches!(c, '\r' | '\n' | '\0'))
        .collect()
}

fn interpolate(value: &str, resolve: impl Fn(&str) -> String) -> String {
    let pattern = regex::Regex::new(r"\$\{([A-Z_][A-Z0-9_]*)\}|\$([A-Z_][A-Z0-9_]*)").unwrap();
    sanitize(
        &pattern.replace_all(value, |captures: &regex::Captures<'_>| {
            resolve(
                captures
                    .get(1)
                    .or_else(|| captures.get(2))
                    .unwrap()
                    .as_str(),
            )
        }),
    )
}

fn allowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || !(ip.is_private()
                    || ip.is_link_local()
                    || ip.is_unspecified()
                    || ip.is_broadcast()
                    || ip.is_multicast()
                    || ip.octets()[0] == 0
                    || ip.octets()[0] >= 240
                    || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1])))
        }
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or_else(
            || {
                ip.is_loopback()
                    || !(ip.is_unspecified()
                        || ip.is_multicast()
                        || ip.is_unique_local()
                        || ip.is_unicast_link_local())
            },
            |ip| allowed_ip(IpAddr::V4(ip)),
        ),
    }
}

#[derive(Debug)]
struct GuardedDns;
impl reqwest::dns::Resolve for GuardedDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addresses: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            if addresses.is_empty() || addresses.iter().any(|address| !allowed_ip(address.ip())) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "HTTP hook DNS target must be public or loopback",
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

pub(super) async fn execute_http(
    hook: &ActiveHook,
    config: &HttpHook,
    input: &Value,
) -> Result<(i32, String, String)> {
    let operation = async {
        config.validate()?;
        if let Some(patterns) = &config.allowed_urls {
            ensure!(
                patterns.iter().any(|pattern| {
                    regex::Regex::new(&format!(
                        "^{}$",
                        regex::escape(pattern).replace(r"\*", ".*")
                    ))
                    .is_ok_and(|pattern| pattern.is_match(&config.url))
                }),
                "HTTP hook URL blocked by allowedHttpHookUrls"
            );
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(GuardedDns))
            .build()?;
        let headers = config.request_headers()?;
        let _progress = super::super::hook_progress::Running::start(
            input["session_id"].as_str().unwrap_or(""),
            hook.status_message.as_deref(),
        );
        let mut response = client
            .post(&config.url)
            .headers(headers)
            .json(input)
            .send()
            .await
            .map_err(|error| error.without_url())?;
        ensure!(
            response.status().is_success(),
            "HTTP hook returned status {}",
            response.status().as_u16()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| error.without_url())?
        {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= OUTPUT_LIMIT as usize,
                "HTTP hook output exceeded 1 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(bytes).context("HTTP hook response is not UTF-8")?;
        let body = if body.trim().is_empty() {
            "{}".to_owned()
        } else {
            body
        };
        let value: Value =
            serde_json::from_str(&body).context("HTTP hook must return a JSON object")?;
        ensure!(value.is_object(), "HTTP hook must return a JSON object");
        ensure!(
            value.get("async").is_none() || value["async"] == false,
            "HTTP hook async output is not supported"
        );
        // Validate before acknowledging once, including event-specific decisions.
        parse_json_output(hook.event, value)?;
        Ok::<_, anyhow::Error>((0, body, String::new()))
    };
    match tokio::time::timeout(Duration::from_secs(hook.timeout), operation).await {
        Ok(Ok(output)) => Ok(output),
        // HTTP failures are nonblocking warnings, never synthetic approval.
        Ok(Err(error)) => Ok((1, String::new(), format!("HTTP hook failed: {error:#}"))),
        Err(_) => Ok((
            1,
            String::new(),
            format!("HTTP hook timed out after {} seconds", hook.timeout),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    };

    #[derive(Default)]
    struct Fixture {
        requests: Mutex<Vec<(HeaderMap, Value)>>,
        reply: Mutex<(u16, String)>,
        delay: AtomicU64,
    }

    async fn endpoint(
        State(state): State<Arc<Fixture>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> impl axum::response::IntoResponse {
        state.requests.lock().unwrap().push((headers, body));
        tokio::time::sleep(Duration::from_millis(state.delay.load(Ordering::Relaxed))).await;
        let (status, body) = state.reply.lock().unwrap().clone();
        (
            StatusCode::from_u16(status).unwrap(),
            [("location", "/redirected")],
            body,
        )
    }

    async fn fixture() -> (String, Arc<Fixture>, tokio::task::JoinHandle<()>) {
        let state = Arc::new(Fixture::default());
        *state.reply.lock().unwrap() = (200, "{}".into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/hook", post(endpoint))
            .route("/redirected", axum::routing::any(|State(state): State<Arc<Fixture>>| async move {
                state.requests.lock().unwrap().push((HeaderMap::new(), Value::Null));
                Json(json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}))
            }))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, state, server)
    }

    fn runner(root: &Path, url: &str, extra: Value, policy: Value) -> HookRunner {
        let mut hook = json!({"type":"http", "url":url, "timeout":1});
        hook.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let mut settings = json!({"hooks":{"PreToolUse":[{"matcher":"Read", "hooks":[hook]}]}});
        settings
            .as_object_mut()
            .unwrap()
            .extend(policy.as_object().unwrap().clone());
        crate::data_layout::write_config_file(
            crate::data_layout::DataLayout::from_root(&root).config_file("settings.json"),
            settings.to_string(),
        )
        .unwrap();
        HookRunner::load(root, root).unwrap()
    }

    async fn run(runner: &HookRunner, session: &str) -> HookOutcome {
        runner.run(HookEvent::PreToolUse, session, json!({"tool_name":"Read", "tool_input":{"file_path":"a.txt"}, "tool_use_id":"call-id"})).await.unwrap()
    }

    #[tokio::test]
    async fn http_payload_headers_decisions_once_and_session_isolation() {
        let (url, state, server) = fixture().await;
        let url = url.replace("127.0.0.1", "localhost");
        let root = tempfile::tempdir().unwrap();
        let variable = format!("CCL_HOOK_TEST_{}", uuid::Uuid::new_v4().simple()).to_uppercase();
        std::env::set_var(&variable, "fixture-token");
        let config = json!({"once":true,"if":"Read", "headers":{"X-Allowed":format!("Bearer ${{{variable}}}"),"X-Denied":"$UNLISTED_FIXTURE_VAR"},"allowedEnvVars":[variable]});
        let runner = runner(
            root.path(),
            &url,
            config,
            json!({"httpHookAllowedEnvVars":[variable]}),
        );
        runner
            .run(HookEvent::PreToolUse, "one", json!({"tool_name":"Write"}))
            .await
            .unwrap();
        assert!(state.requests.lock().unwrap().is_empty());
        *state.reply.lock().unwrap() = (200, json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","updatedInput":{"file_path":"b.txt"}}}).to_string());
        let output = run(&runner, "one").await;
        assert!(output.ask_permission && !output.allow_permission && !output.blocked);
        assert_eq!(output.updated_input, Some(json!({"file_path":"b.txt"})));
        run(&HookRunner::load(root.path(), root.path()).unwrap(), "one").await;
        run(&runner, "two").await;
        let requests = state.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0["x-allowed"], "Bearer fixture-token");
        assert_eq!(requests[0].0["x-denied"], "");
        assert_eq!(
            requests[0].1,
            json!({"session_id":"one","cwd":root.path(),"transcript_path":crate::services::lifecycle_events::transcript(root.path(),"one").unwrap(),"hook_event_name":"PreToolUse","tool_name":"Read","tool_input":{"file_path":"a.txt"},"tool_use_id":"call-id"})
        );
        drop(requests);
        let policy_runner = self::runner(
            root.path(),
            &url,
            json!({"headers":{"X-Allowed":format!("[${variable}]")},"allowedEnvVars":[variable]}),
            json!({"httpHookAllowedEnvVars":[],"allowedHttpHookUrls":["http://localhost:*/hook"]}),
        );
        run(&policy_runner, "three").await;
        assert_eq!(
            state.requests.lock().unwrap().last().unwrap().0["x-allowed"],
            "[]"
        );
        std::env::remove_var(variable);
        server.abort();
    }

    #[tokio::test]
    async fn http_failures_do_not_approve_consume_once_or_follow_redirects() {
        let (url, state, server) = fixture().await;
        let root = tempfile::tempdir().unwrap();
        let runner = runner(root.path(), &url, json!({"once":true}), json!({}));
        for (status, body) in [
            (503, "{}"),
            (302, "{}"),
            (200, "not json"),
            (200, "[]"),
            (200, "{\"async\":true}"),
            (200, "{\"hookSpecificOutput\":{\"hookEventName\":\"Stop\"}}"),
        ] {
            *state.reply.lock().unwrap() = (status, body.into());
            let before = state.requests.lock().unwrap().len();
            let output = run(&runner, "test").await;
            assert!(!output.blocked && !output.allow_permission && !output.ask_permission);
            assert!(!output.warnings.is_empty());
            assert_eq!(state.requests.lock().unwrap().len(), before + 1);
        }
        *state.reply.lock().unwrap() = (200, json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"fixture denied"}}).to_string());
        let output = run(&runner, "test").await;
        assert!(output.blocked);
        assert_eq!(output.reason, "fixture denied");
        assert!(!run(&runner, "test").await.blocked);
        assert_eq!(state.requests.lock().unwrap().len(), 7);
        server.abort();
    }

    #[tokio::test]
    async fn http_empty_success_and_oversized_response_follow_output_contract() {
        let (url, state, server) = fixture().await;
        let root = tempfile::tempdir().unwrap();
        let runner = runner(root.path(), &url, json!({}), json!({}));
        *state.reply.lock().unwrap() = (204, String::new());
        assert!(run(&runner, "empty").await.warnings.is_empty());
        *state.reply.lock().unwrap() = (
            200,
            format!(
                "{{\"systemMessage\":\"{}\"}}",
                "x".repeat(OUTPUT_LIMIT as usize)
            ),
        );
        let output = run(&runner, "large").await;
        assert!(!output.allow_permission && !output.blocked);
        assert!(output.warnings[0].contains("exceeded 1 MiB"));
        server.abort();
    }

    #[tokio::test]
    async fn http_condition_and_network_failure_retry_clear_status() {
        let (url, state, server) = fixture().await;
        let root = tempfile::tempdir().unwrap();
        let filtered = runner(root.path(), &url, json!({"if":"Write"}), json!({}));
        assert!(run(&filtered, "filtered").await.warnings.is_empty());
        assert!(state.requests.lock().unwrap().is_empty());

        let session = uuid::Uuid::new_v4().to_string();
        let progress = super::super::super::hook_progress::subscribe(&session);
        let runner = runner(
            root.path(),
            &url,
            json!({"once":true,"statusMessage":"Checking HTTP"}),
            json!({}),
        );
        // Stop the listener, then restart on the same port: a transport failure
        // must leave the durable once registration eligible for a retry.
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let output = run(&runner, &session).await;
        assert!(!output.allow_permission && !output.ask_permission && !output.blocked);
        assert!(!output.warnings.is_empty());
        assert!(progress.snapshot().is_empty());
        let parsed = Url::parse(&url).unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", parsed.port().unwrap()))
            .await
            .unwrap();
        let app = Router::new()
            .route("/hook", post(endpoint))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        assert!(run(&runner, &session).await.warnings.is_empty());
        assert!(progress.snapshot().is_empty());
        run(&runner, &session).await;
        assert_eq!(state.requests.lock().unwrap().len(), 1);
        server.abort();
    }

    #[test]
    fn http_configuration_rejects_unsupported_options_and_preserves_command_identity() {
        let root = tempfile::tempdir().unwrap();
        for extra in [
            json!({"async":true}),
            json!({"asyncRewake":true}),
            json!({"timeout":0}),
            json!({"timeout":1.5}),
            json!({"url":"http://169.254.169.254/"}),
            json!({"url":"file:///hook"}),
            json!({"url":"http://user:secret@localhost/"}),
            json!({"headers":{"Bad Header":"value"}}),
            json!({"allowedEnvVars":"TOKEN"}),
            json!({"shell":"sh"}),
        ] {
            let mut hook = json!({"type":"http","url":"http://localhost/hook"});
            hook.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            crate::data_layout::write_config_file(
                crate::data_layout::DataLayout::from_root(&root.path())
                    .config_file("settings.json"),
                json!({"hooks":{"PreToolUse":[{"hooks":[hook]}]}}).to_string(),
            )
            .unwrap();
            assert!(
                HookRunner::load(root.path(), root.path()).is_err(),
                "{extra}"
            );
        }
        crate::data_layout::write_config_file(crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"), json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"echo ok","once":true}]}]}}).to_string()).unwrap();
        let runner = HookRunner::load(root.path(), root.path()).unwrap();
        assert_eq!(
            serde_json::to_value(&runner.list()[0]).unwrap(),
            json!({
                "event":"PreToolUse","matcher":"","command":"echo ok","timeout":60,
                "once":true,"source":crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"),"shell":null
            })
        );
    }

    #[tokio::test]
    async fn http_timeout_cancel_and_policy_failures_keep_authority_and_clear_progress() {
        let (url, state, server) = fixture().await;
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let mut progress = super::super::super::hook_progress::subscribe(&session);
        let runner = Arc::new(runner(
            root.path(),
            &url,
            json!({"once":true,"statusMessage":"HTTP fixture"}),
            json!({}),
        ));
        state.delay.store(3000, Ordering::Relaxed);
        let output = run(&runner, &session).await;
        assert!(output.warnings[0].contains("timed out"));
        assert!(progress.snapshot().is_empty());
        let worker = runner.clone();
        let worker_session = session.clone();
        let task = tokio::spawn(async move { run(&worker, &worker_session).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while progress.snapshot().is_empty() {
                progress.changed().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(progress.snapshot().is_empty());
        state.delay.store(0, Ordering::Relaxed);
        assert!(run(&runner, &session).await.warnings.is_empty());
        let blocked = self::runner(
            root.path(),
            &url,
            json!({}),
            json!({"allowedHttpHookUrls":[]}),
        );
        let before = state.requests.lock().unwrap().len();
        assert!(run(&blocked, "other").await.warnings[0].contains("allowedHttpHookUrls"));
        assert_eq!(state.requests.lock().unwrap().len(), before);
        server.abort();
    }

    #[test]
    fn header_interpolation_and_network_targets_are_bounded() {
        assert_eq!(
            interpolate("Bearer ${ALLOWED}/$DENIED\r\n", |name| {
                if name == "ALLOWED" {
                    "ok\r\nX-Evil: 1\0".into()
                } else {
                    String::new()
                }
            }),
            "Bearer okX-Evil: 1/"
        );
        for value in ["127.0.0.1", "::1", "8.8.8.8"] {
            assert!(allowed_ip(value.parse().unwrap()));
        }
        for value in [
            "0.0.0.0",
            "10.0.0.1",
            "169.254.169.254",
            "192.168.1.1",
            "100.64.0.1",
            "224.0.0.1",
            "::",
            "fc00::1",
            "fe80::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(!allowed_ip(value.parse().unwrap()), "{value}");
        }
    }

    #[tokio::test]
    async fn http_and_command_input_updates_preserve_sdk_deny_priority() {
        use crate::services::hook_callbacks::{self, Host};
        struct Deny;
        impl Host for Deny {
            fn invoke<'a>(
                &'a self,
                _: &'a str,
                input: Value,
                _: Option<String>,
            ) -> futures::future::BoxFuture<'a, Result<Value>> {
                Box::pin(async move {
                    assert_eq!(input["tool_input"]["file_path"], "a.txt");
                    Ok(
                        json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"SDK denied"}}),
                    )
                })
            }
        }
        let (url, state, server) = fixture().await;
        *state.reply.lock().unwrap() = (200, json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"file_path":"http.txt"}}}).to_string());
        let root = tempfile::tempdir().unwrap();
        let script = "const fs=require('fs');let s='';process.stdin.on('data',d=>s+=d);process.stdin.on('end',()=>{fs.writeFileSync('command-input.json',s);process.stdout.write(JSON.stringify({hookSpecificOutput:{hookEventName:'PreToolUse',permissionDecision:'ask'}}));});";
        std::fs::write(root.path().join("hook.cjs"), script).unwrap();
        crate::data_layout::write_config_file(crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"), json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"http","url":url},{"type":"command","command":"node hook.cjs"}]}]}}).to_string()).unwrap();
        let output = run(
            &HookRunner::load(root.path(), root.path()).unwrap(),
            "ask-priority",
        )
        .await;
        // The engine checks ask before allow, and deny before either.
        assert!(output.ask_permission && output.allow_permission && !output.blocked);
        let session = uuid::Uuid::new_v4().to_string();
        let host: Arc<dyn Host> = Arc::new(Deny);
        let _guard = hook_callbacks::install(
            &session,
            hook_callbacks::validate(&json!({"PreToolUse":[{"hookCallbackIds":["deny"]}]}))
                .unwrap(),
            host.clone(),
        );
        let runner = HookRunner::load(root.path(), root.path())
            .unwrap()
            .with_session_callbacks(&session);
        let output = run(&runner, &session).await;
        assert!(output.blocked && output.ask_permission && output.allow_permission);
        assert_eq!(output.reason, "SDK denied");
        let command_input: Value = serde_json::from_str(
            &std::fs::read_to_string(root.path().join("command-input.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(command_input["tool_input"]["file_path"], "http.txt");
        server.abort();
    }
}
