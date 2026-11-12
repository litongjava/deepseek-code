//! Real-QuickJS tests for the bounded ToolScript foundation.
//!
//! These tests deliberately use fake host callbacks. They prove the embedded
//! engine, boundary validation, limits, interruption, cancellation, and the
//! drop contract. They do not prove executor permissions, hooks, history, or a
//! model-visible tool.

use super::*;
use serde_json::json;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[derive(Clone, Debug)]
struct Record {
    call_id: u64,
    name: String,
    args: Value,
}

enum Mode {
    Echo,
    Fixed(Value),
    Error {
        code: &'static str,
        message: &'static str,
    },
    HangOnSlow {
        started: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
        effect: Arc<AtomicUsize>,
    },
}

struct RecordingHandler {
    mode: Mode,
    delay: Duration,
    records: Mutex<Vec<Record>>,
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl RecordingHandler {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            delay: Duration::ZERO,
            records: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn records(&self) -> Vec<Record> {
        self.records.lock().unwrap().clone()
    }
}

struct ActiveCounter<'a>(&'a AtomicUsize);

impl Drop for ActiveCounter<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ToolCallHandler for RecordingHandler {
    async fn call(
        &self,
        request: ToolCallRequest,
        cancellation: Cancellation,
    ) -> Result<Value, ToolCallError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let _active = ActiveCounter(&self.active);
        self.records.lock().unwrap().push(Record {
            call_id: request.call_id,
            name: request.name.clone(),
            args: request.args.clone(),
        });
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        match &self.mode {
            Mode::Echo => Ok(request.args),
            Mode::Fixed(value) => Ok(value.clone()),
            Mode::Error { code, message } => Err(ToolCallError::new(*code, *message)),
            Mode::HangOnSlow {
                started,
                dropped,
                effect,
            } => {
                if request.name != "Slow" {
                    return Ok(request.args);
                }
                started.store(true, Ordering::SeqCst);
                let _dropped = DropFlag(dropped.clone());
                tokio::select! {
                    _ = cancellation.cancelled() => {
                        Err(ToolCallError::new("cancelled", "cancelled in fixture"))
                    }
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {
                        effect.fetch_add(1, Ordering::SeqCst);
                        Ok(json!({"late": true}))
                    }
                }
            }
        }
    }
}

fn build_engine(
    handler: Arc<RecordingHandler>,
    limits: ToolScriptLimits,
) -> (ToolScriptEngine, Arc<RecordingHandler>) {
    let engine = ToolScriptEngine::with_limits(handler.clone(), limits).unwrap();
    (engine, handler)
}

fn default_limits() -> ToolScriptLimits {
    let mut limits = ToolScriptLimits::default();
    // Keep tests independent of the production 5-minute budget.
    limits.execution_timeout = Duration::from_secs(10);
    limits.call_timeout = Duration::from_secs(5);
    limits
}

#[test]
fn real_quickjs_runs_sequential_calls_with_conditions_and_transforms() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, handler) = build_engine(handler, default_limits());
    let source = r#"
        const values = [];
        for (const i of [1, 2, 3]) {
            const reply = await tools.call("Echo", { i });
            values.push(reply.i * 2);
        }
        const mapped = values.map((value) => value + 1).filter((value) => value > 3);
        return {
            values,
            mapped,
            branch: values.length === 3 ? "three" : "other",
            seed: args.seed,
        };
    "#;

    let outcome = block_on(engine.run(source, &json!({"seed": 7}), &Cancellation::new()))
        .expect("script should succeed");

    assert_eq!(
        outcome.value,
        json!({"values": [2, 4, 6], "mapped": [5, 7], "branch": "three", "seed": 7})
    );
    assert_eq!(outcome.calls_started, 3);
    assert_eq!(outcome.calls_succeeded, 3);
    assert_eq!(outcome.peak_concurrency, 1);
    assert_eq!(outcome.abandoned_calls, 0);

    let records = handler.records();
    let indexes: Vec<u64> = records
        .iter()
        .map(|record| record.args["i"].as_u64().unwrap())
        .collect();
    assert_eq!(indexes, vec![1, 2, 3]);
    let call_ids: Vec<u64> = records.iter().map(|record| record.call_id).collect();
    assert_eq!(call_ids, vec![1, 2, 3]);
}

