//! Real ToolScript → `ToolLoopExecutor` bridge.
//!
//! A script's `tools.call(name, args)` is dispatched through the executor's
//! normal preparation path (`prepare_tool_call_with`) and completion path
//! (`complete_tool_call_with`): loading policy, live permission mode, plan
//! reset, Pre/PostToolUse hooks, real permission prompts, path scope, file
//! state, MCP output and sandbox handling all apply exactly as for a
//! model-initiated call.
//!
//! Thread ownership: the accepted QuickJS runner is `!Send`, while the engine
//! spawns the executor future, so the VM runs on one dedicated thread with its
//! own current-thread runtime. Only `serde_json`/id/outcome data crosses the
//! bounded channels; no QuickJS handle leaves its thread and there is no
//! `unsafe Send/Sync`.

use super::*;
use crate::toolscript::{
    Cancellation, ToolCallError, ToolCallHandler, ToolCallRequest, ToolScriptEngine,
    ToolScriptError, ToolScriptLimits, ToolScriptOutcome,
};
use anyhow::{anyhow, Context, Result};
use futures::stream::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Stack for the VM thread: QuickJS stack limit plus Rust/engine overhead.
const VM_STACK_BYTES: usize = 16 * 1024 * 1024;

/// One `tools.call` handed from the VM thread to the executor.
struct NestedRequest {
    name: String,
    args: Value,
    reply: oneshot::Sender<std::result::Result<Value, ToolCallError>>,
    /// Dropped together with the handler future. The executor selects on this
    /// to cancel an in-flight tool body when JS abandons or times out the call.
    alive: oneshot::Receiver<()>,
}

struct BridgeHandler {
    tx: mpsc::Sender<NestedRequest>,
}

/// Dropping this guard (the handler future ending or being dropped) wakes the
/// executor's per-call abandonment watcher.
struct AliveGuard(#[allow(dead_code)] Option<oneshot::Sender<()>>);

#[async_trait::async_trait]
impl ToolCallHandler for BridgeHandler {
    async fn call(
        &self,
        request: ToolCallRequest,
        _cancellation: Cancellation,
    ) -> std::result::Result<Value, ToolCallError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let (alive_tx, alive_rx) = oneshot::channel();
        let _alive = AliveGuard(Some(alive_tx));
        let sent = self
            .tx
            .send(NestedRequest {
                name: request.name,
                args: request.args,
                reply: reply_tx,
                alive: alive_rx,
            })
            .await;
        if sent.is_err() {
            return Err(ToolCallError::new(
                "bridge_closed",
                "the tool executor stopped before this call could run",
            ));
        }
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(ToolCallError::new(
                "bridge_closed",
                "the tool executor dropped this call before it finished",
            )),
        }
    }
}

fn bridge_limits() -> ToolScriptLimits {
    let concurrency = max_tool_concurrency().clamp(1, 16);
    ToolScriptLimits {
        max_source_bytes: 64 * 1024,
        max_script_args_bytes: 256 * 1024,
        max_args_bytes: 32 * 1024,
        max_call_result_bytes: 2 * 1024 * 1024,
        max_script_result_bytes: 2 * 1024 * 1024,
        max_log_bytes: 64 * 1024,
        max_concurrent_calls: concurrency,
        max_pending_calls: 32,
        max_tool_calls: 256,
        max_call_name_bytes: 128,
        max_boundary_depth: 64,
        max_diagnostic_bytes: 4096,
        execution_timeout: Duration::from_secs(600),
        call_timeout: Duration::from_secs(600),
        memory_limit_bytes: 64 * 1024 * 1024,
        max_stack_size_bytes: 1024 * 1024,
    }
}

/// Lifecycle tools this bounded bridge cannot own. Rejected before prepare so a
/// tool `run_builtin` happens to know can never be admitted by accident.
const UNSUPPORTED_LIFECYCLE: &[&str] = &[
    // A script must not recursively start another script.
    "ToolScript",
    "Workflow",
    "Agent",
    "Skill",
    "PluginRun",
    "EnterPlanMode",
    "ExitPlanMode",
    "CronCreate",
    "CronDelete",
    "CronList",
    "ScheduleWakeup",
    "TeamCreate",
    "TeamDelete",
    "TeamSpawn",
    "TaskInput",
    "EnterWorktree",
    "ExitWorktree",
];

