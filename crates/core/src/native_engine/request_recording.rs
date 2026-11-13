//! Opt-in wire records. All context is explicit; no ambient turn/task state.
use super::{provider_manager::ResolvedProvider, request_options::RequestOptions};
use anyhow::{Context, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub cli: bool,
    pub full_access: bool,
}

pub fn enabled(cli: bool, env: Option<&str>, setting: Option<bool>, full_access: bool) -> bool {
    cli || env
        .and_then(|v| match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" => Some(true),
            "0" | "false" => Some(false),
            _ => None,
        })
        .or(setting)
        .unwrap_or(full_access)
}

impl Policy {
    pub fn enabled_at(&self, root: &Path) -> bool {
        let setting = fs::read(root.join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["requestRecording"]["enabled"].as_bool());
        enabled(
            self.cli,
            std::env::var("DEEPSEEK_CODE_RECORD_REQUESTS")
                .ok()
                .as_deref(),
            setting,
            self.full_access,
        )
    }
}

#[derive(Clone, Debug)]
pub struct RecordingContext {
    pub root: PathBuf,
    pub parent_session_id: Option<String>,
}

fn warn(error: impl std::fmt::Display) {
    tracing::warn!(module = "RequestRecording", error = %error, "Request recording failed; transport continues");
}

pub fn valid_component(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn directory(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(path)?.file_type().is_dir() {
                return Err(std::io::Error::other(
                    "record directory is not a real directory",
                ));
            }
        }
        Err(e) => return Err(e),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn file(path: &Path, new: bool) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    if new {
        opts.create_new(true);
    } else {
        opts.create(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let f = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(f)
}

fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = file(path, true)?;
    f.write_all(bytes)?;
    f.sync_data()
}

fn save_meta(path: &Path, value: &Value) -> Result<()> {
    // Atomic replace leaves the initial incomplete metadata intact on interruption.
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    write_new(&temp, &serde_json::to_vec_pretty(value)?)?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(temp);
        return Err(error.into());
    }
    Ok(())
}

/// Name matching is case insensitive. Also omit recognizable bearer/basic,
/// provider key prefixes, JWTs and long opaque tokens even on arbitrary names.
fn secret_header(name: &str, value: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let v = value.trim().to_ascii_lowercase();
    name.contains("auth")
        || name.contains("cookie")
        || name.contains("api-key")
        || name.contains("apikey")
        || name.contains("api_key")
        || name.contains("token")
        || name.contains("secret")
        || name.contains("credential")
        || name.contains("password")
        || name.ends_with("-key")
        || [
            "bearer ",
            "basic ",
            "sk-",
            "sk_",
            "ghp_",
            "github_pat_",
            "aiza",
            "akia",
            "eyj",
        ]
        .iter()
        .any(|p| v.starts_with(p))
        || (value.len() >= 32
            && uuid::Uuid::parse_str(value).is_err()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_+/=.".contains(&b))
            && value.bytes().any(|b| b.is_ascii_digit())
            && value.bytes().any(|b| b.is_ascii_alphabetic()))
}

fn headers(request: &reqwest::Request) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in request.headers() {
        if let Ok(value) = value.to_str() {
            if !secret_header(name.as_str(), value) {
                out.entry(name.to_string())
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .unwrap()
                    .push(json!(value));
            }
        }
    }
    Value::Object(out)
}

/// Root lock orders startup, sequence allocation and deletion across processes.
/// A shared session lease is held from before allocation until all I/O ends.
struct Lease {
    _file: File,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = crate::fs::unlock_file(&self._file);
    }
}
fn root_lock(root: &Path) -> Result<File> {
    directory(root)?;
    let lock = file(&root.join(".lock"), false)?;
    crate::fs::lock_file(&lock)?;
    Ok(lock)
}
fn acquire(root: &Path, session: &str) -> Result<(PathBuf, Lease, File)> {
    anyhow::ensure!(valid_component(session), "Invalid recording session ID");
    let lock = root_lock(root)?;
    // Expiry errors never prevent recording the current request.
    if let Err(error) = maybe_cleanup(root) {
        warn(error);
    }
    let dir = root.join(session);
    directory(&dir)?;
    let lease = file(&dir.join(".active"), false)?;
    lease.lock_shared()?;
    Ok((dir, Lease { _file: lease }, lock))
}

