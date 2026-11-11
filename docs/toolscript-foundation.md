# ToolScript embedded JavaScript foundation (J1–J3)

Status: engine-only foundation on branch `feat/toolscript-runtime`. This is not a
model-visible tool and it is not wired to the tool executor. It executes a real,
in-process QuickJS runtime (`rquickjs 0.14.0`) and exposes one typed asynchronous
host callback.

## Scope

Implemented:

- `crates/core/src/toolscript/`: `ToolScriptEngine`, `ToolCallHandler`,
  `ToolCallRequest`, `ToolCallError`, `Cancellation`, `ToolScriptLimits`,
  `ToolScriptOutcome`, `ToolScriptError`.
- Real QuickJS execution with sequential calls, `Promise.all`, conditions, and
  ordinary JavaScript transformations.
- Rust-side strict JSON boundary validation, depth and cycle detection, Unicode
  preservation, and bounded diagnostics.
- Declared hard limits for source, script arguments, per-call arguments, per-call
  results, script results, log output, concurrency, pending calls, total calls,
  depth, CPU/execution time, stack, and memory.
- Cancellation and drop semantics, including an external cancellation flag
  readable by the QuickJS interrupt hook.

Not implemented (separate increments):

- `ToolScript` model-visible registration.
- `ToolLoopExecutor` dispatch, NORMAL permissions, current policy, hooks, events,
  transcript, or persistence.
- Independent call IDs / parent relationships for nested executor calls (this
  foundation assigns a monotonic internal `call_id`, but no executor occurrence
  ID yet).
- Durable task IDs, `TaskGet`, `TaskStop`, background query timeouts, owner-loss
  recovery, or persistence.
- Any provider/model integration.

## API

```rust
let engine = ToolScriptEngine::with_limits(handler, limits)?;
let outcome = engine
    .run(source, &serde_json::json!({ "topic": "example" }), &cancellation)
    .await?;
```

`ToolCallHandler::call(request, cancellation)` returns `Result<serde_json::Value,
ToolCallError>`. `ToolCallRequest` contains:

- `call_id: u64` — independent monotonic call number for this run (1-based).
- `name: String` — the script-visible tool name.
- `args: serde_json::Value` — already validated at the Rust boundary.

`ToolCallError { code, message, details }` reaches the script as a catchable
`ToolScriptError` with `code`, `tool`, `callId`, and optional `details`. Uncaught
tool failures remain structured in `ToolScriptError::Script`.

`Cancellation` is `Clone + Send + Sync` and is cancelled from any thread. The
runner passes a clone to the handler and also checks it in the QuickJS interrupt
hook.

`ToolScriptEngine::run` is intentionally **not `Send`**. Like every `rquickjs`
future it owns the VM on the calling thread. Do not move it onto another executor
thread. A future integration that needs a different thread must own the VM on a
dedicated current-thread executor and use typed channels; this round adds no
`unsafe impl Send/Sync` and no transmute.

## Script capabilities

The user body is evaluated as a **separate global script** from the trusted
prelude. It can access exactly these globals:

- `tools.call(name, args)` returns a Promise.
- `log(...parts)` and `console.log/info/warn/error`.
- `args` — the external script arguments parsed from JSON.

The prelude's raw host bridge, configuration, validator helpers, and error class
live in a private closure. They are deleted from `globalThis` and are not in the
user body's lexical scope. `typeof rawCall`, `typeof config`, and their global
forms are `undefined`; `Function("return typeof __toolscript_host_call")()` is
also `undefined` in the real QuickJS regression.

No filesystem, network, process, module loader, or timer globals are registered.
QuickJS provides none by default. Dynamic `import()` has no loader and must not
expose host helpers; `require`, `process`, `fetch`, `setTimeout`, `XMLHttpRequest`,
`WebSocket`, `Deno`, and `Bun` are absent. These are covered by tests.

`JSON.stringify`, `Object.keys`, `Object.getPrototypeOf`, `Number.isFinite`, and
`Array.isArray` are **not** used for boundary validation. Arguments are passed as
live JS values into Rust, which walks them directly; the host reply is parsed by a
captured `JSON.parse`. Monkeypatching those intrinsics therefore cannot change
what the host receives or bypass a limit (regression:
`monkeypatched_intrinsics_cannot_change_arguments_or_bypass_validation`).

## Limits and defaults