#[test]
fn promise_all_preserves_result_order_and_bounds_peak_concurrency() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo).with_delay(Duration::from_millis(20)));
    let mut limits = default_limits();
    limits.max_concurrent_calls = 2;
    limits.max_pending_calls = 8;
    let (engine, handler) = build_engine(handler, limits);
    let source = r#"
        const replies = await Promise.all(
            [0, 1, 2, 3, 4, 5].map((i) => tools.call("Echo", { i }))
        );
        return replies.map((reply) => reply.i);
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("script should succeed");

    assert_eq!(outcome.value, json!([0, 1, 2, 3, 4, 5]));
    assert_eq!(outcome.calls_started, 6);
    assert_eq!(outcome.calls_succeeded, 6);
    assert_eq!(outcome.peak_concurrency, 2);
    assert_eq!(handler.peak.load(Ordering::SeqCst), 2);
    assert_eq!(handler.records().len(), 6);
}

#[test]
fn caught_tool_errors_keep_code_tool_and_call_id() {
    let handler = Arc::new(RecordingHandler::new(Mode::Error {
        code: "permission_denied",
        message: "fixture denied the call",
    }));
    let (engine, _handler) = build_engine(handler, default_limits());
    let source = r#"
        try {
            await tools.call("Bash", { command: "true" });
            return { unexpected: true };
        } catch (error) {
            return {
                code: error.code,
                tool: error.tool,
                callId: error.callId,
                name: error.name,
                message: error.message,
            };
        }
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("script should catch the tool error");

    assert_eq!(
        outcome.value,
        json!({
            "code": "permission_denied",
            "tool": "Bash",
            "callId": 1,
            "name": "ToolScriptError",
            "message": "fixture denied the call",
        })
    );
    assert_eq!(outcome.calls_failed, 1);
}

#[test]
fn uncaught_tool_error_is_reported_without_hiding_the_failure() {
    let handler = Arc::new(RecordingHandler::new(Mode::Error {
        code: "permission_denied",
        message: "fixture denied the call",
    }));
    let (engine, _handler) = build_engine(handler, default_limits());
    let source = r#"await tools.call("Edit", { path: "secret.txt" }); return "unreachable";"#;

    let error = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect_err("the tool error must escape the script");

    match error {
        ToolScriptError::Script {
            code,
            tool,
            call_id,
            message,
            ..
        } => {
            assert_eq!(code.as_deref(), Some("permission_denied"));
            assert_eq!(tool.as_deref(), Some("Edit"));
            assert_eq!(call_id, Some(1));
            assert!(message.contains("fixture denied the call"));
        }
        other => panic!("expected a structured script error, got {other:?}"),
    }
}

#[test]
fn script_throw_is_a_structured_javascript_error() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, _handler) = build_engine(handler, default_limits());
    let source = r#"throw new Error("fixture boom");"#;

    let error = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect_err("the throw must fail the run");

    match error {
        ToolScriptError::Script {
            name,
            message,
            stack,
            code,
            tool,
            call_id,
        } => {
            assert_eq!(name, "Error");
            assert_eq!(message, "fixture boom");
            let stack = stack.expect("a JavaScript stack should be present");
            assert!(!stack.is_empty());
            assert!(stack.len() <= 4096);
            assert_eq!(code, None);
            assert_eq!(tool, None);
            assert_eq!(call_id, None);
        }
        other => panic!("expected a structured script error, got {other:?}"),
    }
}

#[test]
fn non_json_arguments_are_rejected_before_the_host_callback() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, handler) = build_engine(handler, default_limits());
    let source = r#"
        const makers = [
            () => undefined,
            () => ({ fn: function () {} }),
            () => ({ big: 1n }),
            () => ({ number: NaN }),
            () => { const value = {}; value.self = value; return value; },
            () => new Date(0),
        ];
        const codes = [];
        for (const make of makers) {
            try {
                await tools.call("Echo", make());
                codes.push("accepted");
            } catch (error) {
                codes.push(error.code);
            }
        }
        return codes;
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("the script itself should succeed");

    assert_eq!(
        outcome.value,
        json!([
            "non_json_value",
            "non_json_value",
            "non_json_value",
            "invalid_number",
            "cyclic_value",
            "non_json_value",
        ])
    );
    assert_eq!(handler.records().len(), 0);
    assert_eq!(outcome.calls_started, 0);
}

