//! QuickJS runtime setup, separate prelude/user evaluation, and error
//! classification.
//!
//! The prelude and the user body are two distinct scripts. The user body never
//! shares a lexical scope with the trusted prelude closure, so raw host bridges,
//! configuration, and validator helpers are unreachable from script code. The
//! body receives only the four explicit globals documented in `runtime.js`.
//!
//! Only compiled on targets where `rquickjs` is linked. Everything stays on the
//! calling thread; there is no `Send`/`Sync` assertion or cross-thread hand-off.

use super::boundary::{js_to_json, BoundaryError};
use super::host::HostState;
use super::{
    truncate_utf8, Cancellation, ToolScriptEngine, ToolScriptError, ToolScriptLimits,
    ToolScriptOutcome,
};
use rquickjs::context::EvalOptions;
use rquickjs::convert::Coerced;
use rquickjs::prelude::{Async, Func};
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Ctx, Exception, Object, Promise,
    Value as JsValue,
};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

const PRELUDE_JS: &str = include_str!("runtime.js");
const USER_SOURCE_PREFIX: &str = "(async () => {\n";
const USER_SOURCE_SUFFIX: &str = "\n})()";

pub(super) async fn run_script(
    engine: &ToolScriptEngine,
    source: &str,
    args: &Value,
    cancellation: &Cancellation,
) -> Result<ToolScriptOutcome, ToolScriptError> {
    let limits = &engine.limits;
    if source.len() > limits.max_source_bytes {
        return Err(ToolScriptError::SourceTooLarge {
            limit: limits.max_source_bytes,
            actual: source.len(),
        });
    }
    let args_json = serde_json::to_string(args).map_err(|error| ToolScriptError::Engine {
        message: format!("cannot serialize script arguments: {error}"),
    })?;
    if args_json.len() > limits.max_script_args_bytes {
        return Err(ToolScriptError::ScriptArgsTooLarge {
            limit: limits.max_script_args_bytes,
            actual: args_json.len(),
        });
    }
    if cancellation.is_cancelled() {
        return Err(ToolScriptError::Cancelled);
    }

    let runtime = AsyncRuntime::new().map_err(|error| ToolScriptError::Engine {
        message: format!("cannot create the QuickJS runtime: {error}"),
    })?;
    runtime.set_memory_limit(limits.memory_limit_bytes).await;
    runtime
        .set_max_stack_size(limits.max_stack_size_bytes)
        .await;
    runtime
        .set_gc_threshold((limits.memory_limit_bytes / 4).max(1024 * 1024))
        .await;

    // The absolute deadline and the external cancellation flag are captured by
    // the engine's real interrupt hook. A Tokio timeout cannot preempt a
    // synchronous JavaScript loop, so this hook is the actual CPU-loop guard.
    let started = Instant::now();
    let deadline = limits.execution_timeout;
    let interrupt_cancellation = cancellation.clone();
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            interrupt_cancellation.is_cancelled() || started.elapsed() >= deadline
        })))
        .await;

    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|error| ToolScriptError::Engine {
            message: format!("cannot create the QuickJS context: {error}"),
        })?;
    let state = Arc::new(HostState::new(
        engine.handler.clone(),
        limits.clone(),
        cancellation.clone(),
        started,
    ));

    let result = {
        let execution = execute(
            &context,
            state.clone(),
            source.to_owned(),
            args_json,
            limits.clone(),
        );
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ToolScriptError::Cancelled),
            timed = tokio::time::timeout(limits.execution_timeout, execution) => match timed {
                Ok(result) => result,
                Err(_) => Err(ToolScriptError::Timeout(limits.execution_timeout)),
            },
        }
    };

    // Snapshot before shutdown: dropping the context/runtime cancels every
    // pending host future, which decrements the pending counter.
    let abandoned = state.pending.load(Ordering::SeqCst);

    // Dropping the context and runtime drops every spawned host future. The
    // runner never spawns host work elsewhere, so nothing survives this point.
    drop(context);
    drop(runtime);

    let logs = state.logs();
    match result {
        Ok(value) => Ok(ToolScriptOutcome {
            value,
            logs,
            calls_started: state.calls_started.load(Ordering::SeqCst),
            calls_succeeded: state.calls_succeeded.load(Ordering::SeqCst),
            calls_failed: state.calls_failed.load(Ordering::SeqCst),
            calls_cancelled: state.calls_cancelled.load(Ordering::SeqCst),
            peak_concurrency: state.peak.load(Ordering::SeqCst),
            abandoned_calls: abandoned,
        }),
        Err(error) => Err(error),
    }
}