All byte limits are UTF-8 bytes. `ToolScriptLimits::validate` rejects unusable
configurations; `with_limits` calls it.

| Field | Default | Validation |
|---|---:|---|
| `max_source_bytes` | 256 KiB | > 0 |
| `max_script_args_bytes` | 1 MiB | > 0 |
| `max_args_bytes` | 64 KiB | > 0 |
| `max_call_result_bytes` | 1 MiB | > 0 |
| `max_script_result_bytes` | 4 MiB | > 0 |
| `max_log_bytes` | 64 KiB | > 0 |
| `max_concurrent_calls` | 8 | 1..=64 |
| `max_pending_calls` | 64 | >= concurrency, <= 1024 |
| `max_tool_calls` | 512 | > 0 |
| `max_call_name_bytes` | 128 | > 0 |
| `max_boundary_depth` | 64 | 1..=128 |
| `max_diagnostic_bytes` | 4096 | 64..=65536 |
| `execution_timeout` | 300 s | 1 ns..=3600 s |
| `call_timeout` | 120 s | > 0 and <= `execution_timeout` |
| `memory_limit_bytes` | 64 MiB | 1 MiB..=1 GiB |
| `max_stack_size_bytes` | 1 MiB | 128 KiB..=8 MiB |

Notes:

- `max_concurrent_calls` is enforced with a Tokio semaphore around the handler.
  `Promise.all` preserves input order; peak concurrency is reported in
  `ToolScriptOutcome::peak_concurrency`.
- `max_pending_calls` bounds admitted-but-unsettled calls, including queued and
  unawaited promises. `max_tool_calls` bounds total admitted attempts.
- `execution_timeout` is a wall-clock budget for the whole script **including time
  spent awaiting host calls**. It is enforced by the real QuickJS interrupt hook;
  a Tokio timeout is only a transport backstop and cannot preempt a synchronous
  loop.
- `call_timeout` starts after the call acquires a concurrency permit.
- `max_stack_size_bytes` is passed to QuickJS. The caller's thread needs room for
  this plus Rust/QuickJS overhead; the CLI and test harness use 16 MiB stacks.
- `max_diagnostic_bytes` bounds message and stack fields surfaced in errors.
  JavaScript-side truncation is approximate; Rust truncates on a UTF-8 boundary.

## Boundary semantics

Arguments and script results are converted from live JS values to strict JSON in
Rust:

- `null`, booleans, finite numbers, strings, arrays, and plain objects are
  accepted.
- Numbers preserve their numeric value. An integral double is stored as an
  integer only when it lies exactly in `[-2^63, 2^63)`; `2^63`, larger values,
  `1e20`, and fractions stay `f64`. There is no saturating cast, so
  `9223372036854775808` does not become `9223372036854775807`. Very large
  integral doubles may render in exponent notation, but the value is preserved.
- `undefined` is rejected inside values. A top-level `undefined` script result
  becomes `null`; `tools.call(name, undefined)` is rejected.
- Functions, symbols, bigints, promises, modules, proxies, errors, `NaN`,
  `Infinity`, sparse arrays, and nested `undefined` are rejected. Proxies are
  rejected before array/object enumeration by class id, so no trap runs.
- Arrays are accepted only when every index `0..length-1` is an own property.
  Holes are rejected even when `Array.prototype[index]` holds a value. The
  boundary scans own keys first and then re-checks each index's ownership
  natively immediately before reading it, so a getter that deletes a later
  element cannot cause an inherited value to cross. Own accessor elements may
  run their getters, but they cannot substitute a prototype value for a missing
  own index. Own properties that are not canonical array indices
  (`0..=2^32-2`), such as `"4294967295"`, `"01"`, or extra `"foo"` keys, are
  ignored for arrays, matching `JSON.stringify`.
- Objects must have the realm's intrinsic `Object.prototype` (captured from a
  fresh native `Object::new` at call time) or a null prototype. `Date`, `Map`,
  class instances, and similar objects are rejected. The check does not trust the
  mutable global `Object` constructor.
- Cycles are detected by JS object identity, not by depth exhaustion.
- Depth is checked against `max_boundary_depth`.
- Serialized bytes are counted while walking; the exact serialized length is
  rechecked after conversion.
- Strings are converted through rquickjs and preserve valid Unicode, including
  Chinese, emoji, and combining characters. Lone-surrogate behavior is not
  guaranteed.