#[test]
fn cyclic_result_is_rejected_with_a_bounded_diagnostic() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let mut limits = default_limits();
    limits.max_diagnostic_bytes = 128;
    let (engine, _handler) = build_engine(handler, limits);
    let source = r#"const value = {}; value.self = value; return value;"#;

    let error = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect_err("a cyclic result must be rejected");

    match error {
        ToolScriptError::Script { code, message, .. } => {
            assert_eq!(code.as_deref(), Some("cyclic_value"));
            assert!(message.len() <= 128);
        }
        other => panic!("expected a boundary script error, got {other:?}"),
    }
}

#[test]
fn oversized_diagnostics_are_bounded() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let mut limits = default_limits();
    limits.max_diagnostic_bytes = 64;
    let (engine, _handler) = build_engine(handler, limits);
    let source = r#"throw new Error("x".repeat(10000));"#;

    let error = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect_err("the throw must fail the run");

    match error {
        ToolScriptError::Script { message, stack, .. } => {
            assert!(message.len() <= 64, "message was {} bytes", message.len());
            assert!(stack.unwrap_or_default().len() <= 64);
        }
        other => panic!("expected a structured script error, got {other:?}"),
    }
}

#[test]
fn unicode_round_trips_through_arguments_and_results() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, _handler) = build_engine(handler, default_limits());
    let source = r#"
        const reply = await tools.call("Echo", { text: "中文 😀 café 日本語" });
        return { text: reply.text, extra: reply.text + "✓" };
    "#;

    let outcome = block_on(engine.run(source, &json!({"名字": "世界😀"}), &Cancellation::new()))
        .expect("unicode should round-trip");

    assert_eq!(
        outcome.value,
        json!({"text": "中文 😀 café 日本語", "extra": "中文 😀 café 日本語✓"})
    );
}

#[test]
fn numeric_boundary_preserves_values_in_arguments() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, handler) = build_engine(handler, default_limits());
    let source = r#"
        const numbers = {
            safeMax: 9007199254740991,
            safeMin: -9007199254740991,
            twoPow53: 9007199254740992,
            negTwoPow53: -9007199254740992,
            i64Min: -9223372036854775808,
            belowI64Min: -9223372036854777856,
            belowI64Max: 9223372036854774784,
            twoPow63: 9223372036854775808,
            aboveI64: 9223372036854777856,
            twoPow64: 18446744073709551616,
            huge: 1e20,
            fraction: -1.5,
            negativeZero: -0,
        };
        await tools.call("Echo", numbers);
        return null;
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("numeric arguments should cross the boundary");
    assert_eq!(outcome.value, json!(null));
    let records = handler.records();
    assert_eq!(records.len(), 1);
    let args = &records[0].args;

    // Values exactly representable as i64 stay integers.
    assert_eq!(args["safeMax"].as_i64(), Some(9_007_199_254_740_991));
    assert_eq!(args["safeMin"].as_i64(), Some(-9_007_199_254_740_991));
    assert_eq!(args["twoPow53"].as_i64(), Some(9_007_199_254_740_992));
    assert_eq!(args["negTwoPow53"].as_i64(), Some(-9_007_199_254_740_992));
    assert_eq!(args["i64Min"].as_i64(), Some(i64::MIN));
    assert_eq!(
        args["belowI64Max"].as_i64(),
        Some(9_223_372_036_854_774_784)
    );

    // 2^63 is not an i64; `i64::MAX as f64` rounds up to it, so a saturating
    // cast would silently turn it into 9223372036854775807.
    assert!(args["twoPow63"].is_f64());
    assert_eq!(args["twoPow63"].as_f64(), Some(9_223_372_036_854_775_808.0));
    assert!(args["aboveI64"].is_f64());
    assert_eq!(args["aboveI64"].as_f64(), Some(9_223_372_036_854_777_856.0));
    assert!(args["belowI64Min"].is_f64());
    assert_eq!(
        args["belowI64Min"].as_f64(),
        Some(-9_223_372_036_854_777_856.0)
    );
    assert_eq!(
        args["twoPow64"].as_f64(),
        Some(18_446_744_073_709_551_616.0)
    );
    assert_eq!(args["huge"].as_f64(), Some(1e20));
    assert_eq!(args["fraction"].as_f64(), Some(-1.5));
    assert_eq!(args["negativeZero"].as_i64(), Some(0));

    let serialized = serde_json::to_string(args).unwrap();
    assert!(
        !serialized.contains("9223372036854775807"),
        "a boundary value was saturated: {serialized}"
    );
}

