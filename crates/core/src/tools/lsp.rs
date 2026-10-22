//! Bounded, reusable LSP stdio sessions shared by CLI and Desktop.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_SERVERS: usize = 8;
#[cfg(test)]
#[path = "lsp_lifecycle_tests.rs"]
mod lifecycle_tests;

#[derive(Debug)]
struct Disconnected;
impl std::fmt::Display for Disconnected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { formatter.write_str("Language server closed stdout") }
}
impl std::error::Error for Disconnected {}

struct Server {
    child: tokio::process::Child,
    _process_tree: crate::services::tool_hooks::ProcessTree,
    writer: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    state: ClientState,
    capabilities: Value,
    documents: HashMap<String, (u64, i64)>,
}

type Pool = HashMap<String, (Instant, Server)>;
#[derive(Default)]
struct WorkspaceConfiguration {
    generation: u64,
    servers: Vec<Value>,
}
#[derive(Default)]
struct Runtime {
    pool: tokio::sync::Mutex<Pool>,
    configurations: std::sync::Mutex<HashMap<String, WorkspaceConfiguration>>,
    recovery: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<u64>>>>,
}
fn runtime() -> &'static Arc<Runtime> {
    static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
    RUNTIME.get_or_init(Default::default)
}
fn pool() -> &'static tokio::sync::Mutex<Pool> {
    &runtime().pool
}

pub(crate) fn validate_configuration(server: &Value) -> Result<()> {
    let fields = server.as_object().context("LSP server configuration must be an object")?;
    for field in fields.keys() {
        if !matches!(field.as_str(), "name" | "command" | "args" | "extensionToLanguage" | "transport" | "env" | "initializationOptions" | "settings" | "workspaceFolder" | "startupTimeout" | "maxRestarts" | "shutdownTimeout" | "restartOnCrash") { bail!("Unknown LSP configuration field: {field}"); }
    }
    if server["command"].as_str().is_none_or(|s| s.trim().is_empty()) {
        bail!("LSP server configuration needs a command");
    }
    if !server["extensionToLanguage"].as_object().is_some_and(|entries| !entries.is_empty() && entries.iter().all(|(extension, language)| extension.starts_with('.') && extension.len() > 1 && !extension.contains(['/', '\\', ' ']) && language.as_str().is_some_and(|language| !language.trim().is_empty()))) {
        bail!("LSP extensionToLanguage needs nonempty .extension to language mappings");
    }
    if server.get("transport").is_some_and(|value| value.as_str() != Some("stdio")) {
        bail!("Only stdio LSP server transport is supported");
    }
    if server.get("args").is_some_and(|value| !value.as_array().is_some_and(|args| args.iter().all(|arg| arg.as_str().is_some_and(|arg| !arg.is_empty())))) {
        bail!("LSP args must contain nonempty strings");
    }
    if server.get("env").is_some_and(|value| !value.as_object().is_some_and(|env| env.values().all(Value::is_string))) {
        bail!("LSP env must contain strings");
    }
    if server.get("workspaceFolder").is_some_and(|value| !value.is_string()) {
        bail!("LSP workspaceFolder must be a string");
    }
    lifecycle_options(server)?;
    Ok(())
}

pub fn configure(workspace: &str, servers: Vec<Value>) -> Result<()> {
    configure_in(runtime(), workspace, servers)
}
fn configure_in(runtime: &Arc<Runtime>, workspace: &str, servers: Vec<Value>) -> Result<()> {
    for server in &servers {
        validate_configuration(server)?;
    }
    let workspace = std::path::Path::new(workspace)
        .canonicalize()?
        .to_string_lossy()
        .into_owned();
    let mut configurations = runtime.configurations.lock().map_err(|_| anyhow::anyhow!("LSP configuration lock poisoned"))?;
    let current = configurations.entry(workspace.clone()).or_default();
    if current.servers == servers { return Ok(()); }
    current.generation = current.generation.checked_add(1).context("LSP configuration generation exhausted")?;
    current.servers = servers;
    runtime.recovery.lock().map_err(|_| anyhow::anyhow!("LSP recovery lock poisoned"))?.retain(|key, _| !identity_workspace(key, &workspace));
    // This synchronous API never waits for the async pool. In-flight or busy
    // entries are rejected by their generation before reuse/recycling.
    if let Ok(mut pool) = runtime.pool.try_lock() {
        pool.retain(|key, _| !identity_workspace(key, &workspace));
    }
    Ok(())
}
fn identity_workspace(key: &str, workspace: &str) -> bool {
    serde_json::from_str::<Value>(key).is_ok_and(|identity| identity["root"] == workspace)
}
fn current_generation(runtime: &Runtime, key: &str) -> bool {
    let Ok(identity) = serde_json::from_str::<Value>(key) else { return false };
    let Ok(configs) = runtime.configurations.lock() else { return false };
    let generation = configs.get(identity["root"].as_str().unwrap_or("")).map_or(0, |config| config.generation);
    identity["generation"].as_u64().unwrap_or(0) == generation
}

fn lifecycle_options(input: &Value) -> Result<(Option<Duration>, u64)> {
    for unsupported in ["restartOnCrash", "shutdownTimeout"] {
        if input.get(unsupported).is_some() { bail!("LSP {unsupported} is not implemented; remove this field from the configuration"); }
    }
    let startup = input.get("startupTimeout").map(|value| value.as_u64().filter(|value| *value > 0).context("LSP startupTimeout must be a positive integer in milliseconds")).transpose()?;
    let restarts = input.get("maxRestarts").map(|value| value.as_u64().context("LSP maxRestarts must be a nonnegative integer")).transpose()?.unwrap_or(3);
    let startup = startup.map(Duration::from_millis);
    if startup.is_some_and(|duration| tokio::time::Instant::now().checked_add(duration).is_none()) { bail!("LSP startupTimeout exceeds the platform clock range"); }
    Ok((startup, restarts))
}

/// Display-only metadata; environment variables and initialization settings are
/// deliberately excluded because plugin configurations may contain credentials.
pub fn configured(workspace: &str) -> Result<Vec<Value>> {
    let root = std::path::Path::new(workspace)
        .canonicalize()?
        .to_string_lossy()
        .into_owned();
    let configs = runtime().configurations
        .lock()
        .map_err(|_| anyhow::anyhow!("LSP configuration lock poisoned"))?;
    Ok(configs.get(&root).into_iter().flat_map(|config| &config.servers).map(|server|json!({"name":server["name"],"command":server["command"],"extensionToLanguage":server["extensionToLanguage"],"startupTimeout":server["startupTimeout"],"maxRestarts":server.get("maxRestarts").cloned().unwrap_or(json!(3))})).collect())
}