async fn execute(
    context: &AsyncContext,
    state: Arc<HostState>,
    source: String,
    args_json: String,
    limits: ToolScriptLimits,
) -> Result<Value, ToolScriptError> {
    context
        .async_with(async move |ctx| {
            // Captured before any user code runs. Plain objects created by the
            // engine have this prototype; anything else is rejected by the
            // Rust boundary walker.
            let trusted_prototype = trusted_object_prototype(&ctx)?;
            install_raw_globals(&ctx, state.clone(), args_json, &limits)?;

            if let Err(error) =
                ctx.eval_with_options::<(), _>(PRELUDE_JS, eval_options("toolscript-prelude.js"))
            {
                let (name, message, _) = exception_details(&ctx);
                return Err(ToolScriptError::Engine {
                    message: truncate_utf8(
                        &format!("cannot initialize the ToolScript prelude ({name}): {message} ({error})"),
                        limits.max_diagnostic_bytes,
                    ),
                });
            }

            let user_source = assemble_user_source(&source);
            let promise: Promise =
                match ctx.eval_with_options(user_source, eval_options("toolscript-user.js")) {
                    Ok(promise) => promise,
                    Err(error) => return Err(classify_eval_error(&ctx, error, &state)),
                };

            let value: JsValue = match promise.into_future::<JsValue>().await.catch(&ctx) {
                Ok(value) => value,
                Err(caught) => return Err(classify_caught(caught, &state)),
            };
            convert_script_result(value, &trusted_prototype, &limits)
        })
        .await
}

fn eval_options(filename: &str) -> EvalOptions {
    let mut options = EvalOptions::default();
    options.filename = Some(filename.to_owned());
    options
}

fn assemble_user_source(body: &str) -> String {
    format!("{USER_SOURCE_PREFIX}{body}{USER_SOURCE_SUFFIX}")
}

fn trusted_object_prototype<'js>(ctx: &Ctx<'js>) -> Result<JsValue<'js>, ToolScriptError> {
    let object: Object = ctx.globals().get("Object").map_err(engine_error)?;
    object.get::<_, JsValue>("prototype").map_err(engine_error)
}

fn plain_object_prototype<'js>(ctx: &Ctx<'js>) -> Result<JsValue<'js>, rquickjs::Error> {
    let probe = Object::new(ctx.clone())?;
    Ok(match probe.get_prototype() {
        Some(prototype) => prototype.into_value(),
        None => JsValue::new_null(ctx.clone()),
    })
}

