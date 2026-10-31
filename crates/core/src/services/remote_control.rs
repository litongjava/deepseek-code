//! Remote Control bridge: registers this frontend as an environment on a
//! configurable server, exposes one session to remote clients, and relays
//! messages and control requests. Only a token configured for that server is
//! sent; Anthropic OAuth credentials are never used here.
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures::{stream::FuturesUnordered, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

pub mod mock;
pub mod peer;

const SETTINGS_FILE: &str = "remote-control.json";
pub const SERVER_ENV: &str = "CLAUDE_BRIDGE_BASE_URL";
pub const TOKEN_ENV: &str = "CLAUDE_BRIDGE_OAUTH_TOKEN";
pub const WEB_ENV: &str = "CLAUDE_BRIDGE_WEB_URL";
const ENVIRONMENTS_BETA: &str = "environments-2025-11-01";
const SESSIONS_BETA: &str = "ccr-byoc-2025-07-29";
const API_VERSION: &str = "2023-06-01";
const FRAME_LIMIT: usize = 4 * 1024 * 1024;

/// Persisted, credential-free server choice.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub server_url: Option<String>,
    pub web_url: Option<String>,
}

pub fn load_settings(data_dir: &Path) -> Result<Settings> {
    match std::fs::read(data_dir.join(SETTINGS_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Invalid remote-control.json"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn save_settings(data_dir: &Path, settings: &Settings) -> Result<()> {
    if let Some(url) = &settings.server_url {
        validate_url(url)?;
    }
    if let Some(url) = &settings.web_url {
        validate_url(url)?;
    }
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(
        data_dir.join(SETTINGS_FILE),
        serde_json::to_vec_pretty(settings)?,
    )?;
    Ok(())
}

/// https anywhere, plain http only on loopback; no embedded credentials.
pub fn validate_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value.trim()).context("Invalid Remote Control URL")?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Remote Control URLs must not contain credentials"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "Remote Control URLs must not contain a query or fragment"
    );
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(&url) => {}
        "http" => bail!("Remote Control requires https except for localhost"),
        scheme => bail!("Unsupported Remote Control URL scheme: {scheme}"),
    }
    ensure!(url.host_str().is_some(), "Remote Control URL needs a host");
    Ok(url)
}

fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn trimmed(url: &reqwest::Url) -> String {
    url.as_str().trim_end_matches('/').to_owned()
}