fn unsupported_nested_tool(name: &str, input: &Value) -> Option<String> {
    if name.starts_with("mcp__") {
        return Some(
            "MCP tools are not supported inside a ToolScript job: the durable owner has no MCP registry, OAuth callback route or elicitation host, so the call was refused before any side effect."
                .to_owned(),
        );
    }
    if name == "Bash" && input["run_in_background"] == true {
        return Some(
            "Bash run_in_background is not supported inside a ToolScript; the managed shell would outlive the script."
                .to_owned(),
        );
    }
    let delegation = super::delegation::definitions()
        .iter()
        .any(|tool| tool.name == name);
    if delegation || UNSUPPORTED_LIFECYCLE.contains(&name) {
        return Some(format!(
            "{name} is not supported inside a ToolScript in this increment (durable task ownership is required)."
        ));
    }
    None
}

/// A call in admission order.
struct Slot {
    occurrence: String,
    name: String,
    /// Post-hook effective input, filled by prepare.
    effective: Value,
    approved: bool,
    pre_contexts: Option<Vec<String>>,
    reply: oneshot::Sender<std::result::Result<Value, ToolCallError>>,
    alive: Option<oneshot::Receiver<()>>,
    concurrent: bool,
    step: Option<CallStep>,
    /// The attempted call was refused after its effective (post-hook) input was
    /// known, e.g. a PreToolUse rewrite that turns Bash into a managed
    /// background task.
    rejected: Option<String>,
    /// The JS promise disappeared (settled script, per-call timeout or
    /// cancellation) before this slot reached its body.
    abandoned: bool,
}

enum BodyResult {
    Finished(Value, String, bool),
    Ran(Result<Value>),
    Abandoned,
}

impl std::fmt::Debug for BodyResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyResult::Finished(_, output, failed) => write!(f, "Finished({failed}: {output})"),
            BodyResult::Ran(Ok(value)) => write!(f, "Ran(ok: {value})"),
            BodyResult::Ran(Err(error)) => write!(f, "Ran(err: {error})"),
            BodyResult::Abandoned => write!(f, "Abandoned"),
        }
    }
}

struct VmThread {
    handle: Option<std::thread::JoinHandle<()>>,
    cancellation: Cancellation,
}

impl VmThread {
    fn new(handle: std::thread::JoinHandle<()>, cancellation: Cancellation) -> Self {
        Self {
            handle: Some(handle),
            cancellation,
        }
    }

    async fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }
    }

    async fn shutdown_and_join(&mut self) {
        self.cancellation.cancel();
        self.join().await;
    }
}

impl Drop for VmThread {
    fn drop(&mut self) {
        // The bridge future itself was dropped (turn cancellation). Stop the
        // script; the VM thread exits through the interrupt hook and is
        // detached. No completion is reported on this path.
        self.cancellation.cancel();
    }
}

fn spawn_vm(
    engine: Arc<ToolScriptEngine>,
    source: String,
    args: Value,
    cancellation: Cancellation,
    done: oneshot::Sender<std::result::Result<ToolScriptOutcome, ToolScriptError>>,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("toolscript-vm".into())
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(engine.run(&source, &args, &cancellation)),
                Err(error) => Err(ToolScriptError::Engine {
                    message: format!("cannot start the ToolScript runtime: {error}"),
                }),
            };
            let _ = done.send(result);
        })
        .context("cannot start the ToolScript VM thread")
}

fn script_outcome(
    result: std::result::Result<
        std::result::Result<ToolScriptOutcome, ToolScriptError>,
        oneshot::error::RecvError,
    >,
) -> Result<ToolScriptOutcome> {
    match result {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(error)) => Err(anyhow!("{error}")),
        Err(_) => Err(anyhow!(
            "the ToolScript VM stopped without reporting a result"
        )),
    }
}

fn failure_code(output: &str) -> &'static str {
    let lower = output.to_ascii_lowercase();
    if lower.contains("permission denied") {
        "permission_denied"
    } else if lower.contains("not allowed for this agent")
        || lower.contains("not loaded")
        || lower.contains("not part of this response")
        || lower.contains("turned off")
    {
        "tool_unavailable"
    } else if lower.contains("not allowed") || lower.contains("plan mode") {
        // Plan-mode refusals are permission denials even though the executor's
        // message does not use the word "permission".
        "permission_denied"
    } else {
        "tool_failed"
    }
}

