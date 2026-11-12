//! Bounded embedded JavaScript orchestration for ToolScript (foundation only).
//!
//! This module is the engine-only increment. It runs a JavaScript script in an
//! in-process QuickJS runtime and exposes one typed asynchronous host callback,
//! `tools.call(name, args)`. It does **not** register a model-visible tool, does
//! **not** dispatch through the shared tool executor, and does **not** know
//! about permissions, hooks, transcripts, or durable background tasks. Callers
//! provide the host callback; the executor integration is a separate increment.
//!
//! The script can use ordinary JavaScript to sequence work:
//!
//! ```js
//! const first = await tools.call("Read", { path: "a.txt" });
//! const all = await Promise.all([
//!   tools.call("Read", { path: "b.txt" }),
//!   tools.call("Read", { path: "c.txt" }),
//! ]);
//! return { first, all };
//! ```
//!
//! Every limit is declared up front in [`ToolScriptLimits`]. The VM runs on the
//! caller's thread: like every `rquickjs` future it is not `Send` and must not
//! be moved to another task. On Android the types remain available but [`ToolScriptEngine::run`]
//! returns [`ToolScriptError::UnsupportedPlatform`], because `rquickjs` has no
//! Android bindings and must not be linked there.

use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::watch;

#[cfg(not(target_os = "android"))]
mod boundary;
#[cfg(not(target_os = "android"))]
mod engine;
#[cfg(not(target_os = "android"))]
mod host;
#[cfg(all(test, not(target_os = "android")))]
mod tests;

/// Cooperative cancellation for one script run.
///
/// The flag is checked by the QuickJS interrupt handler, so a synchronous CPU
/// loop stops even though a Tokio timer cannot be polled while the loop runs.
/// The token is `Send + Sync`: a supervisor thread or signal handler can cancel
/// a blocked script. Cancellation means "the run stops and every pending host
/// future is dropped"; it does not promise operating-system side-effect
/// cancellation beyond the callback contract described on [`ToolCallHandler`].
#[derive(Clone, Debug)]
pub struct Cancellation {
    inner: Arc<CancellationInner>,
}

#[derive(Debug)]
struct CancellationInner {
    cancelled: AtomicBool,
    sender: watch::Sender<bool>,
}

impl Default for Cancellation {
    fn default() -> Self {
        let (sender, _receiver) = watch::channel(false);
        Self {
            inner: Arc::new(CancellationInner {
                cancelled: AtomicBool::new(false),
                sender,
            }),
        }
    }
}

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Safe to call from any thread.
    pub fn cancel(&self) {
        // Set the lock-free flag first so the interrupt handler sees it even
        // when there is no async receiver to wake.
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.sender.send_replace(true);
    }

    /// Lock-free check used by the QuickJS interrupt handler.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Relaxed)
    }

    /// Resolves when cancellation is requested. Dropping every token also
    /// resolves it (the run owner is gone).
    pub fn cancelled(&self) -> impl std::future::Future<Output = ()> + '_ {
        let mut receiver = self.inner.sender.subscribe();
        async move {
            if self.is_cancelled() {
                return;
            }
            loop {
                if receiver.changed().await.is_err() {
                    return;
                }
                if self.is_cancelled() {
                    return;
                }
            }
        }
    }
}

/// One host tool call requested by the script.
#[derive(Clone, Debug)]
pub struct ToolCallRequest {
    /// Independent, monotonically increasing call number (not a provider tool
    /// call ID). A future executor integration maps this to its own occurrence
    /// ID and records the parent relationship.
    pub call_id: u64,
    /// Tool name exactly as passed by the script.
    pub name: String,
    /// JSON arguments validated at the JS boundary.
    pub args: Value,
}

/// A host tool failure that reaches the script as a catchable `ToolScriptError`.
#[derive(Clone, Debug)]
pub struct ToolCallError {
    /// Stable machine-readable code, for example `permission_denied`.
    pub code: String,
    /// Human-readable, bounded by the caller before returning it here.
    pub message: String,
    /// Optional structured detail; it must be JSON-compatible.
    pub details: Option<Value>,
}

impl ToolCallError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

/// The host side of `tools.call(name, args)`.
///
/// The callback is typed, asynchronous, and receives an independent
/// [`ToolCallRequest`]. Tool failures must be returned as [`ToolCallError`] so
/// the script can catch them; they are never replaced by a generic failure.
///
/// # Cancellation contract
///
/// The runner drops the call future when the script is cancelled, times out, or
/// the top-level script promise settles without awaiting the call. Dropping a
/// future is the cancellation signal. The supplied [`Cancellation`] is also
/// available for implementations that need to stop their own subordinate work
/// deliberately, but the runner does not await a handler-specific shutdown
/// protocol and never claims more than "the future was dropped". Implementations
/// must not spawn detached work that outlives the future they return.
#[async_trait::async_trait]
pub trait ToolCallHandler: Send + Sync {
    async fn call(
        &self,
        request: ToolCallRequest,
        cancellation: Cancellation,
    ) -> Result<Value, ToolCallError>;
}