fn next_sequence(dir: &Path, compaction: bool) -> Result<u64> {
    let mut max = 0;
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let number = if compaction {
            name.strip_prefix("compaction-")
                .and_then(|n| n.strip_suffix(".json"))
        } else {
            name.split_once('-').map(|(n, _)| n)
        };
        max = max.max(number.and_then(|n| n.parse::<u64>().ok()).unwrap_or(0));
    }
    max.checked_add(1).context("Record sequence exhausted")
}

fn maybe_cleanup(root: &Path) -> Result<()> {
    let marker = root.join(".cleanup");
    let now = SystemTime::now();
    if fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .is_some_and(|at| now.duration_since(at).unwrap_or_default() < Duration::from_secs(3600))
    {
        return Ok(());
    }
    cleanup_locked(root, now)?;
    let mut marker = file(&marker, false)?;
    marker.set_len(0)?;
    marker.write_all(chrono::Utc::now().to_rfc3339().as_bytes())?;
    marker.sync_data()?;
    Ok(())
}

fn cleanup_locked(root: &Path, now: SystemTime) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let mut newest = entry.metadata()?.modified()?;
        for child in fs::read_dir(&dir)? {
            newest = newest.max(child?.metadata()?.modified()?);
        }
        if now.duration_since(newest).unwrap_or_default() <= Duration::from_secs(14 * 86400) {
            continue;
        }
        // Do not create an active marker: that would refresh the age.
        let active = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join(".active"));
        let _held = match active {
            Ok(f) => {
                if f.try_lock().is_err() {
                    continue;
                }
                Some(f)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                warn(e);
                continue;
            }
        };
        if let Err(error) = fs::remove_dir_all(&dir) {
            warn(error);
        }
    }
    Ok(())
}