pub async fn status(workspace: &str) -> Result<Value> {
    let root = std::path::Path::new(workspace).canonicalize()?;
    let configured = configured(workspace)?;
    let mut active = Vec::new();
    let mut pool = pool().lock().await;
    pool.retain(|key, _| current_generation(runtime(), key));
    for (key, (last_used, server)) in pool.iter_mut() {
        let key: Value = serde_json::from_str(key)?;
        if key["root"] != json!(root) {
            continue;
        }
        let exited = server.child.try_wait()?.is_some();
        active.push(json!({"command":key["server"],"pid":server.child.id(),"state":if exited {"exited"} else {"ready"},"documents":server.documents.len(),"idle_seconds":last_used.elapsed().as_secs()}));
    }
    Ok(
        json!({"configured":configured,"servers":active,"idle_timeout_seconds":IDLE_TIMEOUT.as_secs()}),
    )
}

fn apply_configuration(runtime: &Runtime, input: &mut Value, cwd: &str) -> Result<()> {
    let extension = std::path::Path::new(input["path"].as_str().unwrap_or(""))
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let extension = format!(".{extension}");
    let root = std::path::Path::new(cwd)
        .canonicalize()?
        .to_string_lossy()
        .into_owned();
    let configs = runtime.configurations
        .lock()
        .map_err(|_| anyhow::anyhow!("LSP configuration lock poisoned"))?;
    input["_lsp_generation"] = json!(configs.get(&root).map_or(0, |config| config.generation));
    if input.get("server").is_some() { return Ok(()); }
    if let Some(server) = configs.get(&root).and_then(|config| {
        config.servers
            .iter()
            .find(|server| server["extensionToLanguage"][&extension].is_string())
    }) {
        for (source, target) in [
            ("command", "server"),
            ("args", "args"),
            ("env", "env"),
            ("initializationOptions", "initialization_options"),
            ("settings", "settings"),
            ("workspaceFolder", "workspace_folder"),
            ("startupTimeout", "startupTimeout"),
            ("maxRestarts", "maxRestarts"),
        ] {
            if input.get(target).is_none() {
                if let Some(value) = server.get(source) {
                    input[target] = value.clone();
                }
            }
        }
        if input.get("language_id").is_none() {
            input["language_id"] = server["extensionToLanguage"][extension].clone();
        }
    }
    Ok(())
}