#[test]
fn numeric_boundary_preserves_values_in_script_results() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, _handler) = build_engine(handler, default_limits());
    let source = r#"
        return {
            twoPow53: 9007199254740992,
            i64Min: -9223372036854775808,
            belowI64Max: 9223372036854774784,
            twoPow63: 9223372036854775808,
            aboveI64: 9223372036854777856,
            huge: 1e20,
            fraction: 0.25,
        };
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("numeric results should cross the boundary");

    assert_eq!(
        outcome.value["twoPow53"].as_i64(),
        Some(9_007_199_254_740_992)
    );
    assert_eq!(outcome.value["i64Min"].as_i64(), Some(i64::MIN));
    assert_eq!(
        outcome.value["belowI64Max"].as_i64(),
        Some(9_223_372_036_854_774_784)
    );
    assert!(outcome.value["twoPow63"].is_f64());
    assert_eq!(
        outcome.value["twoPow63"].as_f64(),
        Some(9_223_372_036_854_775_808.0)
    );
    assert_eq!(
        outcome.value["aboveI64"].as_f64(),
        Some(9_223_372_036_854_777_856.0)
    );
    assert_eq!(outcome.value["huge"].as_f64(), Some(1e20));
    assert_eq!(outcome.value["fraction"].as_f64(), Some(0.25));

    let serialized = serde_json::to_string(&outcome.value).unwrap();
    assert!(
        !serialized.contains("9223372036854775807"),
        "a result value was saturated: {serialized}"
    );
}

#[test]
fn sparse_array_holes_are_rejected_despite_inherited_values() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, handler) = build_engine(handler, default_limits());
    let source = r#"
        // A hole must not inherit Array.prototype[0] and pass as [7].
        Array.prototype[0] = 7;
        const holey = new Array(1);
        const literal = [1, , 3];
        const codes = [];
        for (const value of [holey, literal]) {
            try {
                await tools.call("Echo", { value });
                codes.push("accepted");
            } catch (error) {
                codes.push(error.code);
            }
        }
        delete Array.prototype[0];
        const dense = await tools.call("Echo", { value: [1, 2, 3] });
        return { codes, dense: dense.value };
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("the script should catch the sparse-array errors");

    assert_eq!(
        outcome.value,
        json!({"codes": ["non_json_value", "non_json_value"], "dense": [1, 2, 3]})
    );
    let records = handler.records();
    assert_eq!(records.len(), 1, "only the dense array may reach the host");
    assert_eq!(records[0].args, json!({"value": [1, 2, 3]}));
}

#[test]
fn getter_mutation_cannot_resurrect_an_argument_hole_from_the_prototype() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, handler) = build_engine(handler, default_limits());
    let source = r#"
        const mutated = [0, 1];
        Object.defineProperty(mutated, "0", {
            get() {
                delete mutated[1];
                return 0;
            },
        });
        Array.prototype[1] = 7;
        let code;
        try {
            await tools.call("Echo", { value: mutated });
            code = "accepted";
        } catch (error) {
            code = error.code;
        }
        delete Array.prototype[1];
        return code;
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("the script should catch the sparse-array error");

    // The getter runs while index 0 is read and deletes index 1. The boundary
    // must re-check ownership immediately before reading index 1 rather than
    // falling back to Array.prototype[1].
    assert_eq!(outcome.value, json!("non_json_value"));
    assert_eq!(handler.records().len(), 0, "no host call may be admitted");
}

#[test]
fn getter_mutation_cannot_resurrect_a_result_hole_from_the_prototype() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, _handler) = build_engine(handler, default_limits());
    let source = r#"
        const mutated = [0, 1];
        Object.defineProperty(mutated, "0", {
            get() {
                delete mutated[1];
                return 0;
            },
        });
        Array.prototype[1] = 7;
        return { value: mutated };
    "#;

    let error = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect_err("the result must be rejected");

    match error {
        ToolScriptError::Script { code, message, .. } => {
            assert_eq!(code.as_deref(), Some("non_json_value"));
            assert!(message.contains("sparse array"), "{message}");
        }
        other => panic!("expected a sparse-array script error, got {other:?}"),
    }
}

#[test]
fn out_of_range_numeric_property_on_an_array_is_ignored() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, handler) = build_engine(handler, default_limits());
    let source = r#"
        const value = [1, 2, 3];
        // 2^32-1 is not an array index, so it is an ordinary extra property and
        // JSON.stringify (and this boundary) ignores it.
        value["4294967295"] = "extra";
        const reply = await tools.call("Echo", { value });
        return reply.value;
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("an out-of-range numeric property must not be treated as a hole");

    assert_eq!(outcome.value, json!([1, 2, 3]));
    let records = handler.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].args, json!({"value": [1, 2, 3]}));
}