struct Attempt {
    _lease: Lease,
    path: PathBuf,
    meta: Value,
    body: Option<File>,
    pending: Vec<u8>,
    completed_marker: bool,
    streaming: bool,
    ended: bool,
}
impl Attempt {
    fn start(
        options: &RequestOptions,
        provider: &ResolvedProvider,
        api: &str,
        request: &reqwest::Request,
        streaming: bool,
    ) -> Result<Option<Self>> {
        let (Some(context), Some(session)) = (&options.recording, &options.session_id) else {
            return Ok(None);
        };
        let root = context.root.join("request-records");
        let (dir, lease, lock) = acquire(&root, session)?;
        let sequence = next_sequence(&dir, false)?;
        let request_id = request
            .headers()
            .get("x-request-id")
            .and_then(|h| h.to_str().ok())
            .filter(|id| valid_component(id))
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let attempt = request
            .headers()
            .get("x-dcl-retry-count")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
            + 1;
        let base = format!("{sequence:04}-{request_id}-a{attempt}");
        let path = dir.join(format!("{base}.meta.json"));
        let mut url = request.url().clone();
        url.set_query(None);
        url.set_fragment(None);
        let _ = url.set_username("");
        let _ = url.set_password(None);
        let meta = json!({"sequence":sequence,"request_id":request_id,"model_request_id":options.model_request_id,
            "session_id":session,"parent_session_id":context.parent_session_id,"provider":provider.provider.id,
            "model":provider.model.id,"api_format":api,"method":request.method().as_str(),"url":url.as_str(),"purpose":options.purpose,
            "compaction_stage":options.compaction_stage,"compaction_label":options.compaction_label,"attempt":attempt,"http_status":null,
            "started_at":chrono::Utc::now().to_rfc3339(),"ended_at":null,"error":null,"stream_completed":false});
        write_new(&path, &serde_json::to_vec_pretty(&meta)?)?;
        if let Some((path, sequences)) = &options.compaction_record {
            sequences
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(sequence);
            if let Ok(bytes) = fs::read(path) {
                if let Ok(mut record) = serde_json::from_slice::<Value>(&bytes) {
                    record["request_sequences"] =
                        json!(*sequences.lock().unwrap_or_else(|e| e.into_inner()));
                    if let Err(error) = save_meta(path, &record) {
                        warn(error);
                    }
                }
            }
        }
        // The serialized reqwest body is the same object the client executes.
        let body = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .context("Request body is not replayable bytes")?;
        write_new(&dir.join(format!("{base}.request.json")), body)?;
        write_new(
            &dir.join(format!("{base}.headers.json")),
            &serde_json::to_vec_pretty(&headers(request))?,
        )?;
        crate::fs::unlock_file(&lock)?;
        Ok(Some(Self {
            _lease: lease,
            path,
            meta,
            body: None,
            pending: Vec::new(),
            completed_marker: false,
            streaming,
            ended: false,
        }))
    }
    fn status(&mut self, response: &reqwest::Response) {
        self.meta["http_status"] = json!(response.status().as_u16());
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if mime.contains("text/event-stream") {
            self.streaming = true;
        } else if mime.contains("application/json") || !response.status().is_success() {
            self.streaming = false;
        }
        let base = self
            .path
            .to_string_lossy()
            .trim_end_matches(".meta.json")
            .to_string();
        match file(
            Path::new(&format!(
                "{base}.{}",
                if self.streaming {
                    "sse.txt"
                } else {
                    "response.json"
                }
            )),
            true,
        ) {
            Ok(file) => self.body = Some(file),
            Err(error) => {
                self.meta["recording_error"] = json!(error.to_string());
                warn(error);
            }
        }
        if let Err(error) = save_meta(&self.path, &self.meta) {
            warn(error);
        }
    }
    fn chunk(&mut self, bytes: &[u8]) {
        if let Some(file) = &mut self.body {
            if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_data()) {
                self.meta["recording_error"] = json!(error.to_string());
                warn(error);
                self.body = None;
            }
        }
        // Only completion recognition is buffered; the on-disk bytes are untouched.
        if self.streaming && !self.completed_marker {
            self.pending.extend_from_slice(bytes);
            if self.meta["api_format"] == "bedrock-converse-stream" {
                self.completed_marker =
                    super::provider_runtime::recorded_binary_completion(&mut self.pending);
                return;
            }
            while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                let line = String::from_utf8_lossy(&self.pending[..end]);
                let data = line.trim().strip_prefix("data:").map(str::trim);
                if let Some(data) = data {
                    self.completed_marker |= data == "[DONE]"
                        || serde_json::from_str::<Value>(data).ok().is_some_and(|v| {
                            matches!(
                                v["type"].as_str(),
                                Some("message_stop" | "response.completed")
                            ) || v["choices"].as_array().is_some_and(|choices| {
                                choices
                                    .iter()
                                    .any(|choice| choice["finish_reason"].is_string())
                            }) || v["candidates"]
                                .as_array()
                                .is_some_and(|a| a.iter().any(|c| c["finishReason"].is_string()))
                        });
                }
                self.pending.drain(..=end);
            }
            if self.pending.len() > 16 * 1024 * 1024 {
                self.pending.clear();
            }
        }
    }
    fn finish(&mut self, error: Option<&str>, eof: bool) {
        self.ended = true;
        self.meta["ended_at"] = json!(chrono::Utc::now().to_rfc3339());
        self.meta["stream_completed"] =
            json!(eof && error.is_none() && (!self.streaming || self.completed_marker));
        self.meta["error"] =
            json!(
                error.or_else(|| if eof && self.streaming && !self.completed_marker {
                    Some("Stream ended without completion marker")
                } else {
                    None
                })
            );
        if self.meta["http_status"]
            .as_u64()
            .is_some_and(|status| status >= 400)
        {
            self.meta["stream_completed"] = json!(false);
            if self.meta["error"].is_null() {
                self.meta["error"] = json!(format!("HTTP status {}", self.meta["http_status"]));
            }
        }
        if let Err(error) = save_meta(&self.path, &self.meta) {
            warn(error);
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.ended {
            if self.completed_marker {
                self.finish(None, true);
            } else {
                self.finish(
                    Some("Response dropped or request cancelled before completion"),
                    false,
                );
            }
        }
    }
}

