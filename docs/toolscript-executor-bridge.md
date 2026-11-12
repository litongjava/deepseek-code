# ToolScript executor bridge (J4)

Status: internal engine entry point. There is **no model-visible ToolScript
tool** and no durable background task lifecycle yet. The next increment
registers the tool and adds TaskGet/TaskStop ownership.

## Entry point

```rust
impl ToolLoopExecutor {
    #[cfg(not(target_os = "android"))]
    pub(super) async fn run_toolscript(
        &mut self,
        source: &str,
        args: &Value,
        parent_tool_use_id: &str,
        cancellation: &crate::toolscript::Cancellation,
    ) -> anyhow::Result<crate::toolscript::ToolScriptOutcome>;
}
```

It reuses the accepted embedded QuickJS foundation
(`docs/toolscript-foundation.md`) and dispatches every `tools.call(name, args)`
through the executor's normal preparation and completion path
(`prepare_tool_call_with` / `complete_tool_call_with`). It never calls
`tools::execute` directly and never uses `execute_host_tool`, the host-picked
bundled-skill bypass.

## Thread ownership

The engine spawns `ToolLoopExecutor::execute()` with `tokio::spawn`, so that
future must be `Send`. The QuickJS runner is `!Send`. Each script run therefore:

- owns one dedicated OS thread (`toolscript-vm`, 16 MiB stack) with its own
  current-thread Tokio runtime that `block_on`s `ToolScriptEngine::run`;
- keeps every QuickJS handle on that thread; only tool names,
  `serde_json::Value`, ids and outcomes cross the bounded channels;
- uses a bounded `mpsc` request channel (capacity 32) and one `oneshot` reply
  per call;
- joins the VM thread with `spawn_blocking` before returning an outcome or a
  `Cancelled` error.

There is no `unsafe impl Send/Sync`, no transmute, and no VM handle shared
across threads.

## Dispatch guarantees

- **Same authority as a model call.** Loading policy, on-demand/deferred tool
  state, agent allowlists, live parent permission mode, plan reset,
  `PreToolUse`, real permission prompts, `PostToolUse`/failure hooks, path
  scope, file-state accounting, MCP output, sandbox and shell policy all apply.
- **Effective input is authoritative.** A batch is prepared serially; then each
  call is reclassified for concurrency from its **post-hook** input. A hook that
  rewrites a read-only Bash into a mutation runs that call alone; a rewrite that
  adds `run_in_background: true` is refused before the body. The original input
  is what `ToolUseStart` reports; `ToolUseDone` reports the effective input.
- **Bounded safe parallelism.** Only consecutive calls classified
  concurrency-safe run together, capped by `max_tool_concurrency()` and the
  foundation's semaphore. Preparation, permission prompts and completion are
  serial. `Promise.all` result order is the array order.
- **Independent occurrences.** Every nested call gets
  `next_invocation_id()` (`inv-{turn_scope}-{n}`), a per-script sequence and the
  caller's parent occurrence id. Raw/model/script ids are never reused as
  durable identity.
- **One terminal record per attempt.** `NestedToolCall` (origin) is emitted
  first, then `ToolUseStart`; exactly one `ToolUseDone` follows, including for
  denial, unsupported tools, cancellation and later admitted batches that were
  never run.
- **No provider protocol fabrication.** Nested calls are events only; the bridge
  never appends to `messages` or `conversation_messages`. Replay conversion
  (`SessionStore::model_history`) drops ids recorded in `tool_call_origins`, so
  OpenAI/Anthropic history keeps only the provider's own pairs.

## Persistence

Product code, not test-only SQL:

- `services::tool_history::apply_event` projects `ToolArgDelta`/`ToolUseStart`/
  `ToolUseDone`/`PipelineToolResult`/`NestedToolCall` into rows and origins with
  the existing late-event rules (an independent accumulator keeps a late delta
  or late start from rewriting a completed call).
- `services::tool_history::save` writes the assistant message and each call; a
  nested call's row and origin are written in **one transaction**
  (`save_stream_nested_tool_result`). An origin whose row is not yet present is
  deferred to the next flush, so the pair is never separated by a crash.
- `tool_call_origins(tool_call_id PRIMARY KEY REFERENCES tool_calls ON DELETE
  CASCADE, parent_tool_call_id, sequence, origin)` is created by
  `SCHEMA_SQL` (`IF NOT EXISTS`), so existing databases gain it on open.
- `save_tool_origin` is first-wins; a conflicting replay never reparents.
- `branch_conversation` remaps and copies origins; message deletion cascades
  through `tool_calls`.