impl Server {
    async fn shutdown(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            for uri in self.documents.keys() {
                send(&mut self.writer, &json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":uri}}})).await?;
            }
            send(&mut self.writer, &json!({"jsonrpc":"2.0","id":999999,"method":"shutdown","params":null})).await?;
            response(&mut self.reader,&mut self.writer,999999,&mut self.state).await?;
            send(&mut self.writer, &json!({"jsonrpc":"2.0","method":"exit"})).await?;
            self.child.wait().await?;
            Ok::<_,anyhow::Error>(())
        }).await;
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// Graceful application shutdown. In-flight requests are bounded and own their
/// subprocesses; dropping a cancelled request kills its server as well.
pub async fn shutdown_all() {
    shutdown_in(runtime()).await;
}
async fn shutdown_in(runtime: &Arc<Runtime>) {
    let servers = std::mem::take(&mut *runtime.pool.lock().await);
    for (_, (_, mut server)) in servers {
        server.shutdown().await;
    }
}

/// Best-effort feedback only from a previously approved, running server which
/// already owns this document. Never starts a process or opens unrelated files.
pub async fn passive_feedback(cwd: &str, file: &str) -> Option<Value> {
    // Most file operations have no LSP session. Avoid even reading the file then.
    if pool().try_lock().ok()?.is_empty() {
        return None;
    }
    let root = std::path::Path::new(cwd).canonicalize().ok()?;
    let path = std::path::PathBuf::from(super::resolve_path(file, cwd).ok()?)
        .canonicalize()
        .ok()?;
    let uri = reqwest::Url::from_file_path(&path).ok()?.to_string();
    let (key, mut server) = {
        let mut pool = pool().try_lock().ok()?;
        let key = pool.iter().find_map(|(key, (used, server))| {
            let identity: Value = serde_json::from_str(key).ok()?;
            (current_generation(runtime(), key) && identity["root"] == json!(root)
                && used.elapsed() < IDLE_TIMEOUT
                && server.documents.contains_key(&uri))
            .then(|| key.clone())
        })?;
        let (_, server) = pool.remove(&key)?;
        (key, server)
    };
    if server.child.try_wait().ok()?.is_some() {
        record_passive_failure(&key);
        return None;
    }
    let outcome = tokio::time::timeout(Duration::from_millis(150), async {
        if tokio::fs::metadata(&path).await?.len() > 8 * 1024 * 1024 { bail!("Document too large"); }
        let text = tokio::fs::read_to_string(&path).await?;
        if text.len() > 8 * 1024 * 1024 { bail!("Document too large"); }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hash);
        let hash = hash.finish();
        let (previous, version) = server.documents[&uri];
        if hash != previous {
            server.state.diagnostics.remove(&uri);
            server.state.document_versions.insert(uri.clone(), version + 1);
            // Sending is separately bounded; on a partial write discard the
            // server instead of reusing a corrupted JSON-RPC stream.
            tokio::time::timeout(Duration::from_millis(40), send(&mut server.writer, &json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":uri,"version":version+1},"contentChanges":[{"text":text}]}}))).await.context("Passive synchronization timed out")??;
            server.documents.insert(uri.clone(), (hash, version + 1));
        }
        let Server {reader, writer, state, ..} = &mut server;
        // Only the receive future times out normally. Its pending frame buffer
        // is retained. No diagnostics is not an error or a request to restart.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(if hash != previous {75} else {2});
        for _ in 0..64 {
            let message = match tokio::time::timeout_at(deadline, receive_buffered(reader, &mut state.pending_frame)).await {
                Ok(message) => message?,
                Err(_) => break,
            };
            state.handle(writer, &message).await?;
            if state.diagnostics.contains_key(&uri) { break; }
        }
        let Some(current) = state.diagnostics.get(&uri).cloned() else { return Ok(None); };
        let previous = state.reported_diagnostics.get(&uri).cloned().unwrap_or(json!([]));
        state.reported_diagnostics.insert(uri.clone(), current.clone());
        let old = previous.as_array().cloned().unwrap_or_default();
        let new = current.as_array().cloned().unwrap_or_default();
        let additions: Vec<Value> = new.iter().filter(|item| !old.contains(item)).take(20).map(|item| {
            let message: String = item["message"].as_str().unwrap_or("").chars().take(1000).collect();
            json!({"message":message,"severity":item["severity"].as_u64(),"range":{"start":{"line":item["range"]["start"]["line"].as_u64(),"character":item["range"]["start"]["character"].as_u64()},"end":{"line":item["range"]["end"]["line"].as_u64(),"character":item["range"]["end"]["character"].as_u64()}}})
        }).collect();
        if additions.is_empty() && !(new.is_empty() && !old.is_empty()) { return Ok(None); }
        Ok::<_, anyhow::Error>(Some(json!({"path":path,"source":"LSP","diagnostics":additions,"cleared":new.is_empty(),"total":new.len(),"position_encoding":"utf-16","position_base":0})))
    }).await;
    match outcome {
        Ok(Ok(value)) => {
            recycle(runtime().clone(), key, server).await;
            value
        }
        // A cancelled caller drops the server and its process tree immediately.
        // Errors must not invalidate the already successful Read/Edit/Write.
        Ok(Err(error)) => {
            if error.is::<Disconnected>() || error.is::<std::io::Error>() { record_passive_failure(&key); }
            None
        }
        _ => None,
    }
}

fn record_passive_failure(key: &str) {
    let recovery = runtime().recovery.lock().ok().and_then(|entries| entries.get(key).cloned());
    if let Some(recovery) = recovery {
        if let Ok(mut failures) = recovery.try_lock() { *failures = failures.saturating_add(1); }
    }
}

async fn recycle(runtime: Arc<Runtime>, key: String, server: Server) {
    let inserted = Instant::now();
    let evicted = {
        let mut pool = runtime.pool.lock().await;
        if !current_generation(&runtime, &key) { return; }
        let old = if pool.len() >= MAX_SERVERS {
            let oldest = pool
                .iter()
                .min_by_key(|(_, (time, _))| *time)
                .map(|(key, _)| key.clone());
            oldest
                .and_then(|key| pool.remove(&key))
                .map(|(_, server)| server)
        } else {
            None
        };
        // Concurrent requests may temporarily use separate servers. Replace and
        // close the previous idle instance instead of leaking its process.
        let replaced = pool
            .insert(key.clone(), (inserted, server))
            .map(|(_, server)| server);
        (old, replaced)
    };
    for mut server in [evicted.0, evicted.1].into_iter().flatten() {
        server.shutdown().await;
    }
    tokio::spawn(async move {
        tokio::time::sleep(IDLE_TIMEOUT).await;
        let expired = {
            let mut pool = runtime.pool.lock().await;
            if pool.get(&key).is_some_and(|(time, _)| *time == inserted) {
                pool.remove(&key)
            } else {
                None
            }
        };
        if let Some((_, mut server)) = expired {
            server.shutdown().await;
        }
    });
}

pub(super) fn definition() -> super::ToolDefinition {
    super::ToolDefinition { name:"LSP".into(), description:"Query an installed language server over stdio: definitions, references, hover, symbols, implementations, call hierarchy and diagnostics. Launching the server requires execution approval. Use filePath with one-based line/character (UTF-16), or legacy path with zero-based positions. Server and language_id are inferred for common extensions; optional server/args override detection. Missing servers and unsupported operations return errors.".into(), input_schema:json!({"type":"object","properties":{
        "server":{"type":"string","description":"Executable, e.g. rust-analyzer, pylsp or typescript-language-server"},"args":{"type":"array","items":{"type":"string"}},"filePath":{"type":"string"},"path":{"type":"string","description":"Legacy path; positions are zero-based unless position_base is set"},"language_id":{"type":"string"},"operation":{"type":"string","enum":["goToDefinition","findReferences","hover","documentSymbol","workspaceSymbol","goToImplementation","prepareCallHierarchy","incomingCalls","outgoingCalls","definition","references","document_symbols","diagnostics"]},"line":{"type":"integer","minimum":0},"character":{"type":"integer","minimum":0},"position_base":{"type":"integer","enum":[0,1]},"query":{"type":"string","description":"Workspace symbol search"},"include_declaration":{"type":"boolean"},"initialization_options":{"type":"object"},"settings":{"type":"object"},"env":{"type":"object","additionalProperties":{"type":"string"}},"reuse":{"type":"boolean","description":"Reuse initialized servers (default true)"},"timeout_secs":{"type":"integer","minimum":1,"maximum":120}
    },"required":["operation"],"anyOf":[{"required":["filePath"]},{"required":["path"]}]}) }
}

#[derive(Default)]
struct ClientState {
    pending_frame: Vec<u8>,
    reported_diagnostics: HashMap<String, Value>,
    diagnostics: std::collections::HashMap<String, Value>,
    document_versions: HashMap<String, i64>,
    settings: Value,
    workspace_folders: Value,
}

impl ClientState {
    async fn handle(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        message: &Value,
    ) -> Result<()> {
        let method = message["method"].as_str().unwrap_or("");
        if method == "textDocument/publishDiagnostics" {
            if let Some(uri) = message["params"]["uri"].as_str() {
                if let Some(current_version) = self.document_versions.get(uri) {
                    if message["params"]["version"]
                        .as_i64()
                        .is_none_or(|version| version >= *current_version)
                    {
                        self.diagnostics
                            .insert(uri.into(), message["params"]["diagnostics"].clone());
                    }
                }
            }
        }
        if let Some(id) = message.get("id") {
            let result = match method {
                "workspace/configuration" => Some(Value::Array(
                    message["params"]["items"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| {
                                    let mut value = &self.settings;
                                    if let Some(section) = item["section"].as_str() {
                                        for key in section.split('.') {
                                            value = &value[key];
                                        }
                                    }
                                    value.clone()
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                )),
                "workspace/workspaceFolders" => Some(self.workspace_folders.clone()),
                "client/registerCapability"
                | "client/unregisterCapability"
                | "window/workDoneProgress/create" => Some(Value::Null),
                "workspace/applyEdit" => {
                    Some(json!({"applied":false,"failureReason":"LSP queries are read-only"}))
                }
                _ => None,
            };
            let reply = match result {
                Some(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                None => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Client operation is not supported"}})
                }
            };
            send(writer, &reply).await?;
        }
        Ok(())
    }
}

async fn send(writer: &mut (impl AsyncWrite + Unpin), value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
async fn receive(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Value> {
    receive_buffered(reader, &mut Vec::new()).await
}

// Partial headers and bodies live outside the future, so a passive deadline can
// cancel a read without losing framing for the next explicit LSP request.
async fn receive_buffered(
    reader: &mut (impl AsyncBufRead + Unpin),
    pending: &mut Vec<u8>,
) -> Result<Value> {
    loop {
        if let Some(end) = pending.windows(4).position(|part| part == b"\r\n\r\n") {
            if end > 8192 {
                bail!("LSP headers exceed 8 KiB");
            }
            let header = std::str::from_utf8(&pending[..end])?;
            let length = header
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("Content-Length"))
                        .map(|(_, value)| value.trim().parse::<usize>())
                })
                .context("LSP Content-Length is missing")??;
            if length > 8 * 1024 * 1024 {
                bail!("LSP message exceeds 8 MiB");
            }
            if pending.len() >= end + 4 + length {
                let value = serde_json::from_slice(&pending[end + 4..end + 4 + length])?;
                pending.drain(..end + 4 + length);
                return Ok(value);
            }
        } else if pending.len() > 8192 {
            bail!("LSP headers exceed 8 KiB");
        }
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(Disconnected.into());
        }
        let count = available.len().min(8192);
        pending.extend_from_slice(&available[..count]);
        reader.consume(count);
    }
}

async fn response(
    reader: &mut (impl AsyncBufRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    id: u64,
    state: &mut ClientState,
) -> Result<Value> {
    loop {
        let message = receive_buffered(reader, &mut state.pending_frame).await?;
        if message.get("method").is_some() {
            state.handle(writer, &message).await?;
            continue;
        }
        if message["id"] == id {
            if let Some(error) = message.get("error") {
                bail!("Language server error: {error}");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

pub(super) async fn execute(input: Value, cwd: &str) -> Result<Value> {
    execute_in(runtime(), input, cwd).await
}
async fn execute_in(runtime: &Arc<Runtime>, mut input: Value, cwd: &str) -> Result<Value> {
    let position_base = input
        .get("position_base")
        .and_then(Value::as_u64)
        .unwrap_or(if input.get("filePath").is_some() {
            1
        } else {
            0
        });
    if position_base > 1 {
        bail!("position_base must be 0 or 1");
    }
    if input.get("path").is_none() {
        input["path"] = input["filePath"].clone();
    }
    apply_configuration(runtime, &mut input, cwd)?;
    lifecycle_options(&input)?;
    if input.get("operation").is_none() {
        let action = input["action"]
            .as_str()
            .context("operation or legacy action is required")?;
        input["operation"] = json!(if action == "symbols" {
            "document_symbols"
        } else {
            action
        });
    }
    if input.get("server").is_none() || input.get("language_id").is_none() {
        let extension = std::path::Path::new(input["path"].as_str().unwrap_or(""))
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let (language, language_id) = super::lsp_servers::for_extension(extension)
            .with_context(|| format!("No default language server for .{extension} files; specify server and language_id"))?;
        if input.get("server").is_none() {
            let workspace = std::path::Path::new(cwd);
            let launch = super::lsp_servers::locate(language, workspace)
                .with_context(|| super::lsp_servers::missing_message(language))?;
            input["server"] = json!(launch.command.to_string_lossy());
            if input.get("args").is_none() {
                input["args"] = json!(launch.args);
            }
            if input.get("initialization_options").is_none() {
                if let Some(options) = super::lsp_servers::initialization_options(language) {
                    input["initialization_options"] = options;
                }
            }
        }
        if input.get("language_id").is_none() {
            input["language_id"] = json!(language_id);
        }
    }
    for field in ["line", "character"] {
        let value = input
            .get(field)
            .map(|value| {
                value
                    .as_u64()
                    .context("Positions must be nonnegative integers")
            })
            .transpose()?
            .unwrap_or(position_base);
        input[field] = json!(value
            .checked_sub(position_base)
            .context("One-based positions must be at least 1")?);
    }
    let seconds = input["timeout_secs"].as_u64().unwrap_or(30);
    if !(1..=120).contains(&seconds) {
        bail!("LSP timeout must be 1–120 seconds");
    }
    tokio::time::timeout(std::time::Duration::from_secs(seconds), session(runtime, input, cwd))
        .await
        .context("Language server request timed out (total timeout_secs deadline)")?
}

async fn session(runtime: &Arc<Runtime>, input: Value, cwd: &str) -> Result<Value> {
    let executable = input["server"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("server is required")?;
    let path = std::path::PathBuf::from(super::resolve_path(
        input["path"].as_str().context("path is required")?,
        cwd,
    )?)
    .canonicalize()?;
    if tokio::fs::metadata(&path).await?.len() > 8 * 1024 * 1024 {
        bail!("LSP document exceeds 8 MiB");
    }
    let bytes = tokio::fs::read(&path).await?;
    if bytes.len() > 8 * 1024 * 1024 {
        bail!("LSP document exceeds 8 MiB");
    }
    let text = String::from_utf8(bytes).context("LSP document must be UTF-8")?;
    let uri =
        reqwest::Url::from_file_path(&path).map_err(|_| anyhow::anyhow!("Invalid document URI"))?;
    let root = std::path::Path::new(cwd).canonicalize()?;
    let root_uri = reqwest::Url::from_directory_path(&root)
        .map_err(|_| anyhow::anyhow!("Invalid workspace URI"))?;
    let method = match input["operation"]
        .as_str()
        .context("operation is required")?
    {
        "hover" => "textDocument/hover",
        "definition" | "goToDefinition" => "textDocument/definition",
        "references" | "findReferences" => "textDocument/references",
        "document_symbols" | "documentSymbol" => "textDocument/documentSymbol",
        "workspaceSymbol" => "workspace/symbol",
        "goToImplementation" => "textDocument/implementation",
        "prepareCallHierarchy" | "incomingCalls" | "outgoingCalls" => {
            "textDocument/prepareCallHierarchy"
        }
        "diagnostics" => "textDocument/diagnostic",
        _ => bail!("Unknown LSP operation"),
    };
    let key = serde_json::to_string(
        &json!({"root":root,"server":executable,"args":input["args"],"env":input["env"],"settings":input["settings"],"initialization_options":input["initialization_options"],"workspace_folder":input["workspace_folder"],"startupTimeout":input["startupTimeout"],"maxRestarts":input["maxRestarts"],"generation":input["_lsp_generation"]}),
    )?;
    let recovery = runtime.recovery.lock().map_err(|_| anyhow::anyhow!("LSP recovery lock poisoned"))?.entry(key.clone()).or_default().clone();
    let mut failures = recovery.lock().await;
    if !current_generation(runtime, &key) { bail!("LSP configuration changed before this request started; retry the request"); }
    let cached = {
        let mut pool = runtime.pool.lock().await;
        pool.retain(|key, _| current_generation(runtime, key));
        pool.remove(&key).map(|(_, server)| server)
    };
    let (_, max_restarts) = lifecycle_options(&input)?;
    let mut server = match cached {
        Some(mut server) => {
            if server.child.try_wait()?.is_none() {
                server
            } else {
                *failures = failures.saturating_add(1);
                start_with_recovery(&input, &root, &root_uri, &mut failures, max_restarts).await?
            }
        }
        _ => start_with_recovery(&input, &root, &root_uri, &mut failures, max_restarts).await?,
    };
    let result=async {
        let Server { writer, reader, state, capabilities, documents, .. } = &mut server;
        let mut hash=std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hash);
        let hash=hash.finish();
        match documents.get(uri.as_str()).copied() {
            Some((previous,version)) if previous != hash => {
                state.diagnostics.remove(uri.as_str());
                state.document_versions.insert(uri.to_string(),version+1);
                send(writer,&json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":uri.as_str(),"version":version+1},"contentChanges":[{"text":text}]}})).await?;
                documents.insert(uri.to_string(),(hash,version+1));
            }
            None => {
                if documents.len()>=64 {
                    if let Some(old)=documents.keys().next().cloned() {
                        send(writer,&json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":old}}})).await?;
                        documents.remove(&old);
                        state.diagnostics.remove(&old);
                        state.reported_diagnostics.remove(&old);
                        state.document_versions.remove(&old);
                    }
                }
                state.document_versions.insert(uri.to_string(),1);
                send(writer,&json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri.as_str(),"languageId":input["language_id"].as_str().context("language_id is required")?,"version":1,"text":text}}})).await?;
                documents.insert(uri.to_string(),(hash,1));
            }
            _ => {}
        }
        let mut params=json!({"textDocument":{"uri":uri.as_str()}});
        if !matches!(method,"textDocument/documentSymbol"|"textDocument/diagnostic"|"workspace/symbol") { params["position"]=json!({"line":input["line"],"character":input["character"]}); }
        if method=="textDocument/references" { params["context"]=json!({"includeDeclaration":input.get("include_declaration").and_then(Value::as_bool).unwrap_or(true)}); }
        if method=="workspace/symbol" { params=json!({"query":input.get("query").and_then(Value::as_str).unwrap_or("")}); }
        let mut value;
        if method=="textDocument/diagnostic" && !capabilities.get("diagnosticProvider").is_some_and(|value| value.is_object() || value == &Value::Bool(true)) {
            // Older servers publish diagnostics asynchronously rather than implement pull diagnostics.
            loop {
                if let Some(items) = state.diagnostics.get(uri.as_str()).cloned() {
                    value=json!({"kind":"full","items":items});
                    break;
                }
                let message=receive_buffered(reader, &mut state.pending_frame).await?;
                state.handle(writer,&message).await?;
            }
        } else {
            send(writer,&json!({"jsonrpc":"2.0","id":2,"method":method,"params":params})).await?;
            value=response(reader,writer,2,state).await?;
        }
        if matches!(input["operation"].as_str(),Some("incomingCalls"|"outgoingCalls")) {
            let mut calls=Vec::new();
            let items = value.as_array().cloned().unwrap_or_default();
            let calls_method=if input["operation"]=="incomingCalls" { "callHierarchy/incomingCalls" } else { "callHierarchy/outgoingCalls" };
            for (index,item) in items.into_iter().enumerate() {
                let id=10+index as u64;
                send(writer,&json!({"jsonrpc":"2.0","id":id,"method":calls_method,"params":{"item":item}})).await?;
                let result=response(reader,writer,id,state).await?;
                if let Some(results)=result.as_array() { calls.extend(results.iter().cloned()); }
            }
            value=Value::Array(calls);
        }
        Ok::<_,anyhow::Error>(json!({"path":path,"operation":input["operation"],"result":value}))
    }.await;
    if result.is_ok() && input.get("reuse").and_then(Value::as_bool).unwrap_or(true) {
        recycle(runtime.clone(), key, server).await;
    } else {
        if result.as_ref().err().is_some_and(|error| error.is::<Disconnected>() || error.is::<std::io::Error>()) || (result.is_err() && server.child.try_wait()?.is_some()) { *failures = failures.saturating_add(1); }
        server.shutdown().await;
    }
    result
}

async fn start_with_recovery(input: &Value, root: &std::path::Path, root_uri: &reqwest::Url, failures: &mut u64, max_restarts: u64) -> Result<Server> {
    if *failures > max_restarts {
        bail!("Language server exceeded maxRestarts ({max_restarts}) after {failures} consecutive failed starts/crashes; change its configuration or restart the application before retrying");
    }
    match start_server(input, root, root_uri).await {
        Ok(server) => { *failures = 0; Ok(server) }
        Err(error) => { *failures = failures.saturating_add(1); Err(error) }
    }
}

async fn start_server(
    input: &Value,
    root: &std::path::Path,
    root_uri: &reqwest::Url,
) -> Result<Server> {
    let workspace_override = input.get("workspace_folder").map(|value| -> Result<std::path::PathBuf> {
        let path = std::path::Path::new(value.as_str().context("LSP workspaceFolder must be a string")?);
        Ok(if path.is_absolute() { path.to_path_buf() } else { root.join(path) }.canonicalize()?)
    }).transpose()?;
    let workspace_uri = workspace_override.as_ref().map(|path| reqwest::Url::from_directory_path(path).map_err(|_| anyhow::anyhow!("Invalid LSP workspaceFolder URI"))).transpose()?;
    let root = workspace_override.as_deref().unwrap_or(root);
    let root_uri = workspace_uri.as_ref().unwrap_or(root_uri);
    let requested = input["server"].as_str().context("server is required")?;
    // A known server missing from PATH may still be a managed install.
    let resolved = super::lsp_servers::resolve_command(requested, root);
    let executable = resolved.as_ref().map_or(std::path::Path::new(requested), |launch| launch.command.as_path());
    let mut command = tokio::process::Command::new(executable);
    if let Some(launch) = &resolved {
        command.args(&launch.args);
    }
    if let Some(env) = input.get("env") {
        for (key, value) in env.as_object().context("env must be an object")? {
            command.env(key, value.as_str().context("env values must be strings")?);
        }
    }
    if let Some(args) = input.get("args") {
        for arg in args.as_array().context("args must be an array")? {
            command.arg(arg.as_str().context("args must contain strings")?);
        }
    }
    command
        .current_dir(&root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let known = super::lsp_servers::LANGUAGES.iter().find(|language| language.command == requested);
            bail!(
                "{}",
                known.map(super::lsp_servers::missing_message).unwrap_or_else(|| format!(
                    "Cannot start language server '{requested}': it was not found. Install it, or configure the full path to its executable"
                ))
            );
        }
        Err(error) => return Err(anyhow::Error::new(error).context("Cannot start language server")),
    };
    let process_tree = crate::services::tool_hooks::ProcessTree::new(
        child.id().context("Missing language server process ID")?,
    )?;
    let mut writer = child.stdin.take().context("Missing server stdin")?;
    let mut reader = BufReader::new(child.stdout.take().context("Missing server stdout")?);
    let mut state = ClientState {
        settings: input.get("settings").cloned().unwrap_or(json!({})),
        workspace_folders: json!([{"uri":root_uri.as_str(),"name":root.file_name().and_then(|n|n.to_str()).unwrap_or("workspace")}]),
        ..Default::default()
    };
    let initialize = async {
    send(&mut writer,&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"processId":std::process::id(),"rootUri":root_uri.as_str(),"clientInfo":{"name":"claude-code-rust"},"initializationOptions":input.get("initialization_options").cloned().unwrap_or(json!({})),"capabilities":{"general":{"positionEncodings":["utf-16"]},"textDocument":{"diagnostic":{},"documentSymbol":{"hierarchicalDocumentSymbolSupport":true},"hover":{"contentFormat":["markdown","plaintext"]},"callHierarchy":{}},"workspace":{"applyEdit":false,"configuration":true,"workspaceFolders":true}},"workspaceFolders":state.workspace_folders}})).await?;
    let initialized = response(&mut reader, &mut writer, 1, &mut state).await?;
    if let Some(encoding) = initialized["capabilities"]["positionEncoding"].as_str() {
        if encoding != "utf-16" {
            bail!("Language server selected unsupported position encoding: {encoding}");
        }
    }
    send(
        &mut writer,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )
    .await?;
    Ok::<_, anyhow::Error>(initialized)
    };
    let initialized = if let Some(timeout) = lifecycle_options(input)?.0 {
        tokio::time::timeout(timeout, initialize).await.with_context(|| format!("Language server initialization timed out after {}ms (startupTimeout)", timeout.as_millis()))??
    } else { initialize.await? };
    send(&mut writer,&json!({"jsonrpc":"2.0","method":"workspace/didChangeConfiguration","params":{"settings":state.settings}})).await?;

    Ok(Server {
        child,
        _process_tree: process_tree,
        writer,
        reader,
        state,
        capabilities: initialized["capabilities"].clone(),
        documents: HashMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Passive feedback deliberately skips busy pool locks and waits only a few
    // milliseconds for push notifications. Synchronize on the observable
    // diagnostic, not scheduler timing, while retaining a hard failure bound.
    async fn await_feedback(cwd: &str, file: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(value) = passive_feedback(cwd, file).await {
                    return value;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Approved server did not publish expected diagnostics within 5 seconds")
    }

    #[tokio::test]
    async fn passive_feedback_reuses_approved_server_versions_deduplicates_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let path = dir.path().join("test.ts");
        std::fs::write(&path, "const text = '中文😀';").unwrap();
        // A configured executable is not authorization to start it on a Read.
        configure(
            cwd,
            vec![
                json!({"command":"must-not-be-started","extensionToLanguage":{".ts":"typescript"}}),
            ],
        )
        .unwrap();
        assert!(passive_feedback(cwd, "test.ts").await.is_none());
        assert!(!dir.path().join("protocol.jsonl").exists());
        let script = SERVER.replace("if(m.method==='exit')", r#"
 if(m.method==='textDocument/didChange'){
   const version=m.params.textDocument.version;
   const diagnostic={message:'新的诊断😀',severity:1,range:{start:{line:0,character:4},end:{line:0,character:6}}};
   send({method:'textDocument/publishDiagnostics',params:{uri,version:version-1,diagnostics:[{message:'STALE'}]}});
   const result={method:'textDocument/publishDiagnostics',params:{uri,version,diagnostics:m.params.contentChanges[0].text==='fixed'?[]:[diagnostic]}};
   if(m.params.contentChanges[0].text.startsWith('changed')){
     // Hold the current-version notification until the test releases it, so
     // the first passive deadline deterministically sees only stale data.
     let released=false;
     const release=()=>{if(!released && fs.existsSync('release-diagnostic')){released=true;watcher.close();send(result);}};
     const watcher=fs.watch('.',release); release();
   } else { send(result); }
   continue;
 }
 if(m.method==='exit')"#);
        let input = json!({"server":"node","args":["-e",script],"filePath":"test.ts","operation":"hover","line":1,"character":1,"timeout_secs":10});
        execute(input.clone(), cwd).await.unwrap();
        let first = await_feedback(cwd, "test.ts").await;
        assert_eq!(first["diagnostics"][0]["message"], "fixture diagnostic");
        assert!(passive_feedback(cwd, "test.ts").await.is_none());
        std::fs::write(&path, "changed 中文😀").unwrap();
        assert!(passive_feedback(cwd, "test.ts").await.is_none());
        std::fs::write(dir.path().join("release-diagnostic"), "release").unwrap();
        let changed = await_feedback(cwd, "test.ts").await;
        assert_eq!(changed["diagnostics"][0]["message"], "新的诊断😀");
        assert_eq!(changed["diagnostics"][0]["range"]["start"]["character"], 4);
        assert!(!changed.to_string().contains("STALE"));
        assert!(passive_feedback(cwd, "test.ts").await.is_none());
        std::fs::write(&path, "fixed").unwrap();
        let cleared = await_feedback(cwd, "test.ts").await;
        assert_eq!(cleared["cleared"], true);
        assert!(passive_feedback(cwd, "test.ts").await.is_none());
        let mut close = input;
        close["reuse"] = json!(false);
        execute(close, cwd).await.unwrap();
        let protocol = std::fs::read_to_string(dir.path().join("protocol.jsonl")).unwrap();
        assert_eq!(
            protocol
                .lines()
                .filter(|line| line.contains("\"method\":\"initialize\""))
                .count(),
            1
        );
        assert!(protocol.contains("changed 中文😀"));
    }

    #[tokio::test]
    async fn cancelled_partial_frame_preserves_unicode_message() {
        let value = json!({"result":"\u{4e2d}\u{6587}\u{1f600}"});
        let body = serde_json::to_vec(&value).unwrap();
        let frame = [
            format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes(),
            body,
        ]
        .concat();
        // Cancel inside every header/body position, including a split UTF-8
        // codepoint, then verify the next coalesced frame remains aligned.
        for split in 1..frame.len() {
            let (mut sender, receiver) = tokio::io::duplex(4096);
            let mut reader = BufReader::new(receiver);
            let mut pending = Vec::new();
            sender.write_all(&frame[..split]).await.unwrap();
            let mut read = Box::pin(receive_buffered(&mut reader, &mut pending));
            // Poll until Pending after consuming the available prefix; no clock
            // delay is required to establish that cancellation occurs mid-frame.
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(read.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            drop(read);
            assert_eq!(pending, frame[..split]);
            sender.write_all(&frame[split..]).await.unwrap();
            sender.write_all(&frame).await.unwrap();
            assert_eq!(
                receive_buffered(&mut reader, &mut pending).await.unwrap(),
                value
            );
            assert_eq!(
                receive_buffered(&mut reader, &mut pending).await.unwrap(),
                value
            );
        }
    }

    // The mock enforces initialization and document synchronization, and records the
    // protocol so tests check actual subprocess requests rather than method mappings.
    const SERVER: &str = r#"
const fs = require('fs');
let buffer=Buffer.alloc(0), initialized=false, opened=false, uri;
function send(m) { const b=JSON.stringify({jsonrpc:'2.0',...m});process.stdout.write('Content-Length: '+Buffer.byteLength(b)+'\r\n\r\n'+b); }
process.stdin.on('data',chunk=>{buffer=Buffer.concat([buffer,chunk]);while(true){
 let end=buffer.indexOf('\r\n\r\n');if(end<0)return;
 let length=Number(buffer.subarray(0,end).toString().match(/Content-Length: (\d+)/i)[1]);
 if(buffer.length<end+4+length)return;
 let m=JSON.parse(buffer.subarray(end+4,end+4+length));buffer=buffer.subarray(end+4+length);
 fs.appendFileSync('protocol.jsonl',JSON.stringify(m)+'\n');
 if(m.method==='initialize'){send({id:m.id,result:{capabilities:{}}});continue;}
 if(m.method==='initialized'){initialized=true;continue;}
 if(m.method==='textDocument/didOpen'){
   if(!initialized)process.exit(12);opened=true;uri=m.params.textDocument.uri;
   send({method:'textDocument/publishDiagnostics',params:{uri,diagnostics:[{message:'fixture diagnostic',severity:2,range:{start:{line:0,character:0},end:{line:0,character:1}}}]}});
   send({id:'config',method:'workspace/configuration',params:{items:[{section:'fixture.enabled'}]}});
   continue;
 }
 if(m.method==='exit'){process.exit(0);}
 if(!m.method)continue;
 if(m.id){
  if(!opened)process.exit(13);
  let result=m.method==='shutdown'?null:{method:m.method,params:m.params};
  if(m.method==='textDocument/prepareCallHierarchy')result=[{name:'first',kind:12,uri,range:{start:{line:0,character:0},end:{line:0,character:1}},selectionRange:{start:{line:0,character:0},end:{line:0,character:1}}},{name:'second',kind:12,uri}];
  if(m.method.startsWith('callHierarchy/'))result=[{from:m.params.item,fromRanges:[]}];
  send({id:m.id,result});
 }
}});
"#;

    async fn fixture(operation: &str) -> (Value, Vec<Value>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.ts"),
            "function greet() {}\ngreet();\n",
        )
        .unwrap();
        let result=execute(json!({"server":"node","args":["-e",SERVER],"filePath":"test.ts","operation":operation,"line":2,"character":3,"settings":{"fixture":{"enabled":true}},"query":"greet","timeout_secs":10,"reuse":false}),dir.path().to_str().unwrap()).await.unwrap();
        let protocol = std::fs::read_to_string(dir.path().join("protocol.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        (result, protocol)
    }

    #[tokio::test]
    async fn official_operations_use_correct_positions_and_close_cleanly() {
        for (operation, method) in [
            ("goToDefinition", "textDocument/definition"),
            ("findReferences", "textDocument/references"),
            ("hover", "textDocument/hover"),
            ("documentSymbol", "textDocument/documentSymbol"),
            ("workspaceSymbol", "workspace/symbol"),
            ("goToImplementation", "textDocument/implementation"),
        ] {
            let (result, protocol) = fixture(operation).await;
            assert_eq!(result["result"]["method"], method);
            if matches!(
                operation,
                "goToDefinition" | "findReferences" | "hover" | "goToImplementation"
            ) {
                assert_eq!(
                    result["result"]["params"]["position"],
                    json!({"line":1,"character":2})
                );
            }
            if operation == "workspaceSymbol" {
                assert_eq!(result["result"]["params"], json!({"query":"greet"}));
            }
            assert!(protocol
                .iter()
                .any(|m| m["id"] == "config" && m["result"] == json!([true])));
            let methods: Vec<_> = protocol
                .iter()
                .filter_map(|m| m["method"].as_str())
                .collect();
            assert!(methods.ends_with(&["textDocument/didClose", "shutdown", "exit"]));
        }
    }

    #[tokio::test]
    async fn call_hierarchy_queries_every_prepared_item() {
        for operation in ["incomingCalls", "outgoingCalls"] {
            let (result, protocol) = fixture(operation).await;
            assert_eq!(result["result"].as_array().unwrap().len(), 2);
            let method = format!("callHierarchy/{operation}");
            assert_eq!(protocol.iter().filter(|m| m["method"] == method).count(), 2);
            assert_eq!(result["result"][0]["from"]["name"], "first");
        }
        let (result, _) = fixture("prepareCallHierarchy").await;
        assert_eq!(result["result"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn push_diagnostics_are_returned_without_unsupported_pull_request() {
        let (result, protocol) = fixture("diagnostics").await;
        assert_eq!(
            result["result"]["items"][0]["message"],
            "fixture diagnostic"
        );
        assert!(!protocol
            .iter()
            .any(|m| m["method"] == "textDocument/diagnostic"));
    }

    #[tokio::test]
    async fn unresponsive_server_is_bounded_and_bad_positions_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), "fn main() {}\n").unwrap();
        let input = json!({"server":"node","args":["-e","setInterval(()=>{},10000)"],"path":"test.rs","operation":"hover","timeout_secs":1});
        let start = std::time::Instant::now();
        let error = execute(input, dir.path().to_str().unwrap())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        let error = execute(
            json!({"filePath":"test.rs","operation":"hover","line":0}),
            dir.path().to_str().unwrap(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("at least 1"));
    }

    #[tokio::test]
    async fn plugin_configuration_reuses_server_and_syncs_changed_documents() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let file = dir.path().join("fixture.custom");
        std::fs::write(&file, "first").unwrap();
        let config = json!({"name":"fixture","command":"node","args":["-e",SERVER],"extensionToLanguage":{".custom":"fixture"},"env":{"LSP_FIXTURE":"1"},"settings":{"fixture":{"enabled":true}}});
        configure(cwd, vec![config.clone()]).unwrap();
        let input = json!({"filePath":"fixture.custom","operation":"hover"});
        execute(input.clone(), cwd).await.unwrap();
        configure(cwd, vec![config]).unwrap();
        execute(input.clone(), cwd).await.unwrap();
        std::fs::write(&file, "changed").unwrap();
        let mut input = input;
        input["reuse"] = json!(false);
        execute(input, cwd).await.unwrap();
        let protocol: Vec<Value> = std::fs::read_to_string(dir.path().join("protocol.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            protocol
                .iter()
                .filter(|m| m["method"] == "initialize")
                .count(),
            1
        );
        assert_eq!(
            protocol
                .iter()
                .filter(|m| m["method"] == "textDocument/didOpen")
                .count(),
            1
        );
        let change = protocol
            .iter()
            .find(|m| m["method"] == "textDocument/didChange")
            .unwrap();
        assert_eq!(change["params"]["textDocument"]["version"], 2);
        assert_eq!(change["params"]["contentChanges"][0]["text"], "changed");
        assert!(protocol.iter().any(|m| m["method"] == "exit"));
    }

    #[tokio::test]
    #[ignore = "requires an externally installed TypeScript LSP and a read-only workspace"]
    async fn installed_typescript_server_reads_real_workspace() {
        let server = std::env::var("LSP_TEST_SERVER_JS").expect("LSP_TEST_SERVER_JS");
        let workspace = std::env::var("LSP_TEST_WORKSPACE").expect("LSP_TEST_WORKSPACE");
        let file = std::env::var("LSP_TEST_FILE").expect("LSP_TEST_FILE");
        let tsserver = std::env::var("LSP_TEST_TSSERVER").expect("LSP_TEST_TSSERVER");
        let line: u64 = std::env::var("LSP_TEST_LINE")
            .unwrap_or("1".into())
            .parse()
            .unwrap();
        let character: u64 = std::env::var("LSP_TEST_CHARACTER")
            .unwrap_or("14".into())
            .parse()
            .unwrap();
        let query = std::env::var("LSP_TEST_QUERY").unwrap_or("EXPLICIT_SPACE".into());
        for operation in [
            "documentSymbol",
            "hover",
            "goToDefinition",
            "findReferences",
            "workspaceSymbol",
            "prepareCallHierarchy",
            "incomingCalls",
            "outgoingCalls",
            "goToImplementation",
        ] {
            let result=execute(json!({"server":"node","args":[server,"--stdio"],"filePath":file,"operation":operation,"line":line,"character":character,"query":query,"initialization_options":{"tsserver":{"path":tsserver}},"timeout_secs":90,"reuse":operation!="goToImplementation"}),&workspace).await.unwrap();
            println!("{operation}: {}", result["result"]);
            if operation == "documentSymbol" {
                assert!(!result["result"].as_array().unwrap().is_empty());
            }
        }
    }
    #[tokio::test]
    async fn framing_handles_unicode_and_rejects_oversize() {
        let (mut sender, receiver) = tokio::io::duplex(4096);
        let message = json!({"jsonrpc":"2.0","id":2,"result":"中文"});
        send(&mut sender, &message).await.unwrap();
        assert_eq!(
            receive(&mut BufReader::new(receiver)).await.unwrap(),
            message
        );
        let (mut sender, receiver) = tokio::io::duplex(4096);
        sender
            .write_all(b"Content-Length: 99999999\r\n\r\n")
            .await
            .unwrap();
        assert!(receive(&mut BufReader::new(receiver)).await.is_err());
    }
    #[tokio::test]
    async fn real_stdio_fixture_runs_initialize_query_and_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), "fn main() {}\n").unwrap();
        let script = r#"let buffer=Buffer.alloc(0);process.stdin.on('data',chunk=>{buffer=Buffer.concat([buffer,chunk]);while(true){let end=buffer.indexOf('\r\n\r\n');if(end<0)return;let length=Number(buffer.subarray(0,end).toString().match(/Content-Length: (\d+)/i)[1]);if(buffer.length<end+4+length)return;let m=JSON.parse(buffer.subarray(end+4,end+4+length));buffer=buffer.subarray(end+4+length);if(m.id){let result=m.method==='initialize'?{capabilities:{}}:m.method==='shutdown'?null:{contents:'fixture hover'};let body=JSON.stringify({jsonrpc:'2.0',id:m.id,result});process.stdout.write('Content-Length: '+Buffer.byteLength(body)+'\r\n\r\n'+body);}if(m.method==='exit')process.exit(0);}});"#;
        let result=execute(json!({"server":"node","args":["-e",script],"path":"test.rs","language_id":"rust","operation":"hover","line":0,"character":3,"reuse":false}),dir.path().to_str().unwrap()).await.unwrap();
        assert_eq!(result["result"]["contents"], "fixture hover");
    }
}