/// Executes exactly the built request. The disabled path returns the original
/// response. The enabled path copies raw body chunks before any parser sees them.
pub async fn send(
    request: reqwest::RequestBuilder,
    options: &RequestOptions,
    provider: &ResolvedProvider,
    api: &str,
    streaming: bool,
) -> Result<reqwest::Response, reqwest::Error> {
    let (client, request) = request.build_split();
    let mut request = request?;
    if options.recording.is_some() {
        // These are reqwest/hyper's defaults for the byte bodies used by model
        // clients. Making them explicit lets the record include wire headers.
        use reqwest::header::{HeaderValue, ACCEPT, CONTENT_LENGTH, HOST, TRANSFER_ENCODING};
        let host = request
            .url()
            .host_str()
            .map(|host| match request.url().port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            });
        let length = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .map(<[u8]>::len);
        let headers = request.headers_mut();
        headers
            .entry(ACCEPT)
            .or_insert(HeaderValue::from_static("*/*"));
        if let Some(host) = host.and_then(|host| HeaderValue::from_str(&host).ok()) {
            headers.entry(HOST).or_insert(host);
        }
        if !headers.contains_key(TRANSFER_ENCODING) {
            if let Some(length) = length {
                headers
                    .entry(CONTENT_LENGTH)
                    .or_insert(HeaderValue::from(length));
            }
        }
    }
    let mut recording = match Attempt::start(options, provider, api, &request, streaming) {
        Ok(recording) => recording,
        Err(error) => {
            warn(error);
            None
        }
    };
    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) => {
            if let Some(recording) = &mut recording {
                recording.finish(
                    Some(if error.is_timeout() {
                        "HTTP request timed out"
                    } else {
                        "HTTP request failed"
                    }),
                    false,
                );
            }
            return Err(error);
        }
    };
    let Some(mut recording) = recording else {
        return Ok(response);
    };
    recording.status(&response);
    use reqwest::ResponseBuilderExt;
    let mut builder = axum::http::Response::builder()
        .status(response.status())
        .version(response.version());
    *builder.headers_mut().unwrap() = response.headers().clone();
    *builder.extensions_mut().unwrap() = response.extensions().clone();
    builder = builder.url(response.url().clone());
    let stream = response.bytes_stream();
    let bytes = async_stream::stream! {
        futures::pin_mut!(stream);
        while let Some(part) = stream.next().await {
            match &part {
                Ok(bytes) => recording.chunk(bytes),
                Err(_) => recording.finish(Some("HTTP response body failed"), false),
            }
            yield part;
        }
        if !recording.ended { recording.finish(None, true); }
    };
    Ok(builder
        .body(reqwest::Body::wrap_stream(bytes))
        .unwrap()
        .into())
}