- The CLI `BackgroundTranscript` uses the same projector and writer.

## Cancellation

`Cancellation` is checked in the request loop, during prepare (permission/hook
waits), before each body and inside each body. The per-call "alive" `oneshot`
fires when the JS promise is dropped (script settlement, per-call timeout or
cancellation); the executor then drops that specific tool future. Completed
siblings keep their real results.

The two signals are distinct and tested separately. The explicit-cancellation
tests use the global `Cancellation`.
`dropping_the_alive_receiver_stops_a_started_bash_body` drives the private
per-call alive seam instead: it starts a real Bash body, waits for the
fixture's own PID barrier, then drops the alive sender exactly as JS
abandonment does. Its exact PID+start-time cleanup guard is constructed inside
the barrier the moment the identity is observed and returned through
`tokio::join!`, so it owns cleanup even if the other branch unwinds; it is held
until the final assertion.

On Unix, dropping a Bash body sends `SIGKILL` to the owned process group
(`ProcessTree::drop`) and tokio kills the root; **the drop signals but does not
await the kernel**. The bridge returns after joining the VM thread, not after
OS reaping. The cancellation test captures the exact shell and child PIDs,
cancels only after they exist, and waits for both PID+start-time identities to
disappear plus a horizon longer than the command's effect. Awaited process
cleanup would be a separate change to `run_logged`/`ProcessTree`.

Script settlement with an unawaited call: the request is either never dispatched
(dropped before admission; `calls_started == 0`) or already in flight, in which
case the same alive/drop path applies. Because completion is serialized after a
run's bodies, a script cannot resume past an awaited sibling while another body
in that run is still running.

## Unsupported tools (refused before side effects)

`Workflow`, `Agent` and delegation definitions, `Skill`, `PluginRun`,
`EnterPlanMode`/`ExitPlanMode`, `CronCreate`/`CronDelete`/`CronList`/
`ScheduleWakeup`, `TeamCreate`/`TeamDelete`/`TeamSpawn`, `TaskInput`,
`EnterWorktree`/`ExitWorktree`, and Bash with `run_in_background: true`.
The check runs on both the original and the post-hook effective input.

Bash foreground-to-background promotion is a host/user action
(`request_background`, Ctrl+B), not automatic; direct bridge runs do not
establish the session shell context. Nested-promotion semantics are a
follow-up for the durable-task increment.

## Limits

Bridge `ToolScriptLimits`: 64 KiB source, 256 KiB script args, 32 KiB per-call
args, 2 MiB per-call result, 2 MiB script result, 64 KiB log output,
`max_tool_concurrency()` concurrent calls (1..=16), 32 pending, 256 total calls,
128-byte names, depth 64, 4096-byte diagnostics, 600 s execution and call
timeouts, 64 MiB heap, 1 MiB QuickJS stack.

## Evidence

Verified on the merged tree (`655c34b` plus this worktree's bridge): the
`toolscript_bridge` suite is **17 tests** — 15 integration tests in
`toolscript_bridge_tests.rs` plus 2 unit tests in `toolscript_bridge.rs`
(`a_prepared_mutation_with_a_gone_caller_never_runs` and
`dropping_the_alive_receiver_stops_a_started_bash_body`). It passes serially
(17 passed) and with 4 threads (17 passed); the retained foundation suite is
33, `services::tool_history` 6, `db::conversation_repo` 7 and the CLI
`background_transcript` 4. Fresh logs and real exit codes are in the round-06
record.

Focused commands (see `docs/cli-regression-tests.md` and the round record):

```bash
cargo test -p deepseek-code-core --lib toolscript_bridge
cargo test -p deepseek-code-core --lib "toolscript::"
cargo test -p deepseek-code-core --lib "services::tool_history"
cargo test -p deepseek-code-core --lib "db::conversation_repo"
cargo test -p deepseek-code-cli --bins background_transcript
cargo check -p deepseek-code-core --lib
cargo check -p deepseek-code-cli
cargo check -p deepseek-code-desktop --all-targets
```

## Not implemented

- Model-visible ToolScript registration or provider schema.
- Durable task IDs, `TaskGet`/`TaskStop`, background query timeouts, owner-loss
  recovery.
- Desktop/`src-tauri` origin persistence: the new event is forwarded, but the
  Desktop host does not yet write `tool_call_origins`; the tool is not
  registered there, so it cannot produce nested calls in production.
- Awaited OS process reaping on cancellation.