impl ToolLoopExecutor {
    /// Runs one script through the real executor. Internal entry point for the
    /// engine integration tests and the future model-visible tool; it is not
    /// registered as a tool by itself.
    pub(in crate::native_engine) async fn run_toolscript(
        &mut self,
        source: &str,
        args: &Value,
        parent_tool_use_id: &str,
        cancellation: &Cancellation,
    ) -> Result<ToolScriptOutcome> {
        let limits = bridge_limits();
        limits
            .validate()
            .map_err(|error| anyhow!("invalid ToolScript bridge limits: {error}"))?;
        let (tx, mut rx) = mpsc::channel::<NestedRequest>(limits.max_pending_calls);
        let (done_tx, mut done_rx) = oneshot::channel();
        let handler: Arc<dyn ToolCallHandler> = Arc::new(BridgeHandler { tx });
        let engine = Arc::new(
            ToolScriptEngine::with_limits(handler, limits)
                .map_err(|error| anyhow!("cannot create the ToolScript engine: {error}"))?,
        );
        let handle = spawn_vm(
            engine,
            source.to_owned(),
            args.clone(),
            cancellation.clone(),
            done_tx,
        )?;
        let mut vm = VmThread::new(handle, cancellation.clone());
        let mut sequence = 0u64;

        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    // Queued requests are dropped unexecuted; the VM stops.
                    drop(rx);
                    vm.shutdown_and_join().await;
                    return Err(anyhow!("ToolScript execution was cancelled"));
                }
                result = &mut done_rx => {
                    vm.join().await;
                    return script_outcome(result);
                }
                maybe = rx.recv() => {
                    match maybe {
                        // All senders are gone: the VM has finished or is
                        // finishing. Its result is the authoritative outcome.
                        None => {
                            let result = (&mut done_rx).await;
                            vm.join().await;
                            return script_outcome(result);
                        }
                        Some(first) => {
                            let mut group = vec![first];
                            while group.len() < max_tool_concurrency() {
                                match rx.try_recv() {
                                    Ok(next) => group.push(next),
                                    Err(_) => {
                                        // Simultaneously issued calls
                                        // (Promise.all, or several unawaited
                                        // calls) can arrive a few microseconds
                                        // apart. A 1 ms gather window lets them
                                        // share one admission group, so later
                                        // calls get a start and a terminal
                                        // record even if the first blocks.
                                        match tokio::time::timeout(
                                            Duration::from_millis(1),
                                            rx.recv(),
                                        )
                                        .await
                                        {
                                            Ok(Some(next)) => group.push(next),
                                            _ => break,
                                        }
                                    }
                                }
                            }
                            let finished =
                                self.run_nested_group(group, parent_tool_use_id, &mut sequence, cancellation).await?;
                            if !finished {
                                drop(rx);
                                vm.shutdown_and_join().await;
                                return Err(anyhow!("ToolScript execution was cancelled"));
                            }
                        }
                    }
                }
            }
        }
    }

    async fn emit_start(&self, occurrence: &str, name: &str, input: &Value) {
        let _ = self
            .event_tx
            .send(EngineEvent::ToolUseStart {
                tool_use_id: occurrence.to_owned(),
                tool_name: name.to_owned(),
                tool_input: input.clone(),
                text_before: String::new(),
                display_title: None,
            })
            .await;
    }

    async fn emit_origin(&self, occurrence: &str, parent: &str, sequence: u64) {
        let _ = self
            .event_tx
            .send(EngineEvent::NestedToolCall {
                tool_use_id: occurrence.to_owned(),
                parent_tool_use_id: parent.to_owned(),
                sequence,
            })
            .await;
    }

    async fn emit_done(
        &self,
        occurrence: &str,
        name: &str,
        input: &Value,
        output: &str,
        failed: bool,
    ) {
        let _ = self
            .event_tx
            .send(EngineEvent::ToolUseDone {
                tool_use_id: occurrence.to_owned(),
                tool_name: name.to_owned(),
                tool_input: input.clone(),
                output: output.to_owned(),
                is_error: failed,
            })
            .await;
    }

    /// Admits one drained group. Returns `false` when cancellation interrupted
    /// an owned batch and the caller must stop the run.
    async fn run_nested_group(
        &mut self,
        requests: Vec<NestedRequest>,
        parent: &str,
        sequence: &mut u64,
        cancellation: &Cancellation,
    ) -> Result<bool> {
        let mut slots: Vec<Slot> = Vec::new();
        for request in requests {
            // The JS promise was already abandoned, so this call must never
            // reach prepare or a tool body.
            if request.reply.is_closed() {
                continue;
            }
            let occurrence = self.next_invocation_id();
            *sequence += 1;
            let seq = *sequence;
            // The nested identity is durable before any row can be created, so
            // a crash can never leave a row that reopen would treat as a
            // provider-issued tool call.
            self.emit_origin(&occurrence, parent, seq).await;
            self.emit_start(&occurrence, &request.name, &request.args)
                .await;

            if let Some(reason) = unsupported_nested_tool(&request.name, &request.args) {
                // Independent failure record: the attempt happened, it was
                // refused before any side effect.
                self.emit_done(&occurrence, &request.name, &request.args, &reason, true)
                    .await;
                let _ = request
                    .reply
                    .send(Err(ToolCallError::new("unsupported_tool", reason)));
                continue;
            }
            let concurrent = self.is_concurrency_safe(&request.name, &request.args);
            slots.push(Slot {
                occurrence,
                name: request.name,
                effective: request.args,
                approved: false,
                pre_contexts: None,
                reply: request.reply,
                alive: Some(request.alive),
                concurrent,
                step: None,
                rejected: None,
                abandoned: false,
            });
        }
        if slots.is_empty() {
            return Ok(true);
        }

        // Consecutive concurrency-safe calls may run together; a mutating call
        // always stands alone in admission order. This is the pre-prepare
        // grouping; each batch re-checks its effective inputs after hooks.
        let mut batches: Vec<Vec<Slot>> = Vec::new();
        for slot in slots {
            match batches.last_mut() {
                Some(batch)
                    if batch.last().is_some_and(|last| last.concurrent) && slot.concurrent =>
                {
                    batch.push(slot)
                }
                _ => batches.push(vec![slot]),
            }
        }
        let mut cancelled = false;
        for batch in batches {
            if cancelled {
                // Every later admitted call already has a start; drain it to a
                // truthful terminal record instead of dropping it silently.
                let _ = self.finish_nested_batch(batch, Vec::new(), true).await?;
                continue;
            }
            if !self.run_nested_batch(batch, cancellation).await? {
                cancelled = true;
            }
        }
        Ok(!cancelled)
    }

    /// Prepares one pre-grouped batch, re-classifies it by its post-hook
    /// effective inputs, then runs each resulting run. Mutating calls are always
    /// alone, so a hook rewrite cannot make a mutation overlap a neighbour.
    async fn run_nested_batch(
        &mut self,
        batch: Vec<Slot>,
        cancellation: &Cancellation,
    ) -> Result<bool> {
        let mut slots = batch;
        let cancelled = self.prepare_nested_slots(&mut slots, cancellation).await;
        if cancelled {
            return self.finish_nested_batch(slots, Vec::new(), true).await;
        }
        for slot in &mut slots {
            slot.concurrent = matches!(slot.step, Some(CallStep::Builtin(..)))
                && slot.rejected.is_none()
                && !slot.abandoned
                && self.is_concurrency_safe(&slot.name, &slot.effective);
        }
        let mut runs: Vec<Vec<Slot>> = Vec::new();
        for slot in slots {
            match runs.last_mut() {
                Some(run) if run.last().is_some_and(|last| last.concurrent) && slot.concurrent => {
                    run.push(slot)
                }
                _ => runs.push(vec![slot]),
            }
        }
        let mut interrupted = false;
        for run in runs {
            if interrupted {
                let _ = self.finish_nested_batch(run, Vec::new(), true).await?;
                continue;
            }
            let mut run_slots = run;
            let (bodies, stop) = self.run_bodies(&mut run_slots, cancellation).await;
            if !self.finish_nested_batch(run_slots, bodies, stop).await? {
                interrupted = true;
            }
        }
        Ok(!interrupted)
    }

    /// Runs the prepare step for every slot in admission order. Returns whether
    /// whole-script cancellation interrupted it.
    async fn prepare_nested_slots(
        &mut self,
        slots: &mut [Slot],
        cancellation: &Cancellation,
    ) -> bool {
        // Prepare serially: permission prompts, hooks and policy stay ordered.
        // `ToolUseStart` was already emitted, so a prompt appears against a call
        // that is visibly in progress.
        let mut cancelled = false;
        for slot in slots.iter_mut() {
            if cancelled {
                slot.abandoned = true;
                continue;
            }
            // Clone the call identity before borrowing the liveness receiver,
            // so prepare can borrow both `slot` disjointly.
            let name = slot.name.clone();
            let effective = slot.effective.clone();
            let occurrence = slot.occurrence.clone();
            enum Prepared {
                Done(Box<PreparedCall>),
                Cancelled,
                Abandoned,
            }
            let outcome = {
                let alive = slot.alive.as_mut();
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Prepared::Cancelled,
                    // A script that settles while this call waits for a
                    // permission prompt or hook must not leave the executor
                    // blocked: the dropped host future is the signal.
                    _ = async {
                        match alive {
                            Some(receiver) => { let _ = receiver.await; }
                            None => futures::future::pending::<()>().await,
                        }
                    } => Prepared::Abandoned,
                    prepared = Box::pin(self.prepare_tool_call_with(
                        &name,
                        &effective,
                        &occurrence,
                        &occurrence,
                    )) => Prepared::Done(Box::new(prepared)),
                }
            };
            match outcome {
                Prepared::Cancelled => {
                    slot.abandoned = true;
                    cancelled = true;
                }
                Prepared::Abandoned => {
                    slot.abandoned = true;
                }
                Prepared::Done(prepared) => {
                    match &prepared.step {
                        CallStep::Finished(result) => {
                            slot.effective = result.0.clone();
                        }
                        CallStep::Builtin(input, approved) => {
                            slot.effective = input.clone();
                            slot.approved = *approved;
                        }
                    }
                    slot.pre_contexts = prepared.pre_contexts;
                    slot.step = Some(prepared.step);
                    // A PreToolUse hook may rewrite the input after the initial
                    // name/argument check. Revalidate the effective arguments
                    // before any body can run, so a rewritten managed-background
                    // Bash cannot escape the bounded bridge.
                    if matches!(slot.step, Some(CallStep::Builtin(..))) {
                        if let Some(reason) = unsupported_nested_tool(&slot.name, &slot.effective) {
                            slot.rejected = Some(reason);
                        }
                    }
                }
            }
        }
        cancelled
    }

    /// Runs the builtin bodies of one prepared batch with bounded concurrency.
    ///
    /// The `&self` borrow is confined here so the caller can complete and
    /// record results with `&mut self` afterwards.
    async fn run_bodies(
        &self,
        slots: &mut [Slot],
        cancellation: &Cancellation,
    ) -> (Vec<Option<BodyResult>>, bool) {
        let mut bodies: Vec<Option<BodyResult>> = (0..slots.len()).map(|_| None).collect();
        let cwd = self.get_workspace_cwd().to_owned();
        let mut queue: Vec<(usize, futures::future::BoxFuture<'_, BodyResult>)> = Vec::new();
        for (index, slot) in slots.iter_mut().enumerate() {
            if slot.abandoned
                || slot.rejected.is_some()
                || !matches!(slot.step, Some(CallStep::Builtin(..)))
            {
                continue;
            }
            let Some(mut alive) = slot.alive.take() else {
                continue;
            };
            // Re-check abandonment before constructing the body: a synchronous
            // Edit/Write must not run after the JS caller already went away.
            if matches!(alive.try_recv(), Err(oneshot::error::TryRecvError::Closed)) {
                bodies[index] = Some(BodyResult::Abandoned);
                continue;
            }
            let name = slot.name.clone();
            let input = slot.effective.clone();
            let approved = slot.approved;
            let cwd = cwd.clone();
            queue.push((
                index,
                Box::pin(async move {
                    let run = self.run_builtin(&name, &input, &cwd, approved);
                    tokio::select! {
                        // Abandonment is polled first: a body that would finish
                        // synchronously must observe a gone caller before it
                        // produces any effect.
                        biased;
                        _ = &mut alive => BodyResult::Abandoned,
                        result = run => BodyResult::Ran(result),
                    }
                }),
            ));
        }
        if queue.is_empty() {
            return (bodies, false);
        }

        // Bounded concurrency, but results are retained per completion: a
        // whole-batch cancellation drops only the bodies still running, never
        // a call that already changed a file and succeeded.
        let mut queue = queue.into_iter();
        let mut in_flight: futures::stream::FuturesUnordered<
            futures::future::BoxFuture<'_, (usize, BodyResult)>,
        > = futures::stream::FuturesUnordered::new();
        let mut exhausted = false;
        let limit = max_tool_concurrency().max(1);
        loop {
            while !exhausted && in_flight.len() < limit {
                match queue.next() {
                    Some((index, future)) => {
                        in_flight.push(Box::pin(async move { (index, future.await) }));
                    }
                    None => exhausted = true,
                }
            }
            if in_flight.is_empty() {
                break;
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return (bodies, true),
                next = in_flight.next() => match next {
                    Some((index, result)) => bodies[index] = Some(result),
                    None => break,
                },
            }
        }
        (bodies, false)
    }

    async fn finish_nested_batch(
        &mut self,
        slots: Vec<Slot>,
        mut bodies: Vec<Option<BodyResult>>,
        cancelled: bool,
    ) -> Result<bool> {
        if bodies.is_empty() {
            bodies = (0..slots.len()).map(|_| None).collect();
        }
        // Complete serially and emit exactly one terminal event per attempt.
        for (index, slot) in slots.into_iter().enumerate() {
            let mut slot = slot;
            if let Some(reason) = slot.rejected.take() {
                self.emit_done(&slot.occurrence, &slot.name, &slot.effective, &reason, true)
                    .await;
                let _ = slot
                    .reply
                    .send(Err(ToolCallError::new("unsupported_tool", reason)));
                continue;
            }
            let step = slot.step.take();
            let ran = match step {
                Some(CallStep::Finished(result)) => {
                    BodyResult::Finished(result.0, result.1, result.2)
                }
                Some(CallStep::Builtin(..)) => match bodies[index].take() {
                    Some(body @ BodyResult::Ran(_)) => body,
                    _ => BodyResult::Abandoned,
                },
                None => BodyResult::Abandoned,
            };
            match ran {
                BodyResult::Abandoned => {
                    let message = "Tool call was cancelled or abandoned before it finished.";
                    self.emit_done(&slot.occurrence, &slot.name, &slot.effective, message, true)
                        .await;
                    let _ = slot
                        .reply
                        .send(Err(ToolCallError::new("cancelled", message)));
                }
                BodyResult::Finished(actual, output, failed) => {
                    let contexts = slot.pre_contexts.take();
                    let (actual, output, failed) = self
                        .complete_tool_call_with(
                            &slot.name,
                            &slot.occurrence,
                            contexts,
                            (actual, output, failed),
                        )
                        .await;
                    self.reply_after_completion(slot, actual, output, failed)
                        .await;
                }
                BodyResult::Ran(result) => {
                    let (output, failed) =
                        self.finish_builtin(&slot.name, &slot.effective, &slot.occurrence, result);
                    let contexts = slot.pre_contexts.take();
                    let (actual, output, failed) = self
                        .complete_tool_call_with(
                            &slot.name,
                            &slot.occurrence,
                            contexts,
                            (slot.effective.clone(), output, failed),
                        )
                        .await;
                    self.reply_after_completion(slot, actual, output, failed)
                        .await;
                }
            }
        }
        Ok(!cancelled)
    }

    async fn reply_after_completion(
        &self,
        slot: Slot,
        actual: Value,
        output: String,
        failed: bool,
    ) {
        self.emit_done(&slot.occurrence, &slot.name, &actual, &output, failed)
            .await;
        if failed {
            let _ = slot
                .reply
                .send(Err(ToolCallError::new(failure_code(&output), output)));
        } else {
            let _ = slot.reply.send(Ok(Value::String(output)));
        }
    }
}