#[test]
fn proxy_values_are_rejected_before_traps_or_enumeration() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, handler) = build_engine(handler, default_limits());
    let source = r#"
        let traps = 0;
        const arrayProxy = new Proxy([1, 2, 3], {
            get(target, key, receiver) {
                traps++;
                return Reflect.get(target, key, receiver);
            },
            getOwnPropertyDescriptor(target, key) {
                traps++;
                return Reflect.getOwnPropertyDescriptor(target, key);
            },
            ownKeys(target) {
                traps++;
                return Reflect.ownKeys(target);
            },
        });
        const objectProxy = new Proxy({ a: 1 }, {
            get(target, key, receiver) {
                traps++;
                return Reflect.get(target, key, receiver);
            },
        });

        const codes = [];
        for (const value of [arrayProxy, objectProxy]) {
            try {
                await tools.call("Echo", { value });
                codes.push("accepted");
            } catch (error) {
                codes.push(error.code);
            }
        }
        const dense = await tools.call("Echo", { value: [1, 2, 3] });
        return { traps, codes, dense: dense.value };
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("the script should catch the proxy errors");

    assert_eq!(
        outcome.value,
        json!({"traps": 0, "codes": ["non_json_value", "non_json_value"], "dense": [1, 2, 3]})
    );
    let records = handler.records();
    assert_eq!(records.len(), 1, "only the dense array may reach the host");
}

#[test]
fn infinite_cpu_loop_is_stopped_by_the_engine_interrupt_hook() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let mut limits = default_limits();
    limits.execution_timeout = Duration::from_millis(150);
    limits.call_timeout = Duration::from_millis(100);
    let (engine, _handler) = build_engine(handler, limits);

    let started = Instant::now();
    let error = block_on(engine.run(
        "let spins = 0; while (true) { spins++; }",
        &json!(null),
        &Cancellation::new(),
    ))
    .expect_err("the loop must be interrupted");

    // A Tokio timeout cannot fire while the synchronous loop owns the thread.
    // Only the QuickJS interrupt handler can end this run.
    assert!(matches!(error, ToolScriptError::Timeout(_)));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn external_cancellation_stops_a_cpu_bound_loop() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let mut limits = default_limits();
    limits.execution_timeout = Duration::from_secs(30);
    limits.call_timeout = Duration::from_secs(1);
    let (engine, _handler) = build_engine(handler, limits);
    let cancellation = Cancellation::new();
    let thread_token = cancellation.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        thread_token.cancel();
    });

    let started = Instant::now();
    let error = block_on(engine.run(
        "let spins = 0; while (true) { spins++; }",
        &json!(null),
        &cancellation,
    ))
    .expect_err("cancellation must stop the loop");
    canceller.join().unwrap();

    // The interrupt handler observes the external atomic flag from another
    // thread; the Tokio timer cannot run during the blocking loop.
    assert!(matches!(error, ToolScriptError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn memory_pressure_returns_a_bounded_engine_error() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let mut limits = default_limits();
    limits.memory_limit_bytes = 4 * 1024 * 1024;
    limits.execution_timeout = Duration::from_secs(5);
    limits.call_timeout = Duration::from_secs(1);
    let (engine, _handler) = build_engine(handler, limits);
    let source = r#"
        const chunks = [];
        while (true) { chunks.push("x".repeat(256 * 1024)); }
    "#;

    let error = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect_err("memory pressure must fail the run");

    match error {
        ToolScriptError::MemoryLimit { limit, message } => {
            assert_eq!(limit, 4 * 1024 * 1024);
            assert!(message.len() <= 4096);
        }
        other => panic!("expected a memory-limit error, got {other:?}"),
    }
}

#[test]
fn cancellation_during_a_waiting_host_call_drops_the_future() {
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let effect = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(RecordingHandler::new(Mode::HangOnSlow {
        started: started.clone(),
        dropped: dropped.clone(),
        effect: effect.clone(),
    }));
    let mut limits = default_limits();
    limits.execution_timeout = Duration::from_secs(30);
    limits.call_timeout = Duration::from_secs(30);
    let (engine, _handler) = build_engine(handler, limits);
    let cancellation = Cancellation::new();
    let thread_token = cancellation.clone();
    let wait_for_start = started.clone();
    let canceller = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !wait_for_start.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        thread_token.cancel();
    });

    let result = block_on(engine.run(
        r#"return await tools.call("Slow", {});"#,
        &json!(null),
        &cancellation,
    ));
    canceller.join().unwrap();

    assert!(matches!(result, Err(ToolScriptError::Cancelled)));
    assert!(started.load(Ordering::SeqCst));
    assert!(
        dropped.load(Ordering::SeqCst),
        "the host future must be dropped"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(effect.load(Ordering::SeqCst), 0, "no late host effect");
}