fn install_raw_globals<'js>(
    ctx: &Ctx<'js>,
    state: Arc<HostState>,
    args_json: String,
    limits: &ToolScriptLimits,
) -> Result<(), ToolScriptError> {
    let globals = ctx.globals();

    let call_state = state.clone();
    let max_depth = limits.max_boundary_depth;
    let max_args_bytes = limits.max_args_bytes;
    globals
        .set(
            "__toolscript_host_call",
            Func::from(Async(
                move |ctx: Ctx<'js>, name: String, args: JsValue<'js>| {
                    let state = call_state.clone();
                    async move {
                        if name.is_empty() || name.len() > state.limits.max_call_name_bytes {
                            return Ok::<String, rquickjs::Error>(state.error_reply(
                                "invalid_tool_name",
                                "tool name is out of range",
                                &name,
                                None,
                                None,
                            ));
                        }
                        // Do not capture a JS value in this Rust closure: a
                        // captured Ctx/Value would keep the context alive past
                        // runtime shutdown. A fresh plain object always has the
                        // realm's intrinsic Object.prototype, even if the script
                        // replaced the global `Object` constructor.
                        let trusted_prototype = match plain_object_prototype(&ctx) {
                            Ok(prototype) => prototype,
                            Err(_) => {
                                return Ok::<String, rquickjs::Error>(state.error_reply(
                                    "engine_closed",
                                    "cannot read the realm Object.prototype",
                                    &name,
                                    None,
                                    None,
                                ))
                            }
                        };
                        let label = format!("arguments for {}", truncate_utf8(&name, 64));
                        let args =
                            match js_to_json(args, &trusted_prototype, max_depth, max_args_bytes) {
                                Ok(args) => args,
                                Err(error) => {
                                    let code = match &error {
                                        BoundaryError::TooLarge { .. } => "args_too_large",
                                        _ => error.code(),
                                    };
                                    let message = error.message(&label);
                                    return Ok::<String, rquickjs::Error>(
                                        state.error_reply(code, &message, &name, None, None),
                                    );
                                }
                            };
                        Ok::<String, rquickjs::Error>(state.host_call(name, args).await)
                    }
                },
            )),
        )
        .map_err(engine_error)?;

    let log_state = state.clone();
    globals
        .set(
            "__toolscript_log",
            Func::from(move |text: String| log_state.push_log(&text)),
        )
        .map_err(engine_error)?;

    globals
        .set("__toolscript_args", args_json)
        .map_err(engine_error)?;
    let config = json!({
        "maxCallNameBytes": limits.max_call_name_bytes,
        "maxLogBytes": limits.max_log_bytes,
    });
    globals
        .set("__toolscript_config", config.to_string())
        .map_err(engine_error)?;
    Ok(())
}

fn engine_error(error: rquickjs::Error) -> ToolScriptError {
    ToolScriptError::Engine {
        message: format!("cannot install the ToolScript runtime: {error}"),
    }
}

fn convert_script_result<'js>(
    value: JsValue<'js>,
    trusted_prototype: &JsValue<'js>,
    limits: &ToolScriptLimits,
) -> Result<Value, ToolScriptError> {
    if value.is_undefined() {
        return Ok(Value::Null);
    }
    match js_to_json(
        value,
        trusted_prototype,
        limits.max_boundary_depth,
        limits.max_script_result_bytes,
    ) {
        Ok(value) => Ok(value),
        Err(BoundaryError::TooLarge { limit, actual }) => {
            Err(ToolScriptError::ResultTooLarge { limit, actual })
        }
        Err(error) => {
            let message = error.message("script result");
            Err(ToolScriptError::Script {
                message: truncate_utf8(&message, limits.max_diagnostic_bytes),
                name: "ToolScriptError".to_owned(),
                stack: None,
                code: Some(error.code().to_owned()),
                tool: None,
                call_id: None,
            })
        }
    }
}

fn classify_eval_error(
    ctx: &Ctx<'_>,
    error: rquickjs::Error,
    state: &HostState,
) -> ToolScriptError {
    if state.cancellation.is_cancelled() {
        return ToolScriptError::Cancelled;
    }
    if state.started_at.elapsed() >= state.limits.execution_timeout {
        return ToolScriptError::Timeout(state.limits.execution_timeout);
    }
    if matches!(error, rquickjs::Error::Allocation) {
        return ToolScriptError::MemoryLimit {
            limit: state.limits.memory_limit_bytes,
            message: "QuickJS ran out of memory while compiling the script".to_owned(),
        };
    }
    let (name, message, _stack) = exception_details(ctx);
    if is_out_of_memory(&message) {
        return ToolScriptError::MemoryLimit {
            limit: state.limits.memory_limit_bytes,
            message: truncate_utf8(&message, state.limits.max_diagnostic_bytes),
        };
    }
    ToolScriptError::Compile {
        message: truncate_utf8(&message, state.limits.max_diagnostic_bytes),
        name: truncate_utf8(&name, 64),
    }
}