/// Hard limits for one script run. All byte limits count UTF-8 bytes.
///
/// [`ToolScriptLimits::default`] is the production baseline; callers that need
/// tighter bounds (tests, future low-resource hosts) construct their own value
/// and pass it through [`ToolScriptEngine::with_limits`], which validates it.
#[derive(Clone, Debug)]
pub struct ToolScriptLimits {
    /// Script body bytes (the wrapper and prelude are not counted).
    pub max_source_bytes: usize,
    /// External script arguments serialized as JSON.
    pub max_script_args_bytes: usize,
    /// JSON arguments for one `tools.call`.
    pub max_args_bytes: usize,
    /// JSON result of one host tool call.
    pub max_call_result_bytes: usize,
    /// JSON result returned by the script.
    pub max_script_result_bytes: usize,
    /// Collected `log()` / `console.*` output.
    pub max_log_bytes: usize,
    /// Host calls actually running at the same time.
    pub max_concurrent_calls: usize,
    /// Host calls admitted but not yet settled, including queued and unawaited
    /// promises. Bounds pending promises.
    pub max_pending_calls: usize,
    /// Total admitted host calls for the whole script.
    pub max_tool_calls: u64,
    /// Tool name bytes.
    pub max_call_name_bytes: usize,
    /// Nesting depth accepted at the JSON boundary.
    pub max_boundary_depth: usize,
    /// Error message / stack bytes kept in a diagnostic.
    pub max_diagnostic_bytes: usize,
    /// Wall-clock budget for the whole script, including time awaiting host
    /// calls. Enforced by the QuickJS interrupt handler (and a Tokio timeout as
    /// a transport backstop). It is not a CPU-only timer.
    pub execution_timeout: Duration,
    /// Budget for one host call after it acquires a concurrency permit.
    pub call_timeout: Duration,
    /// QuickJS heap limit.
    pub memory_limit_bytes: usize,
    /// QuickJS stack limit. The caller's thread must have room for this plus
    /// Rust future/QuickJS overhead; the CLI and test harness use 16 MiB stacks.
    pub max_stack_size_bytes: usize,
}

impl Default for ToolScriptLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 256 * 1024,
            max_script_args_bytes: 1024 * 1024,
            max_args_bytes: 64 * 1024,
            max_call_result_bytes: 1024 * 1024,
            max_script_result_bytes: 4 * 1024 * 1024,
            max_log_bytes: 64 * 1024,
            max_concurrent_calls: 8,
            max_pending_calls: 64,
            max_tool_calls: 512,
            max_call_name_bytes: 128,
            max_boundary_depth: 64,
            max_diagnostic_bytes: 4096,
            execution_timeout: Duration::from_secs(300),
            call_timeout: Duration::from_secs(120),
            memory_limit_bytes: 64 * 1024 * 1024,
            max_stack_size_bytes: 1024 * 1024,
        }
    }
}

impl ToolScriptLimits {
    /// Rejects configurations that would make a limit meaningless or unsafe.
    pub fn validate(&self) -> Result<(), ToolScriptError> {
        let invalid = |message: &str| {
            Err(ToolScriptError::InvalidLimits {
                message: message.to_owned(),
            })
        };
        if self.max_source_bytes == 0 {
            return invalid("max_source_bytes must be greater than zero");
        }
        if self.max_script_args_bytes == 0 {
            return invalid("max_script_args_bytes must be greater than zero");
        }
        if self.max_args_bytes == 0 {
            return invalid("max_args_bytes must be greater than zero");
        }
        if self.max_call_result_bytes == 0 {
            return invalid("max_call_result_bytes must be greater than zero");
        }
        if self.max_script_result_bytes == 0 {
            return invalid("max_script_result_bytes must be greater than zero");
        }
        if self.max_log_bytes == 0 {
            return invalid("max_log_bytes must be greater than zero");
        }
        if self.max_call_name_bytes == 0 {
            return invalid("max_call_name_bytes must be greater than zero");
        }
        if self.max_concurrent_calls == 0 || self.max_concurrent_calls > 64 {
            return invalid("max_concurrent_calls must be in 1..=64");
        }
        if self.max_pending_calls < self.max_concurrent_calls || self.max_pending_calls > 1024 {
            return invalid("max_pending_calls must be >= max_concurrent_calls and <= 1024");
        }
        if self.max_tool_calls == 0 {
            return invalid("max_tool_calls must be greater than zero");
        }
        if self.max_boundary_depth == 0 || self.max_boundary_depth > 128 {
            return invalid("max_boundary_depth must be in 1..=128");
        }
        if self.max_diagnostic_bytes < 64 || self.max_diagnostic_bytes > 64 * 1024 {
            return invalid("max_diagnostic_bytes must be in 64..=65536");
        }
        if self.execution_timeout.is_zero() || self.execution_timeout > Duration::from_secs(3600) {
            return invalid("execution_timeout must be in 1ns..=3600s");
        }
        if self.call_timeout.is_zero() || self.call_timeout > self.execution_timeout {
            return invalid("call_timeout must be non-zero and <= execution_timeout");
        }
        if !(1024 * 1024..=1024 * 1024 * 1024).contains(&self.memory_limit_bytes) {
            return invalid("memory_limit_bytes must be in 1 MiB..=1 GiB");
        }
        if !(128 * 1024..=8 * 1024 * 1024).contains(&self.max_stack_size_bytes) {
            return invalid("max_stack_size_bytes must be in 128 KiB..=8 MiB");
        }
        Ok(())
    }
}