#[cfg(test)]
mod abandoned_body_tests {
    use super::*;
    use crate::native_engine::engine_core::ReasoningMode;
    use crate::native_engine::provider_manager::ResolvedProvider;
    use serde_json::json;

    fn provider() -> ResolvedProvider {
        ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        }
    }

    fn executor(root: &std::path::Path, tx: mpsc::Sender<EngineEvent>) -> ToolLoopExecutor {
        let manager = crate::runtime::default_permissions();
        manager.set_mode(crate::permissions::PermissionMode::BypassPermissions);
        ToolLoopExecutor::new(
            provider(),
            vec![],
            None,
            64,
            tx,
            root.to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        )
        .with_conv_id("alive-fixture".into())
        .with_permission_manager(manager)
    }

    #[tokio::test]
    async fn a_prepared_mutation_with_a_gone_caller_never_runs() {
        let temp = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let executor = executor(temp.path(), tx);
        let input = json!({"file_path":"nope.txt","content":"must not exist"});
        let (reply, _reply_rx) = oneshot::channel();
        // The JS caller already went away before the body could start.
        let (alive_tx, alive_rx) = oneshot::channel();
        drop(alive_tx);
        let mut slots = vec![Slot {
            occurrence: "occ-abandoned".into(),
            name: "Write".into(),
            effective: input.clone(),
            approved: true,
            pre_contexts: None,
            reply,
            alive: Some(alive_rx),
            concurrent: false,
            step: Some(CallStep::Builtin(input, true)),
            rejected: None,
            abandoned: false,
        }];
        let (bodies, interrupted) = executor.run_bodies(&mut slots, &Cancellation::new()).await;
        assert!(!interrupted);
        assert!(
            matches!(bodies[0], Some(BodyResult::Abandoned)),
            "{bodies:?}"
        );
        assert!(
            !temp.path().join("nope.txt").exists(),
            "an abandoned Write reached the filesystem"
        );
    }

    /// Private-seam test for the per-call alive receiver, distinct from the
    /// global `Cancellation`: it constructs one real Bash slot, waits for the
    /// fixture's own start barrier (`root.pid`), then drops the alive sender
    /// exactly as JS abandonment does. A real body is started, then dropped.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_the_alive_receiver_stops_a_started_bash_body() {
        use std::time::{Duration, Instant};

        fn read_pid(path: &std::path::Path) -> Option<u32> {
            std::fs::read_to_string(path).ok()?.trim().parse().ok()
        }
        fn proc_start(pid: u32) -> Option<u64> {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
            fields.get(19)?.parse().ok()
        }
        async fn wait_gone(pid: u32, start: u64, limit: Duration) -> bool {
            let deadline = Instant::now() + limit;
            loop {
                if proc_start(pid) != Some(start) {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        /// Test-only cleanup for a failed assertion: signals only the exact PID
        /// and start time the fixture published, never a pattern or group name.
        struct ExactCleanup(Vec<(u32, u64)>);
        impl Drop for ExactCleanup {
            fn drop(&mut self) {
                for (pid, start) in &self.0 {
                    if proc_start(*pid) == Some(*start) {
                        let _ = std::process::Command::new("kill")
                            .args(["-9", "--", &pid.to_string()])
                            .status();
                    }
                }
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (tx, mut rx) = mpsc::channel(8);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let executor = executor(root, tx);
        let input = json!({"command":"echo $$ > root.pid; sleep 2; touch late.txt"});
        let (reply, _reply_rx) = oneshot::channel();
        let (alive_tx, alive_rx) = oneshot::channel();
        let mut slots = vec![Slot {
            occurrence: "occ-started".into(),
            name: "Bash".into(),
            effective: input.clone(),
            approved: true,
            pre_contexts: None,
            reply,
            alive: Some(alive_rx),
            concurrent: false,
            step: Some(CallStep::Builtin(input, true)),
            rejected: None,
            abandoned: false,
        }];
        let barrier_root = root.to_path_buf();
        let barrier = async move {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(pid) = read_pid(&barrier_root.join("root.pid")) {
                    let start = proc_start(pid).expect("the Bash body must be alive");
                    // No window between observing the identity and owning
                    // cleanup: if `run_bodies` panics, this future (and the
                    // guard it is about to return) is dropped, so the exact
                    // process is signalled even though no assertion ran.
                    let cleanup = ExactCleanup(vec![(pid, start)]);
                    drop(alive_tx);
                    return (cleanup, pid, start);
                }
                assert!(Instant::now() < deadline, "the Bash body never started");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let cancellation = Cancellation::new();
        let ((bodies, interrupted), (cleanup, pid, start)) =
            tokio::join!(executor.run_bodies(&mut slots, &cancellation), barrier);
        // `tokio::join!` keeps the completed branch's output even if the other
        // branch unwinds, so this guard owns cleanup until the last assertion.
        let _cleanup = cleanup;
        assert!(!interrupted);
        assert!(
            matches!(bodies[0], Some(BodyResult::Abandoned)),
            "{bodies:?}"
        );
        // Drop signals the group; the test waits for the OS afterwards.
        assert!(
            wait_gone(pid, start, Duration::from_secs(5)).await,
            "the started Bash outlived per-call abandonment"
        );
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !root.join("late.txt").exists(),
            "a dropped body produced a late effect"
        );
        drop(drain);
    }
}