#[test]
fn host_call_timeout_drops_the_future() {
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let effect = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(RecordingHandler::new(Mode::HangOnSlow {
        started: started.clone(),
        dropped: dropped.clone(),
        effect: effect.clone(),
    }));
    let mut limits = default_limits();
    limits.call_timeout = Duration::from_millis(50);
    limits.execution_timeout = Duration::from_secs(5);
    let (engine, _handler) = build_engine(handler, limits);
    let source = r#"
        try {
            await tools.call("Slow", {});
            return "late";
        } catch (error) {
            return error.code;
        }
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("the script catches the timeout");

    assert_eq!(outcome.value, json!("call_timeout"));
    assert!(started.load(Ordering::SeqCst));
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(effect.load(Ordering::SeqCst), 0);
}

#[test]
fn unawaited_call_is_abandoned_without_a_late_effect() {
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let effect = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(RecordingHandler::new(Mode::HangOnSlow {
        started: started.clone(),
        dropped: dropped.clone(),
        effect: effect.clone(),
    }));
    let (engine, _handler) = build_engine(handler, default_limits());
    let source = r#"
        tools.call("Slow", {});
        await tools.call("Echo", { ok: true });
        return "done";
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("the top-level script finishes");

    assert_eq!(outcome.value, json!("done"));
    assert!(started.load(Ordering::SeqCst));
    assert_eq!(outcome.abandoned_calls, 1);
    assert!(dropped.load(Ordering::SeqCst));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(effect.load(Ordering::SeqCst), 0);
}

#[test]
fn no_filesystem_network_process_or_timer_globals_are_available() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, _handler) = build_engine(handler, default_limits());
    let source = r#"
        const names = [
            "require", "process", "fetch", "setTimeout", "setInterval",
            "XMLHttpRequest", "WebSocket", "Deno", "Bun",
            "__toolscript_host_call", "__toolscript_log",
            "__toolscript_config", "__toolscript_args",
        ];
        const report = {};
        for (const name of names) report[name] = typeof globalThis[name];
        report.toolsCall = typeof tools.call;
        report.log = typeof log;
        report.consoleLog = typeof console.log;
        return report;
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("inspection script should succeed");

    let report = outcome.value.as_object().unwrap();
    for name in [
        "require",
        "process",
        "fetch",
        "setTimeout",
        "setInterval",
        "XMLHttpRequest",
        "WebSocket",
        "Deno",
        "Bun",
        "__toolscript_host_call",
        "__toolscript_log",
        "__toolscript_config",
        "__toolscript_args",
    ] {
        assert_eq!(report[name], json!("undefined"), "{name} must be absent");
    }
    assert_eq!(report["toolsCall"], json!("function"));
    assert_eq!(report["log"], json!("function"));
    assert_eq!(report["consoleLog"], json!("function"));
}