/// Server, token and session-link base.
#[derive(Clone)]
pub struct Endpoint {
    pub base_url: String,
    pub web_url: String,
    token: Option<String>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .field("web_url", &self.web_url)
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl Endpoint {
    pub fn new(base_url: &str, web_url: Option<&str>, token: Option<String>) -> Result<Self> {
        let base = validate_url(base_url)?;
        let token = token.filter(|token| !token.trim().is_empty());
        ensure!(
            token.is_some() || is_loopback(&base),
            "Remote Control needs a server token in {TOKEN_ENV} for non-local servers"
        );
        if let Some(token) = &token {
            ensure!(
                !token.chars().any(|c| c.is_control() || c.is_whitespace()),
                "Invalid {TOKEN_ENV}"
            );
        }
        let web = match web_url {
            Some(url) => trimmed(&validate_url(url)?),
            None => trimmed(&base),
        };
        Ok(Self {
            base_url: trimmed(&base),
            web_url: web,
            token,
        })
    }

    /// Argument, then saved setting, then environment. There is no implicit
    /// default server: Anthropic OAuth credentials are not reused for this.
    pub fn resolve(data_dir: &Path, server: Option<&str>) -> Result<Self> {
        let settings = load_settings(data_dir)?;
        let base = server
            .map(str::to_owned)
            .or(settings.server_url)
            .or_else(|| std::env::var(SERVER_ENV).ok().filter(|v| !v.trim().is_empty()))
            .with_context(|| format!("Remote Control has no server configured. Use /remote-control --server URL or set {SERVER_ENV}"))?;
        let web = std::env::var(WEB_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or(settings.web_url);
        Self::new(&base, web.as_deref(), std::env::var(TOKEN_ENV).ok())
    }

    pub fn session_url(&self, session_id: &str) -> String {
        format!("{}/code/{session_id}", self.web_url)
    }
}

/// Environment description sent at registration.
#[derive(Debug, Clone, Default)]
pub struct Registration {
    pub machine_name: String,
    pub directory: String,
    pub branch: Option<String>,
    pub git_repo_url: Option<String>,
    pub title: String,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
}

/// Intervals; tests shorten them.
#[derive(Debug, Clone)]
pub struct Timing {
    pub poll: Duration,
    pub keep_alive: Duration,
    pub heartbeat: Duration,
    pub backoff_start: Duration,
    pub backoff_max: Duration,
    pub give_up: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(2000),
            keep_alive: Duration::from_secs(120),
            heartbeat: Duration::from_secs(30),
            backoff_start: Duration::from_secs(2),
            backoff_max: Duration::from_secs(60),
            give_up: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Registered; waiting for the server to dispatch the session.
    Ready,
    Connected,
    Reconnecting(String),
    Failed(String),
    Disconnected,
}

/// Work for the local frontend. Control replies are sent back to the server.
#[derive(Debug)]
pub enum Inbound {
    Prompt(String),
    Control {
        subtype: String,
        request: Value,
        reply: oneshot::Sender<std::result::Result<Value, String>>,
    },
}

#[derive(Debug)]
pub enum Update {
    Inbound(Inbound),
    Status(Status),
    /// The bridge task ended; carries its final status.
    Closed(Status),
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

/// Cloneable handle that lets a running turn ask the remote client for tool
/// approval and observe remote interrupts.
#[derive(Clone)]
pub struct Relay {
    outbound: mpsc::UnboundedSender<Value>,
    session_id: String,
    pending: Pending,
    interrupts: watch::Receiver<u64>,
}

impl Relay {
    /// Sends a `can_use_tool` request; dropping the result before an answer
    /// cancels the remote prompt.
    pub fn request_permission(
        &self,
        request_id: &str,
        tool_name: &str,
        input: &Value,
        tool_use_id: &str,
        description: &str,
    ) -> PermissionRequest {
        let (reply, answer) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(request_id.to_owned(), reply);
        let _ = self.outbound.send(json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {"subtype": "can_use_tool", "tool_name": tool_name, "input": input, "tool_use_id": tool_use_id, "description": description},
            "uuid": uuid::Uuid::new_v4().to_string(),
            "session_id": self.session_id,
        }));
        PermissionRequest {
            request_id: request_id.to_owned(),
            relay: self.clone(),
            answer,
            answered: false,
        }
    }

    /// Interrupts requested after this call.
    pub fn interrupts(&self) -> Interrupts {
        let mut receiver = self.interrupts.clone();
        receiver.borrow_and_update();
        Interrupts(Some(receiver))
    }
}

pub struct PermissionRequest {
    pub request_id: String,
    relay: Relay,
    answer: oneshot::Receiver<Value>,
    answered: bool,
}

impl PermissionRequest {
    /// The remote decision (`true` allows). Pending forever when the remote
    /// client cannot decide, so a local answer still wins. Cancel-safe.
    pub async fn decision(&mut self) -> bool {
        if !self.answered {
            let response = (&mut self.answer).await;
            self.answered = true;
            if let Ok(response) = response {
                if response["subtype"] == "success" {
                    return response["response"]["behavior"] == "allow";
                }
            }
        }
        std::future::pending().await
    }
}

impl Drop for PermissionRequest {
    fn drop(&mut self) {
        if self.answered {
            return;
        }
        self.relay.pending.lock().unwrap().remove(&self.request_id);
        let _ = self.relay.outbound.send(json!({
            "type": "control_cancel_request",
            "request_id": self.request_id,
            "uuid": uuid::Uuid::new_v4().to_string(),
            "session_id": self.relay.session_id,
        }));
    }
}

/// The remote decision for an optional request; pending when there is none.
pub async fn remote_decision(request: &mut Option<PermissionRequest>) -> bool {
    match request.as_mut() {
        Some(request) => request.decision().await,
        None => std::future::pending().await,
    }
}

/// Remote interrupt notifications; never fires without a relay.
#[derive(Default)]
pub struct Interrupts(Option<watch::Receiver<u64>>);

impl Interrupts {
    pub async fn next(&mut self) {
        if let Some(receiver) = self.0.as_mut() {
            if receiver.changed().await.is_ok() {
                return;
            }
        }
        std::future::pending().await
    }
}

#[derive(Debug, Deserialize)]
struct WorkSecret {
    version: u32,
    session_ingress_token: String,
    api_base_url: String,
}

fn decode_secret(secret: &str) -> Result<WorkSecret> {
    let bytes = URL_SAFE_NO_PAD
        .decode(secret.trim_end_matches('='))
        .context("Work secret is not base64url")?;
    let secret: WorkSecret = serde_json::from_slice(&bytes).context("Invalid work secret")?;
    ensure!(
        secret.version == 1,
        "Unsupported work secret version {}",
        secret.version
    );
    ensure!(
        !secret.session_ingress_token.is_empty(),
        "Work secret has no ingress token"
    );
    validate_url(&secret.api_base_url)?;
    Ok(secret)
}

#[derive(Debug)]
enum CallError {
    Fatal(String),
    Transient(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fatal(m) | Self::Transient(m) => f.write_str(m),
        }
    }
}