/// Successful result of one script run.
#[derive(Clone, Debug)]
pub struct ToolScriptOutcome {
    /// JSON value returned by the script (`undefined` becomes `null`).
    pub value: Value,
    /// Collected `log()` / `console.*` text, lines joined by `\n`.
    pub logs: String,
    /// Host calls admitted to the runner.
    pub calls_started: u64,
    pub calls_succeeded: u64,
    pub calls_failed: u64,
    pub calls_cancelled: u64,
    /// Highest number of host callbacks running at once.
    pub peak_concurrency: usize,
    /// Host calls still pending when the script settled. Their futures were
    /// dropped before this outcome was returned.
    pub abandoned_calls: usize,
}

/// Structured failure of a script run.
#[derive(Debug, Error)]
pub enum ToolScriptError {
    #[error("invalid ToolScript limits: {message}")]
    InvalidLimits { message: String },
    #[error("ToolScript execution is not available on Android")]
    UnsupportedPlatform,
    #[error("script source is {actual} bytes, above the {limit}-byte limit")]
    SourceTooLarge { limit: usize, actual: usize },
    #[error("script arguments are {actual} bytes, above the {limit}-byte limit")]
    ScriptArgsTooLarge { limit: usize, actual: usize },
    #[error("script could not be compiled: {message}")]
    Compile { message: String, name: String },
    /// A JavaScript exception escaped the script. Per-tool failures are kept in
    /// `code`, `tool`, and `call_id` instead of being flattened into `message`.
    #[error("script failed: {message}")]
    Script {
        message: String,
        name: String,
        stack: Option<String>,
        code: Option<String>,
        tool: Option<String>,
        call_id: Option<u64>,
    },
    #[error("script result is {actual} bytes, above the {limit}-byte limit")]
    ResultTooLarge { limit: usize, actual: usize },
    #[error("script exceeded the {0:?} execution time limit")]
    Timeout(Duration),
    #[error("script execution was cancelled")]
    Cancelled,
    #[error("script exceeded the {limit}-byte memory limit: {message}")]
    MemoryLimit { limit: usize, message: String },
    #[error("ToolScript engine failure: {message}")]
    Engine { message: String },
}

/// Runs scripts against one host callback and one set of limits.
pub struct ToolScriptEngine {
    handler: Arc<dyn ToolCallHandler>,
    limits: ToolScriptLimits,
}

impl ToolScriptEngine {
    /// Creates an engine with [`ToolScriptLimits::default`].
    pub fn new(handler: Arc<dyn ToolCallHandler>) -> Self {
        Self {
            handler,
            limits: ToolScriptLimits::default(),
        }
    }

    /// Creates an engine after validating explicit limits.
    pub fn with_limits(
        handler: Arc<dyn ToolCallHandler>,
        limits: ToolScriptLimits,
    ) -> Result<Self, ToolScriptError> {
        limits.validate()?;
        Ok(Self { handler, limits })
    }

    pub fn limits(&self) -> &ToolScriptLimits {
        &self.limits
    }

    /// Executes one script and returns its JSON result.
    ///
    /// The returned future is **not `Send`**: it owns a QuickJS runtime on the
    /// calling thread. Do not move it to another executor thread. The future
    /// resolves only after the JS context and runtime are dropped, which drops
    /// every outstanding host future.
    pub async fn run(
        &self,
        source: &str,
        args: &Value,
        cancellation: &Cancellation,
    ) -> Result<ToolScriptOutcome, ToolScriptError> {
        #[cfg(target_os = "android")]
        {
            let _ = (source, args, cancellation);
            Err(ToolScriptError::UnsupportedPlatform)
        }
        #[cfg(not(target_os = "android"))]
        {
            engine::run_script(self, source, args, cancellation).await
        }
    }
}

/// Truncates to at most `limit` UTF-8 bytes on a character boundary.
pub(crate) fn truncate_utf8(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