#[test]
fn deep_recursion_hits_the_stack_limit() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let mut limits = default_limits();
    limits.max_stack_size_bytes = 256 * 1024;
    let (engine, _handler) = build_engine(handler, limits);
    let source = "function dive(n) { return dive(n + 1); } dive(0);";

    let error = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect_err("deep recursion must fail");

    match error {
        ToolScriptError::Script { name, message, .. } => {
            assert_eq!(name, "RangeError");
            assert!(
                message.to_ascii_lowercase().contains("stack"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected a RangeError script failure, got {other:?}"),
    }
}

#[test]
fn source_and_script_argument_limits_are_enforced() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let mut limits = default_limits();
    limits.max_source_bytes = 32;
    let (engine, _handler) = build_engine(handler, limits);
    let source = "0".repeat(33);
    let error = block_on(engine.run(&source, &json!(null), &Cancellation::new()))
        .expect_err("oversized source must be rejected");
    assert!(matches!(
        error,
        ToolScriptError::SourceTooLarge {
            limit: 32,
            actual: 33
        }
    ));

    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let mut limits = default_limits();
    limits.max_script_args_bytes = 8;
    let (engine, _handler) = build_engine(handler, limits);
    let error = block_on(engine.run(
        "return args;",
        &json!({"padding": "1234567890"}),
        &Cancellation::new(),
    ))
    .expect_err("oversized script arguments must be rejected");
    assert!(matches!(
        error,
        ToolScriptError::ScriptArgsTooLarge { limit: 8, .. }
    ));
}

#[test]
fn invalid_limits_are_rejected() {
    let handler: Arc<dyn ToolCallHandler> = Arc::new(RecordingHandler::new(Mode::Echo));
    let mut limits = ToolScriptLimits::default();
    limits.max_concurrent_calls = 0;
    assert!(matches!(
        ToolScriptEngine::with_limits(handler.clone(), limits),
        Err(ToolScriptError::InvalidLimits { .. })
    ));

    let mut limits = ToolScriptLimits::default();
    limits.call_timeout = limits.execution_timeout + Duration::from_secs(1);
    assert!(matches!(
        ToolScriptEngine::with_limits(handler, limits),
        Err(ToolScriptError::InvalidLimits { .. })
    ));
}

#[test]
fn per_call_argument_result_and_log_limits_are_enforced() {
    let handler = Arc::new(RecordingHandler::new(Mode::Fixed(json!("x".repeat(100)))));
    let mut limits = default_limits();
    limits.max_args_bytes = 16;
    limits.max_call_result_bytes = 16;
    limits.max_log_bytes = 16;
    let (engine, _handler) = build_engine(handler, limits);
    let source = r#"
        const report = {};
        try {
            await tools.call("Echo", { text: "x".repeat(100) });
            report.args = "unexpected";
        } catch (error) {
            report.args = error.code;
        }
        try {
            await tools.call("Echo", { ok: true });
            report.result = "unexpected";
        } catch (error) {
            report.result = error.code;
        }
        try {
            log("x".repeat(100));
            report.log = "unexpected";
        } catch (error) {
            report.log = error.code;
        }
        log("ok");
        return report;
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("the script catches each limit");

    assert_eq!(
        outcome.value,
        json!({"args": "args_too_large", "result": "result_too_large", "log": "log_limit"})
    );
    assert_eq!(outcome.logs, "ok");
}

#[test]
fn script_result_limit_is_enforced() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let mut limits = default_limits();
    limits.max_script_result_bytes = 16;
    let (engine, _handler) = build_engine(handler, limits);
    let source = r#"return "x".repeat(100);"#;

    let error = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect_err("oversized result must be rejected");

    match error {
        ToolScriptError::ResultTooLarge { limit, actual } => {
            assert_eq!(limit, 16);
            assert!(actual > 16, "actual size was {actual}");
        }
        other => panic!("expected a result-limit error, got {other:?}"),
    }
}

#[test]
fn pending_and_total_call_limits_bound_unawaited_work() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo).with_delay(Duration::from_millis(20)));
    let mut limits = default_limits();
    limits.max_concurrent_calls = 2;
    limits.max_pending_calls = 3;
    limits.max_tool_calls = 5;
    let (engine, _handler) = build_engine(handler, limits);
    let source = r#"
        const promises = [];
        for (let i = 0; i < 10; i++) {
            promises.push(
                tools.call("Echo", { i })
                    .then((value) => ({ ok: true, value }))
                    .catch((error) => ({ ok: false, code: error.code }))
            );
        }
        return await Promise.all(promises);
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("admission rejections are catchable");

    let results = outcome.value.as_array().unwrap();
    let succeeded = results.iter().filter(|item| item["ok"] == true).count();
    let pending_rejected = results
        .iter()
        .filter(|item| item["code"] == "too_many_pending_calls")
        .count();
    let total_rejected = results
        .iter()
        .filter(|item| item["code"] == "too_many_calls")
        .count();
    assert_eq!(succeeded, 3);
    assert_eq!(pending_rejected, 2);
    assert_eq!(total_rejected, 5);
    assert_eq!(outcome.calls_started, 3);
    assert!(outcome.peak_concurrency <= 2);
}