fn classify_caught<'js>(caught: CaughtError<'js>, state: &HostState) -> ToolScriptError {
    if state.cancellation.is_cancelled() {
        return ToolScriptError::Cancelled;
    }
    if state.started_at.elapsed() >= state.limits.execution_timeout {
        return ToolScriptError::Timeout(state.limits.execution_timeout);
    }
    match caught {
        CaughtError::Exception(exception) => {
            let name = exception_name(&exception);
            let message = exception.message().unwrap_or_default();
            let stack = exception.stack();
            if is_out_of_memory(&message) {
                return ToolScriptError::MemoryLimit {
                    limit: state.limits.memory_limit_bytes,
                    message: truncate_utf8(&message, state.limits.max_diagnostic_bytes),
                };
            }
            let code =
                exception_string_prop(&exception, "code").map(|code| truncate_utf8(&code, 64));
            let tool = exception_string_prop(&exception, "tool")
                .map(|tool| truncate_utf8(&tool, state.limits.max_call_name_bytes));
            let call_id = exception_call_id(&exception);
            ToolScriptError::Script {
                message: truncate_utf8(&message, state.limits.max_diagnostic_bytes),
                name: truncate_utf8(&name, 64),
                stack: stack.map(|stack| truncate_utf8(&stack, state.limits.max_diagnostic_bytes)),
                code,
                tool,
                call_id,
            }
        }
        CaughtError::Value(value) => {
            let message = describe_thrown_value(&value);
            ToolScriptError::Script {
                message: truncate_utf8(&message, state.limits.max_diagnostic_bytes),
                name: "ThrownValue".to_owned(),
                stack: None,
                code: None,
                tool: None,
                call_id: None,
            }
        }
        CaughtError::Error(error) => {
            if matches!(error, rquickjs::Error::Allocation) {
                ToolScriptError::MemoryLimit {
                    limit: state.limits.memory_limit_bytes,
                    message: "QuickJS ran out of memory".to_owned(),
                }
            } else {
                ToolScriptError::Engine {
                    message: format!("JavaScript engine error: {error}"),
                }
            }
        }
    }
}

fn describe_thrown_value(value: &JsValue<'_>) -> String {
    if value.is_null() {
        "null".to_owned()
    } else if value.is_undefined() {
        "undefined".to_owned()
    } else if value.is_bool() {
        value.as_bool().unwrap_or(false).to_string()
    } else if value.is_int() {
        value.as_int().unwrap_or(0).to_string()
    } else if value.is_float() {
        value.as_float().unwrap_or(0.0).to_string()
    } else if value.is_string() {
        value
            .as_string()
            .and_then(|string| string.to_string().ok())
            .unwrap_or_else(|| "<unreadable string>".to_owned())
    } else {
        format!("non-Error {} value", value.type_of().as_str())
    }
}

fn exception_details(ctx: &Ctx<'_>) -> (String, String, Option<String>) {
    let value = ctx.catch();
    if let Some(object) = value.as_object() {
        if let Some(exception) = Exception::from_object(object.clone()) {
            return (
                exception_name(&exception),
                exception.message().unwrap_or_default(),
                exception.stack(),
            );
        }
    }
    let message = match ctx.json_stringify(value) {
        Ok(Some(text)) => text
            .to_string()
            .unwrap_or_else(|_| "script failed".to_owned()),
        _ => {
            if ctx.has_exception() {
                let _ = ctx.catch();
            }
            "script failed".to_owned()
        }
    };
    ("Error".to_owned(), message, None)
}

fn exception_name<'js>(exception: &Exception<'js>) -> String {
    exception
        .get::<_, Option<Coerced<String>>>("name")
        .ok()
        .flatten()
        .map(|name| name.0)
        .unwrap_or_else(|| "Error".to_owned())
}

fn exception_string_prop<'js>(exception: &Exception<'js>, name: &str) -> Option<String> {
    exception
        .get::<_, Option<Coerced<String>>>(name)
        .ok()
        .flatten()
        .map(|value| value.0)
}

fn exception_call_id<'js>(exception: &Exception<'js>) -> Option<u64> {
    let value = exception
        .get::<_, Option<JsValue>>("callId")
        .ok()
        .flatten()?;
    let number = value.as_number()?;
    if number.is_finite() && number >= 0.0 && number.fract() == 0.0 && number <= u64::MAX as f64 {
        Some(number as u64)
    } else {
        None
    }
}

fn is_out_of_memory(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("out of memory")
        || lower.contains("memory limit")
        || lower.contains("allocation failed")
}