/// Read-only plain-text inspection, also works after the conversation is deleted.
pub fn list(root: &Path, session: &str) -> Result<String> {
    anyhow::ensure!(valid_component(session), "Invalid session ID");
    let dir = root.join("request-records").join(session);
    if !dir.is_dir() {
        return Ok(format!("No request records for session {session}. Recording may have been disabled.\nDirectory: {}\n", dir.display()));
    }
    let mut entries = fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| {
        e.file_name()
            .to_string_lossy()
            .split('-')
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0)
    });
    let mut result = format!("Session: {session}\nDirectory: {}\n", dir.display());
    let mut count = 0;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".meta.json") {
            continue;
        }
        count += 1;
        let meta: Value = match fs::read(entry.path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
        {
            Some(meta) => meta,
            None => {
                result.push_str(&format!(
                    "Unreadable metadata: {}\n",
                    entry.path().display()
                ));
                continue;
            }
        };
        result.push_str(&format!(
            "\n{}: purpose={} model={} HTTP={} completed={}\n",
            meta["sequence"],
            meta["purpose"],
            meta["model"],
            meta["http_status"],
            meta["stream_completed"]
        ));
        let base = name.trim_end_matches(".meta.json");
        for suffix in [
            "request.json",
            "headers.json",
            "sse.txt",
            "response.json",
            "meta.json",
        ] {
            let path = dir.join(format!("{base}.{suffix}"));
            if let Ok(stat) = fs::metadata(&path) {
                result.push_str(&format!("  {} bytes: {}\n", stat.len(), path.display()));
            }
        }
    }
    if count == 0 {
        result.push_str("No request records in this session directory.\n");
    }
    Ok(result)
}

#[cfg(test)]
mod tests;

/// One compaction operation, kept incomplete across cancellation/crash.
pub(crate) struct CompactionRecord {
    _lease: Lease,
    path: PathBuf,
    meta: Value,
    sequences: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
}
impl CompactionRecord {
    pub(crate) fn start(
        options: &mut RequestOptions,
        trigger: &str,
        transcript: &str,
    ) -> Option<Self> {
        let result = (|| -> Result<Option<Self>> {
            let (Some(context), Some(session)) = (&options.recording, &options.session_id) else {
                return Ok(None);
            };
            let (dir, lease, lock) = acquire(&context.root.join("request-records"), session)?;
            let sequence = next_sequence(&dir, true)?;
            let path = dir.join(format!("compaction-{sequence:04}.json"));
            let rows = serde_json::from_str::<Value>(transcript).unwrap_or(Value::Null);
            let meta = json!({"session_id":session,"trigger":trigger,"started_at":chrono::Utc::now().to_rfc3339(),
                "ended_at":null,"completed":false,"before_estimated_tokens":transcript.chars().count().div_ceil(3),
                "before_messages":rows,"summary":null,"after_boundary":null,"request_sequences":[]});
            write_new(&path, &serde_json::to_vec_pretty(&meta)?)?;
            crate::fs::unlock_file(&lock)?;
            let sequences = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            options.compaction_record = Some((path.clone(), sequences.clone()));
            Ok(Some(Self {
                _lease: lease,
                path,
                meta,
                sequences,
            }))
        })();
        match result {
            Ok(value) => value,
            Err(error) => {
                warn(error);
                None
            }
        }
    }
    pub(crate) fn finish(
        &mut self,
        summary: &crate::services::compaction::GeneratedSummary,
        boundary: &str,
    ) {
        self.meta["completed"] = json!(true);
        self.meta["ended_at"] = json!(chrono::Utc::now().to_rfc3339());
        self.meta["summary"] = json!(summary.text);
        self.meta["after_boundary"] = json!(boundary);
        self.meta["after_native_content"] = json!(summary.native);
        self.save();
    }
    pub(crate) fn finish_partial(
        &mut self,
        summary: &str,
        branch: &str,
        message: &str,
        direction: &str,
    ) {
        self.meta["completed"] = json!(true);
        self.meta["ended_at"] = json!(chrono::Utc::now().to_rfc3339());
        self.meta["summary"] = json!(summary);
        self.meta["after_boundary"] = json!({"branch_session_id":branch,"selected_message_id":message,"summarized_direction":direction});
        self.save();
    }
    fn save(&mut self) {
        self.meta["request_sequences"] =
            json!(*self.sequences.lock().unwrap_or_else(|e| e.into_inner()));
        if let Err(error) = save_meta(&self.path, &self.meta) {
            warn(error);
        }
    }
}
impl Drop for CompactionRecord {
    fn drop(&mut self) {
        self.save();
    }
}