#[test]
fn logs_are_collected_in_order() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, _handler) = build_engine(handler, default_limits());
    let source = r#"
        log("first");
        log("第二");
        console.error("third");
        return null;
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("logging script should succeed");

    assert_eq!(outcome.logs, "first\n第二\nthird");
    assert_eq!(outcome.value, json!(null));
}

#[test]
fn private_prelude_bindings_are_not_visible_to_user_code() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, _handler) = build_engine(handler, default_limits());
    // Real QuickJS regression for the parent's lexical-scope probe: the user
    // body is a separate script, so the prelude closure's private bindings and
    // the deleted host globals must all be unreachable.
    let source = r#"
        return {
            rawCall: typeof rawCall,
            rawLog: typeof rawLog,
            config: typeof config,
            parse: typeof parse,
            ToolScriptError: typeof ToolScriptError,
            normalizeResult: typeof normalizeResult,
            checkBoundary: typeof checkBoundary,
            hostGlobal: typeof globalThis.__toolscript_host_call,
            logGlobal: typeof globalThis.__toolscript_log,
            configGlobal: typeof globalThis.__toolscript_config,
            argsGlobal: typeof globalThis.__toolscript_args,
            tools: typeof tools,
            toolCall: typeof tools.call,
            args: typeof args,
        };
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("inspection script should succeed");

    for name in [
        "rawCall",
        "rawLog",
        "config",
        "parse",
        "ToolScriptError",
        "normalizeResult",
        "checkBoundary",
        "hostGlobal",
        "logGlobal",
        "configGlobal",
        "argsGlobal",
    ] {
        assert_eq!(
            outcome.value[name],
            json!("undefined"),
            "{name} must not be visible to user code"
        );
    }
    assert_eq!(outcome.value["tools"], json!("object"));
    assert_eq!(outcome.value["toolCall"], json!("function"));
    assert_eq!(outcome.value["args"], json!("object"));
}

#[test]
fn monkeypatched_intrinsics_cannot_change_arguments_or_bypass_validation() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, handler) = build_engine(handler, default_limits());
    let source = r#"
        JSON.stringify = () => '{"hacked":true}';
        JSON.parse = () => ({ ok: true, value: { hacked: true } });
        Object.keys = () => [];
        Object.getPrototypeOf = () => ({ fake: true });
        Number.isFinite = () => true;
        Array.isArray = () => false;

        const reply = await tools.call("Echo", { value: 42, nested: { ok: true } });

        const cyclic = {};
        cyclic.self = cyclic;
        let cyclicCode;
        try {
            await tools.call("Echo", cyclic);
            cyclicCode = "accepted";
        } catch (error) {
            cyclicCode = error.code;
        }
        return { reply, cyclicCode };
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("the script should complete");

    assert_eq!(
        outcome.value,
        json!({
            "reply": {"value": 42, "nested": {"ok": true}},
            "cyclicCode": "cyclic_value",
        }),
        "the host reply must come from the captured parser and Rust validation"
    );
    let records = handler.records();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].args,
        json!({"value": 42, "nested": {"ok": true}})
    );
}

#[test]
fn function_constructor_and_dynamic_import_expose_no_host_or_io() {
    let handler = Arc::new(RecordingHandler::new(Mode::Echo));
    let (engine, _handler) = build_engine(handler, default_limits());
    let source = r#"
        const report = {};
        report.functionHostCall = Function("return typeof __toolscript_host_call")();
        report.functionRawCall = Function("return typeof rawCall")();
        report.functionRequire = Function("return typeof require")();
        report.functionProcess = Function("return typeof process")();
        let importState;
        try {
            const candidate = Function("return import('node:fs')")();
            if (candidate && typeof candidate.then === 'function') {
                try {
                    const module = await candidate;
                    importState = typeof module.readFile;
                } catch (_) {
                    importState = 'blocked';
                }
            } else {
                importState = 'blocked';
            }
        } catch (_) {
            importState = 'blocked';
        }
        report.importReadFile = importState;
        return report;
    "#;

    let outcome = block_on(engine.run(source, &json!(null), &Cancellation::new()))
        .expect("the inspection script should complete");

    for name in [
        "functionHostCall",
        "functionRawCall",
        "functionRequire",
        "functionProcess",
    ] {
        assert_eq!(
            outcome.value[name],
            json!("undefined"),
            "{name} must be absent"
        );
    }
    let import_state = outcome.value["importReadFile"].as_str().unwrap();
    assert!(
        import_state == "blocked" || import_state == "undefined",
        "dynamic import exposed something: {import_state}"
    );
}