Host-call boundary failures use codes `non_json_value`, `invalid_number`,
`cyclic_value`, `value_too_deep`, `args_too_large`, or `unreadable_value`. Script
result failures use the same codes except that oversize results produce
`ToolScriptError::ResultTooLarge { limit, actual }`.

## Errors

`ToolScriptError` variants:

- `InvalidLimits`, `UnsupportedPlatform`, `SourceTooLarge`,
  `ScriptArgsTooLarge`, `ResultTooLarge`.
- `Compile { message, name }` for syntax errors in the user body.
- `Script { message, name, stack, code, tool, call_id }` for uncaught JS
  exceptions and boundary failures.
- `Timeout`, `Cancelled`.
- `MemoryLimit { limit, message }`.
- `Engine { message }` for engine setup/installation failures.

Per-tool failures are never flattened into a generic engine error. A caught
`ToolScriptError` keeps `code`, `tool`, and `callId`; an uncaught one keeps them
in `ToolScriptError::Script`.

## Cancellation, drop, and shutdown semantics

- The runner never `tokio::spawn`s host work. A host call is a future owned by the
  JS runtime's scheduler.
- Cancellation, a per-call timeout, or the top-level script settling without
  awaiting a call drops the host future. **Drop is the cancellation contract.**
  The callback also receives the `Cancellation` token so it can stop its own
  subordinate work deliberately, but the runner does not await a handler-specific
  shutdown protocol.
- A CPU-bound JavaScript loop is stopped by the QuickJS interrupt hook, which
  observes an absolute deadline and an external atomic flag. A cancellation
  token cancelled from another thread stops the loop even though the Tokio timer
  cannot be polled.
- When the top-level script settles, pending host calls are counted in
  `ToolScriptOutcome::abandoned_calls` **before** shutdown. The JS context and
  runtime are then dropped before `run` returns, which drops those futures.
- Tests assert the actual host `Drop` guard and the absence of a late effect. They
  do not rely only on counters.
- No OS-level side-effect cancellation is claimed. If a host handler has already
  performed a side effect before its future is dropped, dropping the future
  cannot undo it. Handlers must structure irreversible work to happen only after
  they would accept cancellation.
- A registered Rust callback must not capture a persistent `Ctx`/`JsValue`; that
  can retain the JS context past runtime shutdown and trip QuickJS's
  `JS_FreeRuntime` assertion. This implementation derives the trusted prototype
  inside each call from a fresh `Object::new` and captures only Rust data.

## Testing

Commands used for this increment (see the support log for exact output):

```bash
cargo test --locked -p deepseek-code-core --lib toolscript::
cargo check --locked -p deepseek-code-core --lib
cargo check --locked -p deepseek-code-cli
cargo check --locked -p deepseek-code-desktop --all-targets
/home/dev/.local/rustfmt/bin/rustfmt --edition 2021 --check crates/core/src/toolscript/*.rs
```

The 33 focused tests use real QuickJS and fake host callbacks. They prove the
engine, boundary validation, limits, interruption, cancellation, drop behavior,
and the absence of ambient I/O. Round 02 adds numeric exactness at `±2^53`, the
signed `2^63` boundary, `2^64`, `1e20`, and fractions; inherited-hole rejection;
getter-mutation races for both arguments and script results; proxy trap
suppression; and the `2^32-1` extra-property rule. They do **not** prove
permissions, hooks, history, persistence, executor dispatch, model-visible
registration, or platform behavior outside the tested Linux host.

## Known unverified boundaries

- Android: `rquickjs` stays out of the Android dependency graph and
  `ToolScriptEngine::run` returns `UnsupportedPlatform`. Android linkage/build of
  the foundation was not exercised.
- Windows and macOS: not run in this round.
- A blocking (non-async) host handler cannot be preempted by this runner.
- A handler that spawns detached work or ignores its future's drop can still have
  side effects; the runner's contract is limited to dropping the returned future.
- User scripts can sabotage their own run (for example by replacing
  `Promise.prototype.then`), but cannot reach the prelude's private bindings or
  bypass Rust boundary validation and limits. Such sabotage is bounded by the
  execution timeout/interrupt hook, not prevented.
- `ToolScriptEngine::run` is `!Send`; integration must own the VM on a dedicated
  current-thread executor if it needs to run away from the current task.