struct Client {
    http: reqwest::Client,
    endpoint: Endpoint,
}

impl Client {
    fn new(endpoint: Endpoint) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            endpoint,
        })
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        beta: &str,
        bearer: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.endpoint.base_url))
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", beta)
            .header("x-environment-runner-version", env!("CARGO_PKG_VERSION"));
        if let Some(token) = bearer.or(self.endpoint.token.as_deref()) {
            request = request.bearer_auth(token);
        }
        request
    }

    async fn send(
        request: reqwest::RequestBuilder,
        what: &str,
    ) -> std::result::Result<Value, CallError> {
        let response = request
            .send()
            .await
            .map_err(|error| CallError::Transient(format!("{what}: {error}")))?;
        let status = response.status();
        let body = response.bytes().await.unwrap_or_default();
        if status.is_success() {
            return Ok(if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).unwrap_or(Value::Null)
            });
        }
        let detail = String::from_utf8_lossy(&body[..body.len().min(300)]).into_owned();
        let message = format!("{what} failed with HTTP {}: {detail}", status.as_u16());
        Err(if matches!(status.as_u16(), 401 | 403 | 404 | 410) {
            CallError::Fatal(message)
        } else {
            CallError::Transient(message)
        })
    }

    async fn register(
        &self,
        registration: &Registration,
        environment_id: Option<&str>,
    ) -> Result<(String, String)> {
        let mut body = json!({
            "machine_name": registration.machine_name,
            "directory": registration.directory,
            "branch": registration.branch,
            "git_repo_url": registration.git_repo_url,
            "max_sessions": 1,
            "metadata": {"worker_type": "claude_code"},
        });
        if let Some(id) = environment_id {
            body["environment_id"] = json!(id);
        }
        let value = Self::send(
            self.request(
                reqwest::Method::POST,
                "/v1/environments/bridge",
                ENVIRONMENTS_BETA,
                None,
            )
            .json(&body),
            "Environment registration",
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        let id = value["environment_id"]
            .as_str()
            .context("Registration response has no environment_id")?;
        let secret = value["environment_secret"]
            .as_str()
            .context("Registration response has no environment_secret")?;
        Ok((id.to_owned(), secret.to_owned()))
    }

    async fn create_session(
        &self,
        registration: &Registration,
        environment_id: &str,
    ) -> Result<String> {
        let mut body = json!({
            "title": registration.title,
            "events": [],
            "session_context": {"sources": [], "outcomes": [], "model": registration.model},
            "environment_id": environment_id,
            "source": "remote-control",
        });
        if let Some(mode) = &registration.permission_mode {
            body["permission_mode"] = json!(mode);
        }
        let value = Self::send(
            self.request(reqwest::Method::POST, "/v1/sessions", SESSIONS_BETA, None)
                .json(&body),
            "Session creation",
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(value["id"]
            .as_str()
            .context("Session response has no id")?
            .to_owned())
    }

    async fn poll(
        &self,
        environment_id: &str,
        secret: &str,
    ) -> std::result::Result<Option<Value>, CallError> {
        let path =
            format!("/v1/environments/{environment_id}/work/poll?reclaim_older_than_ms=5000");
        let value = Self::send(
            self.request(reqwest::Method::GET, &path, ENVIRONMENTS_BETA, Some(secret)),
            "Work poll",
        )
        .await?;
        Ok((!value.is_null()).then_some(value))
    }

    async fn work(
        &self,
        environment_id: &str,
        work_id: &str,
        action: &str,
        token: &str,
        body: Value,
    ) -> std::result::Result<(), CallError> {
        let path = format!("/v1/environments/{environment_id}/work/{work_id}/{action}");
        Self::send(
            self.request(reqwest::Method::POST, &path, ENVIRONMENTS_BETA, Some(token))
                .json(&body),
            &format!("Work {action}"),
        )
        .await
        .map(|_| ())
    }

    async fn reconnect(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> std::result::Result<(), CallError> {
        let path = format!("/v1/environments/{environment_id}/bridge/reconnect");
        Self::send(
            self.request(reqwest::Method::POST, &path, ENVIRONMENTS_BETA, None)
                .json(&json!({"session_id": session_id})),
            "Session reconnect",
        )
        .await
        .map(|_| ())
    }

    async fn archive(&self, session_id: &str) -> Result<()> {
        let path = format!("/v1/sessions/{session_id}/archive");
        match Self::send(
            self.request(reqwest::Method::POST, &path, SESSIONS_BETA, None)
                .json(&json!({})),
            "Session archive",
        )
        .await
        {
            Ok(_) => Ok(()),
            Err(CallError::Fatal(m) | CallError::Transient(m)) if m.contains("HTTP 409") => Ok(()),
            Err(e) => bail!("{e}"),
        }
    }

    async fn deregister(&self, environment_id: &str) -> Result<()> {
        let path = format!("/v1/environments/bridge/{environment_id}");
        Self::send(
            self.request(reqwest::Method::DELETE, &path, ENVIRONMENTS_BETA, None),
            "Environment deregistration",
        )
        .await
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn post_events(
        &self,
        ingress: &WorkSecret,
        session_id: &str,
        events: Vec<Value>,
    ) -> std::result::Result<(), CallError> {
        let url = format!(
            "{}/v1/session_ingress/session/{session_id}/events",
            ingress.api_base_url.trim_end_matches('/')
        );
        let request = self
            .http
            .post(url)
            .bearer_auth(&ingress.session_ingress_token)
            .header("anthropic-version", API_VERSION)
            .json(&json!({"events": events}));
        Self::send(request, "Event upload").await.map(|_| ())
    }
}

/// A connected Remote Control session owned by one frontend.
pub struct Bridge {
    pub session_id: String,
    pub environment_id: String,
    pub url: String,
    status: watch::Receiver<Status>,
    inbound: mpsc::Receiver<Inbound>,
    outbound: mpsc::UnboundedSender<Value>,
    relay: Relay,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl Bridge {
    /// Registers the environment and creates the session before returning, so
    /// configuration and authorization errors are reported to the caller.
    pub async fn connect(
        endpoint: Endpoint,
        registration: Registration,
        timing: Timing,
    ) -> Result<Self> {
        let url_base = endpoint.clone();
        let client = Client::new(endpoint)?;
        let (environment_id, secret) = client.register(&registration, None).await?;
        let session_id = match client.create_session(&registration, &environment_id).await {
            Ok(id) => id,
            Err(error) => {
                let _ = client.deregister(&environment_id).await;
                return Err(error);
            }
        };
        let (status_tx, status) = watch::channel(Status::Ready);
        let (inbound_tx, inbound) = mpsc::channel(64);
        let (outbound, outbound_rx) = mpsc::unbounded_channel();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (interrupt, interrupts) = watch::channel(0);
        let pending = Pending::default();
        let relay = Relay {
            outbound: outbound.clone(),
            session_id: session_id.clone(),
            pending: pending.clone(),
            interrupts,
        };
        let state = Loop {
            client,
            environment_id: environment_id.clone(),
            secret,
            session_id: session_id.clone(),
            timing,
            status: status_tx,
            inbound: inbound_tx,
            outbound: outbound_rx,
            echoes: HashSet::new(),
            pending,
            interrupt,
        };
        let task = tokio::spawn(state.run(shutdown_rx));
        Ok(Self {
            url: url_base.session_url(&session_id),
            session_id,
            environment_id,
            status,
            inbound,
            outbound,
            relay,
            shutdown: Some(shutdown),
            task,
        })
    }

    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    pub fn relay(&self) -> Relay {
        self.relay.clone()
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.status(), Status::Failed(_) | Status::Disconnected)
    }

    /// Waits for the next status change.
    pub async fn changed(&mut self) -> Status {
        let _ = self.status.changed().await;
        self.status()
    }

    pub async fn next(&mut self) -> Option<Inbound> {
        self.inbound.recv().await
    }

    /// Next inbound item or status change. Cancel-safe.
    pub async fn update(&mut self) -> Update {
        tokio::select! {
            changed = self.status.changed() => match changed {
                Ok(()) => Update::Status(self.status.borrow_and_update().clone()),
                Err(_) => Update::Closed(self.status()),
            },
            inbound = self.inbound.recv() => match inbound {
                Some(inbound) => Update::Inbound(inbound),
                None => Update::Closed(self.status()),
            },
        }
    }

    /// Queues an SDK-format event; held until the session is connected.
    pub fn publish(&self, mut event: Value) {
        if event.get("uuid").is_none() {
            event["uuid"] = json!(uuid::Uuid::new_v4().to_string());
        }
        event["session_id"] = json!(self.session_id);
        let _ = self.outbound.send(event);
    }

    pub fn publish_message(&self, role: &str, text: &str) {
        let content = if role == "assistant" {
            json!([{"type": "text", "text": text}])
        } else {
            json!(text)
        };
        self.publish(json!({"type": role, "message": {"role": role, "content": content}, "parent_tool_use_id": null}));
    }

    /// Stops work, archives the session and deregisters the environment.
    pub async fn disconnect(mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        match tokio::time::timeout(Duration::from_secs(10), &mut self.task).await {
            Ok(result) => result.context("Remote Control task panicked")?,
            Err(_) => {
                self.task.abort();
                bail!("Remote Control teardown timed out")
            }
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if self.shutdown.is_some() {
            self.task.abort();
        }
    }
}

struct Loop {
    client: Client,
    environment_id: String,
    secret: String,
    session_id: String,
    timing: Timing,
    status: watch::Sender<Status>,
    inbound: mpsc::Sender<Inbound>,
    outbound: mpsc::UnboundedReceiver<Value>,
    echoes: HashSet<String>,
    pending: Pending,
    interrupt: watch::Sender<u64>,
}

struct Lease {
    work_id: String,
    ingress: WorkSecret,
}

enum Ended {
    Shutdown,
    Closed(String),
}

impl Loop {
    async fn run(mut self, mut shutdown: oneshot::Receiver<()>) -> Result<()> {
        let mut lease: Option<Lease> = None;
        let mut failure: Option<String> = None;
        let mut backoff = self.timing.backoff_start;
        let mut failing_since: Option<Instant> = None;
        loop {
            let polled = tokio::select! {
                _ = &mut shutdown => break,
                polled = self.client.poll(&self.environment_id, &self.secret) => polled,
            };
            let work = match polled {
                Ok(work) => {
                    backoff = self.timing.backoff_start;
                    failing_since = None;
                    work
                }
                Err(CallError::Fatal(message)) => {
                    failure = Some(message);
                    break;
                }
                Err(CallError::Transient(message)) => {
                    let since = *failing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= self.timing.give_up {
                        failure = Some(format!("Gave up reconnecting: {message}"));
                        break;
                    }
                    let _ = self.status.send(Status::Reconnecting(message));
                    tokio::select! {
                        _ = &mut shutdown => break,
                        _ = tokio::time::sleep(backoff) => {},
                    }
                    backoff = (backoff * 2).min(self.timing.backoff_max);
                    continue;
                }
            };
            let Some(work) = work else {
                if matches!(*self.status.borrow(), Status::Reconnecting(_)) {
                    let _ = self.status.send(Status::Ready);
                }
                tokio::select! {
                    _ = &mut shutdown => break,
                    _ = tokio::time::sleep(self.timing.poll) => {},
                }
                continue;
            };
            let work_id = work["id"].as_str().unwrap_or_default().to_owned();
            let kind = work["data"]["type"].as_str().unwrap_or_default();
            let ingress = match work["secret"]
                .as_str()
                .context("Work has no secret")
                .and_then(decode_secret)
            {
                Ok(secret) => secret,
                Err(error) => {
                    tracing::warn!("Remote Control ignored work {work_id}: {error:#}");
                    continue;
                }
            };
            if let Err(error) = self
                .client
                .work(
                    &self.environment_id,
                    &work_id,
                    "ack",
                    &ingress.session_ingress_token,
                    json!({}),
                )
                .await
            {
                tracing::warn!("Remote Control work ack failed: {error}");
            }
            if kind != "session" || work["data"]["id"].as_str() != Some(self.session_id.as_str()) {
                continue;
            }
            let current = Lease { work_id, ingress };
            match self.ingress(&current, &mut shutdown).await {
                Ended::Shutdown => {
                    lease = Some(current);
                    break;
                }
                Ended::Closed(reason) => {
                    let _ = self
                        .client
                        .work(
                            &self.environment_id,
                            &current.work_id,
                            "stop",
                            &current.ingress.session_ingress_token,
                            json!({"force": false}),
                        )
                        .await;
                    let _ = self.status.send(Status::Reconnecting(reason));
                    if let Err(CallError::Fatal(message)) = self
                        .client
                        .reconnect(&self.environment_id, &self.session_id)
                        .await
                    {
                        failure = Some(message);
                        break;
                    }
                }
            }
        }
        let mut result = Ok(());
        if let Some(lease) = &lease {
            let _ = self.client.post_events(&lease.ingress, &self.session_id, vec![json!({"type": "result", "subtype": "success", "is_error": false, "session_id": self.session_id, "result": ""})]).await;
            let _ = self
                .client
                .work(
                    &self.environment_id,
                    &lease.work_id,
                    "stop",
                    &lease.ingress.session_ingress_token,
                    json!({"force": true}),
                )
                .await;
        }
        if let Err(error) = self.client.archive(&self.session_id).await {
            result = Err(error);
        }
        if let Err(error) = self.client.deregister(&self.environment_id).await {
            result = result.and(Err(error));
        }
        let _ = self.status.send(match failure {
            Some(message) => Status::Failed(message),
            None => Status::Disconnected,
        });
        result
    }

    async fn ingress(&mut self, lease: &Lease, shutdown: &mut oneshot::Receiver<()>) -> Ended {
        let base = lease.ingress.api_base_url.trim_end_matches('/');
        let ws_base = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else {
            format!("ws://{}", base.trim_start_matches("http://"))
        };
        let mut request = match format!("{ws_base}/v1/session_ingress/ws/{}", self.session_id)
            .into_client_request()
        {
            Ok(request) => request,
            Err(error) => return Ended::Closed(format!("Invalid ingress URL: {error}")),
        };
        match format!("Bearer {}", lease.ingress.session_ingress_token).parse() {
            Ok(value) => {
                request.headers_mut().insert("authorization", value);
            }
            Err(_) => return Ended::Closed("Invalid ingress token".into()),
        }
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
            max_message_size: Some(FRAME_LIMIT),
            max_frame_size: Some(FRAME_LIMIT),
            ..Default::default()
        };
        let mut socket = match tokio_tungstenite::connect_async_with_config(
            request,
            Some(config),
            false,
        )
        .await
        {
            Ok((socket, _)) => socket,
            Err(error) => return Ended::Closed(format!("Ingress connection failed: {error}")),
        };
        let _ = self.status.send(Status::Connected);
        let mut keep_alive = tokio::time::interval_at(
            tokio::time::Instant::now() + self.timing.keep_alive,
            self.timing.keep_alive,
        );
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + self.timing.heartbeat,
            self.timing.heartbeat,
        );
        let mut replies = FuturesUnordered::new();
        loop {
            tokio::select! {
                _ = &mut *shutdown => {
                    let _ = socket.close(None).await;
                    return Ended::Shutdown;
                },
                frame = socket.next() => match frame {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                        if let Some(response) = self.handle_frame(value, &mut replies).await {
                            self.upload(lease, vec![response]).await;
                        }
                    },
                    Some(Ok(Message::Ping(payload))) => { let _ = socket.send(Message::Pong(payload)).await; },
                    Some(Ok(Message::Close(_))) | None => return Ended::Closed("Ingress connection closed".into()),
                    Some(Err(error)) => return Ended::Closed(format!("Ingress connection failed: {error}")),
                    Some(Ok(_)) => {},
                },
                Some(event) = self.outbound.recv() => {
                    let mut events = vec![event];
                    while let Ok(event) = self.outbound.try_recv() { events.push(event); }
                    for event in &events {
                        if let Some(id) = event["uuid"].as_str() { self.echoes.insert(id.to_owned()); }
                    }
                    self.upload(lease, events).await;
                },
                Some(response) = replies.next(), if !replies.is_empty() => {
                    self.upload(lease, vec![response]).await;
                },
                _ = keep_alive.tick() => self.upload(lease, vec![json!({"type": "keep_alive"})]).await,
                _ = heartbeat.tick() => {
                    if let Err(error) = self.client.work(&self.environment_id, &lease.work_id, "heartbeat", &lease.ingress.session_ingress_token, json!({})).await {
                        tracing::warn!("Remote Control heartbeat failed: {error}");
                    }
                },
            }
        }
    }

    async fn upload(&self, lease: &Lease, events: Vec<Value>) {
        if let Err(error) = self
            .client
            .post_events(&lease.ingress, &self.session_id, events)
            .await
        {
            tracing::warn!("Remote Control event upload failed: {error}");
        }
    }

    /// Returns an immediate response, or queues one that the frontend answers.
    async fn handle_frame(
        &mut self,
        frame: Value,
        replies: &mut FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>,
        >,
    ) -> Option<Value> {
        if let Some(id) = frame["uuid"].as_str() {
            if self.echoes.remove(id) {
                return None;
            }
        }
        match frame["type"].as_str()? {
            "user" => {
                let text = message_text(&frame["message"]["content"]);
                if !text.trim().is_empty() {
                    let _ = self.inbound.send(Inbound::Prompt(text)).await;
                }
                None
            }
            "control_response" => {
                let response = frame["response"].clone();
                let request_id = response["request_id"].as_str().unwrap_or_default();
                if let Some(reply) = self.pending.lock().unwrap().remove(request_id) {
                    let _ = reply.send(response);
                }
                None
            }
            "control_request" => {
                let request_id = frame["request_id"].as_str().unwrap_or_default().to_owned();
                let request = frame["request"].clone();
                let subtype = request["subtype"].as_str().unwrap_or_default().to_owned();
                if subtype == "initialize" {
                    return Some(control_response(
                        &request_id,
                        Ok(json!({"commands": [], "models": []})),
                    ));
                }
                // Observed by a running turn, if any; idle sessions ignore it.
                if subtype == "interrupt" {
                    self.interrupt.send_modify(|count| *count += 1);
                    return Some(control_response(&request_id, Ok(json!({}))));
                }
                let (reply, answer) = oneshot::channel();
                if self
                    .inbound
                    .send(Inbound::Control {
                        subtype,
                        request,
                        reply,
                    })
                    .await
                    .is_err()
                {
                    return Some(control_response(
                        &request_id,
                        Err("Frontend is not accepting requests".into()),
                    ));
                }
                replies.push(Box::pin(async move {
                    control_response(
                        &request_id,
                        answer
                            .await
                            .unwrap_or_else(|_| Err("Request was dropped".into())),
                    )
                }));
                None
            }
            _ => None,
        }
    }
}

fn control_response(request_id: &str, result: std::result::Result<Value, String>) -> Value {
    match result {
        Ok(response) => {
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": request_id, "response": response}})
        }
        Err(error) => {
            json!({"type": "control_response", "response": {"subtype": "error", "request_id": request_id, "error": error}})
        }
    }
}

fn message_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_require_tls_except_loopback_and_reject_credentials() {
        assert!(validate_url("https://bridge.example.com").is_ok());
        assert!(validate_url("http://127.0.0.1:9000").is_ok());
        assert!(validate_url("http://localhost:9000/").is_ok());
        assert!(validate_url("http://[::1]:9000").is_ok());
        assert!(validate_url("http://bridge.example.com").is_err());
        assert!(validate_url("https://user:pw@bridge.example.com").is_err());
        assert!(validate_url("https://bridge.example.com/?token=x").is_err());
        assert!(validate_url("ftp://bridge.example.com").is_err());
        assert!(Endpoint::new("https://bridge.example.com", None, None).is_err());
        let endpoint = Endpoint::new(
            "https://bridge.example.com/",
            Some("https://web.example.com/"),
            Some("t".into()),
        )
        .unwrap();
        assert_eq!(
            endpoint.session_url("s1"),
            "https://web.example.com/code/s1"
        );
        assert!(!format!("{endpoint:?}").contains("\"t\""));
    }

    #[test]
    fn settings_persist_without_tokens_and_resolve_in_priority_order() {
        let temp = tempfile::tempdir().unwrap();
        assert!(Endpoint::resolve(temp.path(), Some("http://127.0.0.1:1")).is_ok());
        save_settings(
            temp.path(),
            &Settings {
                server_url: Some("http://127.0.0.1:2".into()),
                web_url: None,
            },
        )
        .unwrap();
        let saved = std::fs::read_to_string(temp.path().join(SETTINGS_FILE)).unwrap();
        assert!(!saved.to_lowercase().contains("token"));
        assert_eq!(
            Endpoint::resolve(temp.path(), None).unwrap().base_url,
            "http://127.0.0.1:2"
        );
        assert_eq!(
            Endpoint::resolve(temp.path(), Some("http://127.0.0.1:3"))
                .unwrap()
                .base_url,
            "http://127.0.0.1:3"
        );
        assert!(save_settings(
            temp.path(),
            &Settings {
                server_url: Some("http://example.com".into()),
                web_url: None
            }
        )
        .is_err());
    }

    #[test]
    fn work_secret_is_validated() {
        let encode = |value: Value| URL_SAFE_NO_PAD.encode(value.to_string());
        assert!(decode_secret(&encode(json!({"version": 1, "session_ingress_token": "a", "api_base_url": "http://127.0.0.1:1"}))).is_ok());
        assert!(decode_secret(&encode(json!({"version": 2, "session_ingress_token": "a", "api_base_url": "http://127.0.0.1:1"}))).is_err());
        assert!(decode_secret(&encode(json!({"version": 1, "session_ingress_token": "a", "api_base_url": "http://example.com"}))).is_err());
        assert!(decode_secret("not base64 !").is_err());
    }
}
