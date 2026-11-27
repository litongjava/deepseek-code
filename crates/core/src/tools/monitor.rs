//! Monitors stream a command's output as durable, session-local events.
//!
//! Every stdout line (grouped into short batches) becomes one row of the
//! monitor's `events.jsonl`. `collect_events` claims those rows exactly once,
//! so the CLI can turn them into task notifications without polling the
//! process itself.
use super::background_shell::NOTIFICATION_START;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::mpsc,
};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60 * 60);
pub(super) const MAX_RUNTIME: Duration = Duration::from_secs(24 * 60 * 60);
/// Short watch intervals can renew silently between notices.
const RENEWAL_NOTICE_INTERVAL: Duration = Duration::from_secs(60);
/// Lines arriving within this window form one event.
const BATCH: Duration = Duration::from_millis(200);
/// More events than this within one minute stops the monitor.
const MAX_EVENTS_PER_MINUTE: usize = 30;
/// A single stdout line is kept at this many characters.
const MAX_LINE_CHARS: usize = 4096;
const MAX_EVENT_LINES: usize = 50;
const MAX_EVENT_BYTES: usize = 16 * 1024;

#[cfg(all(test, unix))]
#[path = "../../tests/support/manual_clock.rs"]
mod manual_clock;

#[cfg(all(test, unix))]
#[derive(Debug, PartialEq, Eq)]
enum TestObservation {
    BatchQueued(tokio::time::Instant),
    BatchFlushed,
    Renewed(tokio::time::Instant),
    Ending,
    Finished,
}

#[cfg(all(test, unix))]
tokio::task_local! {
    static TEST_OBSERVER: mpsc::UnboundedSender<TestObservation>;
}

#[cfg(all(test, unix))]
fn observe(observer: &Option<mpsc::UnboundedSender<TestObservation>>, event: TestObservation) {
    if let Some(observer) = observer {
        let _ = observer.send(event);
    }
}

/// Starts a single-interval monitor for the owning session.
pub fn start(
    command: Command,
    description: &str,
    command_text: &str,
    timeout: Duration,
    guard: Option<crate::services::wsl::Guard>,
) -> Result<Value> {
    start_with_runtime_limit(command, description, command_text, timeout, None, guard)
}

pub(super) fn start_with_runtime_limit(
    mut command: Command,
    description: &str,
    command_text: &str,
    timeout: Duration,
    max_runtime: Option<Duration>,
    guard: Option<crate::services::wsl::Guard>,
) -> Result<Value> {
    anyhow::ensure!(
        !timeout.is_zero() && timeout <= MAX_TIMEOUT,
        "Monitor timeout must be between 1 second and 60 minutes"
    );
    anyhow::ensure!(
        max_runtime.is_none_or(|limit| limit > timeout && limit <= MAX_RUNTIME),
        "Monitor max_runtime_ms must exceed timeout_ms and be at most 86400000"
    );
    let (root, session) = super::background_shell::CONTEXT
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .context("Monitor requires an owning engine session")?;
    let session = uuid::Uuid::parse_str(&session)
        .context("Invalid task session ID")?
        .to_string();
    let started_ms = chrono::Utc::now().timestamp_millis();
    let mut metadata = json!({"monitor":true,"command":command_text,"started_ms":started_ms,"timeout_ms":timeout.as_millis() as u64});
    if let Some(limit) = max_runtime {
        metadata["max_runtime_ms"] = json!(limit.as_millis() as u64);
    }
    let task = super::tasks::execute_for_shell_owner(
        "TaskCreate",
        // `monitor` marks the record for event collection; without
        // `managed_shell` the shell completion notice stays silent.
        json!({"subject":description,"metadata":metadata}),
        &root,
        &session,
    )?;
    let id = task["id"].as_str().context("Task ID missing")?.to_string();
    let directory = root.join("tasks/shell").join(&session).join(&id);
    let mut completion = Completion {
        root,
        session,
        id: id.clone(),
        phase: MonitorPhase::Starting,
    };
    std::fs::create_dir_all(&directory)?;
    let stdout_log = directory.join("stdout.log");
    let stderr_log = directory.join("stderr.log");
    std::fs::write(&stdout_log, b"")?;
    std::fs::write(&stderr_log, b"")?;
    let events_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("events.jsonl"))?;
    std::fs::write(directory.join("heartbeat"), b"running")?;

    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    // A blocked SQLite commit must never consume a launched child's cap.
    super::tasks::execute_for_shell_owner(
        "TaskUpdate",
        json!({"id":id,"status":"running"}),
        &completion.root,
        &completion.session,
    )?;
    let cap_started = max_runtime.map(|_| tokio::time::Instant::now());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            completion.finish(
                "failed",
                json!({"is_error":true,"error":format!("{error:#}")}),
            );
            return Err(error).context("Starting monitor command");
        }
    };
    let tree = crate::services::tool_hooks::ProcessTree::new(
        child.id().context("Monitor process ID missing")?,
    )?;
    let stdout = child.stdout.take().context("Monitor stdout missing")?;
    let stderr = child.stderr.take().context("Monitor stderr missing")?;
    completion.phase = MonitorPhase::Running;
    let (lines, mut output) = mpsc::unbounded_channel();
    let mut out_reader = tokio::spawn(read_lines(stdout, stdout_log, Some(lines)));
    let mut err_reader = tokio::spawn(read_lines(stderr, stderr_log, None));

    let runtime = match max_runtime {
        Some(limit) => format!(
            "renews every {} ms while the script runs, up to {} ms total",
            timeout.as_millis(),
            limit.as_millis()
        ),
        None => format!("stops after {} min", timeout.as_secs() / 60),
    };
    let message = format!(
        "Monitor \"{description}\" started (task {id}, {runtime}). Each output line arrives as a notification; TaskStop stops it."
    );
    #[cfg(all(test, unix))]
    let observer = TEST_OBSERVER.try_with(Clone::clone).ok();
    tokio::spawn(async move {
        // The WSL lease must outlive the command; dropping it cancels it.
        let _guard = guard;
        let started = cap_started.unwrap_or_else(tokio::time::Instant::now);
        let overall_deadline = started + max_runtime.unwrap_or(timeout);
        let mut deadline = started + timeout;
        let mut next_renewal_notice = started;
        let cancel = directory.join("cancel");
        let mut emitter = Emitter {
            file: events_file,
            seq: 0,
            output_events: 0,
            output_times: VecDeque::new(),
        };
        let mut pending: Vec<String> = Vec::new();
        let mut batch_start: Option<tokio::time::Instant> = None;
        let mut stdout_open = true;
        let mut ticker = tokio::time::interval(Duration::from_millis(50));
        let mut heartbeat = tokio::time::Instant::now();
        let mut tree = Some(tree);
        let mut end = loop {
            let batch_deadline = batch_start.map(|start| start + BATCH);
            tokio::select! {
                line = output.recv(), if stdout_open => match line {
                    Some(line) => {
                        if pending.is_empty() {
                            let now = tokio::time::Instant::now();
                            batch_start = Some(now);
                            #[cfg(all(test, unix))]
                            observe(&observer, TestObservation::BatchQueued(now));
                        }
                        pending.push(line);
                    }
                    None => stdout_open = false,
                },
                status = child.wait() => {
                    break End::Exited(status.ok().and_then(|status| status.code()));
                }
                _ = tokio::time::sleep_until(deadline) => {
                    // Without a cap this branch always expires when the timer wins.
                    if max_runtime.is_none() {
                        break End::Expired;
                    }
                    if cancel.exists() {
                        break End::Stopped;
                    }
                    match child.try_wait() {
                        Ok(Some(status)) => break End::Exited(status.code()),
                        Err(error) => {
                            tracing::warn!(%error, "Monitor process check failed");
                            break End::ProcessCheckFailed;
                        }
                        Ok(None) => {}
                    }
                    let now = tokio::time::Instant::now();
                    if now >= overall_deadline {
                        break End::Expired;
                    }
                    // Start a fresh interval without replaying missed deadlines.
                    deadline = (now + timeout).min(overall_deadline);
                    if now >= next_renewal_notice {
                        next_renewal_notice = now + RENEWAL_NOTICE_INTERVAL;
                        let remaining_ms = (deadline - now).as_millis();
                        let text = format!("Monitor renewed for up to {remaining_ms} ms. The script is still running.");
                        if let Err(error) = emitter.event("renewed", &text) {
                            tracing::warn!(%error, "Monitor renewal event write failed");
                        }
                    }
                    #[cfg(all(test, unix))]
                    observe(&observer, TestObservation::Renewed(deadline));
                }
                _ = ticker.tick() => {
                    if cancel.exists() {
                        break End::Stopped;
                    }
                    if heartbeat.elapsed() >= Duration::from_secs(1) {
                        let _ = std::fs::write(directory.join("heartbeat"), b"running");
                        heartbeat = tokio::time::Instant::now();
                    }
                }
                _ = async {
                    match batch_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    if !pending.is_empty() {
                        let text = batch_text(&pending);
                        pending.clear();
                        batch_start = None;
                        if let Err(error) = emitter.output(&text, tokio::time::Instant::now()) {
                            tracing::warn!(%error, "Monitor event write failed");
                        }
                        #[cfg(all(test, unix))]
                        observe(&observer, TestObservation::BatchFlushed);
                        if emitter.flooded() {
                            break End::Flooded;
                        }
                    }
                }
            }
        };
        #[cfg(all(test, unix))]
        observe(&observer, TestObservation::Ending);
        // Release the process group so descendants cannot keep the pipes open,
        // then collect whatever the readers saw before EOF.
        if !matches!(end, End::Exited(_)) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        drop(tree.take());
        let drained = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = (&mut out_reader).await;
            let _ = (&mut err_reader).await;
        })
        .await;
        if drained.is_err() {
            out_reader.abort();
            err_reader.abort();
        }
        if !matches!(end, End::Flooded) {
            while let Ok(line) = output.try_recv() {
                pending.push(line);
            }
            if !pending.is_empty() {
                let text = batch_text(&pending);
                if let Err(error) = emitter.output(&text, tokio::time::Instant::now()) {
                    tracing::warn!(%error, "Monitor event write failed");
                }
            }
            if emitter.flooded() && !matches!(end, End::Exited(_)) {
                end = End::Flooded;
            }
        }
        let elapsed = started.elapsed();
        let minutes = elapsed.as_secs() / 60;
        let seconds = elapsed.as_secs() % 60;
        let events = emitter.output_events;
        let (kind, text, status) = match end {
            End::Exited(code) => {
                let code = code.unwrap_or(-1);
                (
                    "exited",
                    format!("Monitor exited with code {code} after {minutes}m {seconds}s ({events} events)"),
                    if code == 0 { "completed" } else { "failed" },
                )
            }
            End::Expired => {
                let text = match max_runtime {
                    Some(limit) => format!(
                        "Monitor expired after reaching its {} ms runtime cap ({events} output events).",
                        limit.as_millis()
                    ),
                    None => format!("Monitor expired after {minutes}m with {events} events delivered. Start it again if you still need the watch."),
                };
                ("expired", text, "completed")
            }
            End::Stopped => (
                "stopped",
                format!("Monitor stopped after {minutes}m ({events} events)"),
                "cancelled",
            ),
            End::Flooded => (
                "flooded",
                "Monitor stopped: more than 30 events in a minute. Use a narrower filter (for example grep --line-buffered) and start it again.".to_string(),
                "failed",
            ),
            End::ProcessCheckFailed => (
                "failed",
                "Monitor stopped after it could not check whether the script was still running.".to_owned(),
                "failed",
            ),
        };
        if let Err(error) = emitter.event(kind, &text) {
            tracing::warn!(%error, "Monitor final event write failed");
        }
        completion.finish(status, json!({"monitor":true,"kind":kind,"events":events}));
        #[cfg(all(test, unix))]
        observe(&observer, TestObservation::Finished);
    });
    let mut result = json!({
        "task_id": id,
        "description": description,
        "status": "running",
        "timeout_ms": timeout.as_millis() as u64,
        "message": message,
    });
    if let Some(limit) = max_runtime {
        result["max_runtime_ms"] = json!(limit.as_millis() as u64);
    }
    Ok(result)
}

pub(super) fn is_active_status(status: &Value) -> bool {
    matches!(status.as_str(), Some("pending" | "running" | "in_progress"))
}

/// Why a monitor's process stopped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Exited(Option<i32>),
    Expired,
    Stopped,
    Flooded,
    ProcessCheckFailed,
}

enum MonitorPhase {
    Starting,
    Running,
    Finished,
}

/// Owns the task record from creation through startup and the spawned future.
struct Completion {
    root: PathBuf,
    session: String,
    id: String,
    phase: MonitorPhase,
}

impl Completion {
    fn finish(&mut self, status: &'static str, output: Value) {
        // Once selected, this outcome also owns any retry. Drop must never
        // substitute owner cancellation for a database error.
        self.phase = MonitorPhase::Finished;
        let db = self
            .root
            .join("data/session-tasks")
            .join(format!("{}.sqlite3", self.session));
        super::monitor_completion::record(db, &self.id, status, output);
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        let (status, message) = match self.phase {
            MonitorPhase::Starting => (
                "failed",
                "Monitor startup failed before the runner was ready",
            ),
            MonitorPhase::Running => (
                "cancelled",
                "Monitor owner exited or the monitor was cancelled",
            ),
            MonitorPhase::Finished => return,
        };
        self.finish(status, json!({"is_error":true,"error":message}));
    }
}

/// Appends events to `events.jsonl` and tracks the flood window.
struct Emitter {
    file: std::fs::File,
    seq: u64,
    output_events: usize,
    output_times: VecDeque<tokio::time::Instant>,
}

impl Emitter {
    fn event(&mut self, kind: &str, text: &str) -> std::io::Result<()> {
        self.seq += 1;
        let record = json!({
            "seq": self.seq,
            "at_ms": chrono::Utc::now().timestamp_millis(),
            "kind": kind,
            "text": text,
        });
        writeln!(self.file, "{record}")?;
        self.file.flush()
    }

    fn output(&mut self, text: &str, now: tokio::time::Instant) -> std::io::Result<()> {
        self.event("output", text)?;
        self.output_events += 1;
        self.output_times.push_back(now);
        while self
            .output_times
            .front()
            .is_some_and(|first| now.duration_since(*first) > Duration::from_secs(60))
        {
            self.output_times.pop_front();
        }
        Ok(())
    }

    fn flooded(&self) -> bool {
        self.output_times.len() > MAX_EVENTS_PER_MINUTE
    }
}

/// Joins a batch, bounding it to 50 lines and 16 KiB.
fn batch_text(lines: &[String]) -> String {
    let mut text = String::new();
    let mut bytes = 0usize;
    let mut kept = 0usize;
    for line in lines {
        if kept == MAX_EVENT_LINES {
            break;
        }
        // The byte count includes the separator between kept lines.
        let addition = line.len() + usize::from(kept > 0);
        if bytes + addition > MAX_EVENT_BYTES {
            break;
        }
        if kept > 0 {
            text.push('\n');
        }
        text.push_str(line);
        bytes += addition;
        kept += 1;
    }
    if kept < lines.len() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("… {} more lines", lines.len() - kept));
    }
    text
}

/// Reads one pipe line by line into its log, forwarding stdout lines.
async fn read_lines<R: AsyncRead + Unpin>(
    reader: R,
    log: PathBuf,
    sink: Option<mpsc::UnboundedSender<String>>,
) {
    let mut reader = BufReader::new(reader);
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .ok();
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) => break,
            Ok(_) => {
                while buffer
                    .last()
                    .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
                {
                    buffer.pop();
                }
                let line: String = String::from_utf8_lossy(&buffer)
                    .chars()
                    .take(MAX_LINE_CHARS)
                    .collect();
                if let Some(log) = log.as_mut() {
                    let _ = writeln!(log, "{line}");
                }
                if let Some(sink) = &sink {
                    if sink.send(line).is_err() {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
}

/// A monitor event that has not been delivered to the model yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub task_id: String,
    pub description: String,
    /// output, renewed, exited, expired, stopped, flooded or failed.
    pub kind: String,
    pub text: String,
}

impl Event {
    /// What the model reads, in the reference CLI's notification format.
    pub fn notification(&self) -> String {
        format!(
            "{NOTIFICATION_START}\n<task-id>{}</task-id>\n<kind>monitor</kind>\n<status>{}</status>\n<summary>{}</summary>\n<event>{}</event>\n</task-notification>",
            self.task_id,
            self.kind,
            escape(&self.summary()),
            escape(&self.text)
        )
    }

    /// The terminal line for this event.
    pub fn summary(&self) -> String {
        let first: String = self
            .text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        format!("Monitor event: \"{}\": {first}", self.description)
    }
}

/// Notification bodies are XML-like; a raw angle bracket would break them.
fn escape(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '<' => '‹',
            '>' => '›',
            other => other,
        })
        .collect()
}

/// Claims every monitor event of the session that was not delivered yet.
/// A busy database is "nothing this tick"; the next poll delivers them.
pub fn collect_events(root: &Path, session: &str) -> Result<Vec<Event>> {
    let session = uuid::Uuid::parse_str(session)?.to_string();
    let path = root
        .join("data/session-tasks")
        .join(format!("{session}.sqlite3"));
    if !path.exists() {
        return Ok(vec![]);
    }
    let mut connection = rusqlite::Connection::open(path)?;
    // The caller polls about once a second; waiting longer would freeze its loop.
    connection.busy_timeout(Duration::from_millis(250))?;
    let tx = match connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        // Another writer holds the database; the next poll claims these events.
        Err(rusqlite::Error::SqliteFailure(error, _))
            if matches!(
                error.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            return Ok(vec![]);
        }
        Err(error) => return Err(error.into()),
    };
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS monitor_event_claims(task_id TEXT, seq INTEGER, PRIMARY KEY(task_id, seq))",
    )?;
    let tasks = {
        let mut statement = tx.prepare(
            "SELECT tasks.id, body -> '$.subject' FROM tasks WHERE json_extract(body,'$.metadata.monitor')=1 ORDER BY tasks.rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut events = Vec::new();
    for (id, subject) in tasks {
        let id = uuid::Uuid::parse_str(&id)?.to_string();
        let subject = subject.and_then(|json| serde_json::from_str::<String>(&json).ok());
        let description: String = subject
            .as_deref()
            .unwrap_or("Monitor")
            .chars()
            .filter(|ch| !ch.is_control() && !matches!(ch, '<' | '>'))
            .take(160)
            .collect();
        let file = root
            .join("tasks/shell")
            .join(&session)
            .join(&id)
            .join("events.jsonl");
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let mut records: Vec<(i64, String, String)> = Vec::new();
        for line in text.lines() {
            // A line still being appended is skipped and read next time.
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(seq) = value["seq"].as_i64() else {
                continue;
            };
            let kind = value["kind"].as_str().unwrap_or("output").to_owned();
            let text = value["text"].as_str().unwrap_or("").to_owned();
            records.push((seq, kind, text));
        }
        records.sort_by_key(|(seq, _, _)| *seq);
        for (seq, kind, text) in records {
            if tx.execute(
                "INSERT OR IGNORE INTO monitor_event_claims(task_id, seq) VALUES(?1, ?2)",
                rusqlite::params![&id, seq],
            )? == 0
            {
                continue;
            }
            events.push(Event {
                task_id: id.clone(),
                description: description.clone(),
                kind,
                text,
            });
        }
    }
    tx.commit()?;
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::background_shell::scope;

    #[test]
    fn renewal_cap_schema_is_optional_and_input_is_validated() {
        let definition = crate::tools::get_tool_definitions()
            .into_iter()
            .find(|tool| tool.name == "Monitor")
            .unwrap();
        let schema = definition.input_schema;
        assert_eq!(schema["required"], json!(["command", "description"]));
        assert_eq!(schema["properties"]["timeout_ms"]["default"], 1_800_000);
        assert_eq!(schema["properties"]["max_runtime_ms"]["type"], "integer");
        assert_eq!(
            schema["properties"]["max_runtime_ms"]["maximum"],
            86_400_000
        );
        let normalized = super::super::monitor_input(json!({})).unwrap();
        assert_eq!(normalized["__monitor_timeout_ms"], 1_800_000);
        assert!(normalized.get("max_runtime_ms").is_none());
        for limit in [
            json!("2000"),
            json!(2000.5),
            json!(999),
            json!(1000),
            json!(86_400_001),
        ] {
            let error =
                super::super::monitor_input(json!({"timeout_ms":1000,"max_runtime_ms":limit}))
                    .unwrap_err();
            assert!(error.to_string().contains("max_runtime_ms"), "{error}");
        }
        for limit in [1001, 86_400_000] {
            let input =
                super::super::monitor_input(json!({"timeout_ms":1000,"max_runtime_ms":limit}))
                    .unwrap();
            assert_eq!(input["max_runtime_ms"], limit);
        }
    }

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    /// Starts a monitor as its owning session and returns the task record.
    #[cfg(unix)]
    async fn start_monitor(root: &Path, session: &str, script: &str, timeout: Duration) -> Value {
        let command = shell(script);
        let (root, session) = (root.to_path_buf(), session.to_owned());
        scope(Some((root, session)), async {
            start(command, "fixture", script, timeout, None).unwrap()
        })
        .await
    }

    #[tokio::test]
    async fn failed_monitor_setup_and_spawn_leave_terminal_deletable_records() {
        use crate::tools::tasks::execute_for_session;
        for block_logs in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let session = uuid::Uuid::new_v4().to_string();
            let log_root = root.path().join("tasks/shell");
            if block_logs {
                std::fs::create_dir_all(root.path().join("tasks")).unwrap();
                std::fs::write(&log_root, b"directory creation must fail").unwrap();
            }
            let program = root.path().join("nonexistent-monitor-command");
            let error = scope(Some((root.path().to_owned(), session.clone())), async {
                start_with_runtime_limit(
                    Command::new(program),
                    "startup failure",
                    "fixture command",
                    Duration::from_secs(1),
                    Some(Duration::from_secs(5)),
                    None,
                )
                .unwrap_err()
            })
            .await;
            assert_eq!(log_root.is_file(), block_logs);
            assert_eq!(
                error.to_string().contains("Starting monitor command"),
                !block_logs,
                "{error:#}"
            );
            let call =
                |name, input| execute_for_session(name, input, root.path(), &session).unwrap();
            let listing = call("TaskList", json!({}));
            let tasks = listing["tasks"].as_array().unwrap();
            assert_eq!(tasks.len(), 1);
            let id = &tasks[0]["id"];
            let failed = call("TaskGet", json!({"id":id,"block":false}));
            assert_eq!(failed["status"], "failed", "{failed}");
            let output: Value = serde_json::from_str(failed["output"].as_str().unwrap()).unwrap();
            assert_eq!(output["is_error"], true);
            assert_eq!(
                call("TaskStop", json!({"id":id}))["cancellation_requested"],
                false
            );
            assert_eq!(
                call("TaskUpdate", json!({"id":id,"status":"deleted"}))["deleted"],
                true
            );
            assert!(call("TaskList", json!({}))["tasks"]
                .as_array()
                .unwrap()
                .is_empty());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn capped_script_cannot_launch_before_running_publication_commits() {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        crate::tools::tasks::execute_for_session(
            "TaskCreate",
            json!({"subject":"schema fixture"}),
            root.path(),
            &session,
        )
        .unwrap();
        let db = rusqlite::Connection::open(
            root.path()
                .join("data/session-tasks")
                .join(format!("{session}.sqlite3")),
        )
        .unwrap();
        db.execute_batch(
            "CREATE TRIGGER reject_publication BEFORE UPDATE ON tasks
            WHEN json_extract(NEW.body,'$.status')='running'
            BEGIN SELECT RAISE(ABORT,'running publication blocked'); END",
        )
        .unwrap();
        let launched = std::fs::File::create(root.path().join("launched")).unwrap();
        let fd = launched.as_raw_fd();
        let mut command = shell("exit 0");
        // pre_exec runs before spawn returns, even if the child is immediately killed.
        // SAFETY: write is async-signal-safe and launched outlives this spawn.
        unsafe {
            command.pre_exec(move || {
                if libc::write(fd, b"X".as_ptr().cast(), 1) == 1 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let error = scope(Some((root.path().to_owned(), session)), async {
            start_with_runtime_limit(
                command,
                "publication fixture",
                "exit 0",
                Duration::from_secs(1),
                Some(Duration::from_millis(1100)),
                None,
            )
            .unwrap_err()
        })
        .await;
        assert!(
            error.to_string().contains("running publication blocked"),
            "{error:#}"
        );
        assert_eq!(
            launched.metadata().unwrap().len(),
            0,
            "child spawned before SQLite commit"
        );
    }

    /// Return only after the child exited and the owner's first write met the reservation.
    #[cfg(unix)]
    async fn locked_clean_exit(root: &Path, session: &str) -> (String, rusqlite::Connection) {
        let mut command = shell("while [ ! -f release ]; do sleep 0.02; done; exit 0");
        command.current_dir(root);
        let (observer, mut observations) = mpsc::unbounded_channel();
        let task = TEST_OBSERVER
            .scope(
                observer,
                scope(
                    Some((root.to_owned(), session.to_ascii_uppercase())),
                    async {
                        start_with_runtime_limit(
                            command,
                            "locked completion",
                            "fixture",
                            Duration::from_secs(30),
                            Some(Duration::from_secs(60)),
                            None,
                        )
                        .unwrap()
                    },
                ),
            )
            .await;
        let db = rusqlite::Connection::open(
            root.join("data/session-tasks")
                .join(format!("{session}.sqlite3")),
        )
        .unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        std::fs::write(root.join("release"), b"").unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while observations.recv().await.unwrap() != TestObservation::Finished {}
        })
        .await
        .unwrap();
        let id = task["task_id"].as_str().unwrap().to_owned();
        assert_eq!(stored_task(&db, &id)["status"], "running");
        (id, db)
    }

    #[cfg(unix)]
    fn stored_task(db: &rusqlite::Connection, id: &str) -> Value {
        let body: String = db
            .query_row("SELECT body FROM tasks WHERE id=?1", [id], |row| row.get(0))
            .unwrap();
        serde_json::from_str(&body).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clean_exit_retries_its_original_outcome_after_sqlite_busy() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let (id, db) = locked_clean_exit(root.path(), &session).await;
        db.execute_batch("ROLLBACK").unwrap();
        // Raw SQLite proves the owner persisted the outcome without any reader.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let row = stored_task(&db, &id);
                if row["status"] != "running" {
                    assert_eq!(row["status"], "completed");
                    let output: Value =
                        serde_json::from_str(row["output"].as_str().unwrap()).unwrap();
                    assert_eq!(output, json!({"monitor":true,"kind":"exited","events":0}));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let events = collect_events(root.path(), &session).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "exited");
        assert!(events[0].text.contains("code 0"));
    }

    #[cfg(unix)]
    #[test]
    fn clean_exit_leaves_the_row_unchanged_when_the_busy_budget_expires() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        let (id, db) = runtime.block_on(locked_clean_exit(root.path(), &session));
        let original = stored_task(&db, &id);
        // Runtime shutdown joins the blocking retry while we still hold the lock.
        drop(runtime);
        assert!(started.elapsed() >= Duration::from_secs(10));
        db.execute_batch("ROLLBACK").unwrap();
        let events = collect_events(root.path(), &session).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "exited");
        assert!(events[0].text.contains("code 0"));
        assert_eq!(stored_task(&db, &id), original);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn task_get_bounds_stored_output_during_and_after_a_busy_monitor_exit() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let (id, db) = locked_clean_exit(root.path(), &session).await;
        let logs = root.path().join("tasks/shell").join(&session).join(&id);
        std::fs::rename(&logs, logs.with_extension("archived")).unwrap();
        let get = || {
            crate::tools::tasks::execute_for_session(
                "TaskGet",
                json!({"id":id,"block":false,"max_output_bytes":4}),
                root.path(),
                &session,
            )
            .unwrap()
        };
        let before = get();
        assert_eq!(before["status"], "running");
        assert!(before["output"].is_null());
        assert_eq!(before["has_more"], false);
        assert_eq!(before["output_truncated"], false);
        db.execute_batch("ROLLBACK").unwrap();
        wait_finished(root.path(), &session, &id).await;
        let after = get();
        assert_eq!(after["status"], "completed");
        assert_eq!(after["output"].as_str().unwrap().len(), 4);
        assert_eq!(after["has_more"], true);
        assert_eq!(after["output_truncated"], true);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn task_update_waits_for_a_busy_monitor_edit_and_preserves_its_description() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let (id, db) = locked_clean_exit(root.path(), &session).await;
        let (edit_root, edit_session, edit_id) =
            (root.path().to_owned(), session.clone(), id.clone());
        let (starting, started) = tokio::sync::oneshot::channel();
        let mut edit = tokio::task::spawn_blocking(move || {
            starting.send(()).unwrap();
            crate::tools::tasks::execute_for_session(
                "TaskUpdate",
                json!({"id":edit_id,"description":"annotation"}),
                &edit_root,
                &edit_session,
            )
        });
        started.await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(200), &mut edit)
            .await
            .is_err());
        db.execute_batch("ROLLBACK").unwrap();
        assert_eq!(edit.await.unwrap().unwrap()["description"], "annotation");
        wait_finished(root.path(), &session, &id).await;
        let row = stored_task(&db, &id);
        assert_eq!(row["status"], "completed");
        assert_eq!(row["description"], "annotation");
    }

    #[cfg(unix)]
    fn check_edit_before_completion_retry(annotation: Option<&str>, future_time: Option<&str>) {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            // Reserve the retry worker until TaskUpdate has committed and returned its receipt.
            let (resume, gate) = std::sync::mpsc::channel::<()>();
            let (entered, ready) = tokio::sync::oneshot::channel();
            tokio::task::spawn_blocking(move || {
                entered.send(()).unwrap();
                let _ = gate.recv();
            });
            ready.await.unwrap();
            let (id, db) = locked_clean_exit(root.path(), &session).await;
            db.execute_batch("ROLLBACK").unwrap();
            let tool = |name, args| {
                crate::tools::tasks::execute_for_session(name, args, root.path(), &session).unwrap()
            };
            let mut changes = json!({"id":id,"description":"annotation after script exit"});
            if let Some(annotation) = annotation {
                changes["output"] = json!(annotation);
            }
            let mut expected = tool("TaskUpdate", changes);
            assert_eq!(expected["status"], "running");
            if let Some(annotation) = annotation {
                assert_eq!(
                    expected["output"].as_str(),
                    Some(annotation),
                    "TaskUpdate receipt"
                );
            }
            assert_eq!(stored_task(&db, &id), expected);
            let edit_time =
                chrono::DateTime::parse_from_rfc3339(expected["updatedAt"].as_str().unwrap())
                    .unwrap();
            if let Some(timestamp) = future_time {
                // Model an edit committed before a backwards system clock adjustment.
                db.execute(
                    "UPDATE tasks SET body=json_set(body,'$.updatedAt',?2) WHERE id=?1",
                    rusqlite::params![id, timestamp],
                )
                .unwrap();
            }
            let released = chrono::Utc::now();
            resume.send(()).unwrap();
            let get = wait_finished(root.path(), &session, &id).await;
            let list = tool("TaskList", json!({}));
            let row = stored_task(&db, &id);
            assert_eq!(list["tasks"][0], row);
            let timestamp = row["updatedAt"].as_str().unwrap();
            let time = chrono::DateTime::parse_from_rfc3339(timestamp).unwrap();
            assert!(
                time >= released && time > edit_time,
                "retry {time}, release {released}, edit {edit_time}"
            );
            if let Some(timestamp) = future_time {
                assert_eq!(row["updatedAt"], timestamp);
            }
            let mut output = json!({"monitor":true,"kind":"exited","events":0});
            if let Some(annotation) = annotation {
                output["task_output"] = json!(annotation);
            }
            expected["output"] = json!(output.to_string());
            expected["status"] = json!("completed");
            expected["updatedAt"] = json!(timestamp);
            assert_eq!(row, expected);
            for (key, value) in expected.as_object().unwrap() {
                assert_eq!(&get[key], value, "TaskGet field {key}");
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn completion_retry_advances_updated_at_after_task_update_commits() {
        check_edit_before_completion_retry(None, None);
    }

    #[cfg(unix)]
    #[test]
    fn completion_retry_preserves_intervening_output_and_the_stored_timestamp_floor() {
        check_edit_before_completion_retry(Some(r#"{"kind":"user text","note":"✓"}"#), None);
        // This future instant sorts earlier as local text. Compare full RFC 3339 instants.
        let future = (chrono::Utc::now() + chrono::Duration::hours(1))
            .with_timezone(&chrono::FixedOffset::west_opt(12 * 3600).unwrap())
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, false);
        check_edit_before_completion_retry(Some(""), Some(&future));
    }

    #[cfg(unix)]
    #[test]
    fn completion_retry_keeps_every_byte_of_acknowledged_task_output() {
        let long = "x".repeat(1024 * 1024) + "\0tail😀";
        for annotation in [
            "prefix\0suffix\n{\"unicode\":\"✓😀\",\"quote\":\"\\\"\"}",
            "\0\0start\r\n\t\u{001f}end\0",
            r#"\uD800 \uDC00 \\ \u0000 "quoted""#,
            "e\u{0301} / é / 🧪",
            &long,
        ] {
            check_edit_before_completion_retry(Some(annotation), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn completion_retry_preserves_raw_surrogate_escapes_and_checks_full_state_fields() {
        use serde::Deserialize;
        use serde_json::value::RawValue;

        #[derive(Deserialize)]
        struct Summary<'a> {
            #[serde(borrow)]
            task_output: &'a RawValue,
        }

        // TaskUpdate accepts Unicode strings. A historical JSON row can additionally
        // contain unpaired surrogate escapes that cannot be represented by a Rust str.
        let token = r#""\uD800\u0000middle\uDC00\uD83D\uDE00\/\u0061""#;
        for (source_status, should_finish) in [
            (r#""r\u0075nning""#, true),
            (r#""running\u0000other""#, false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let session = uuid::Uuid::new_v4().to_string();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (id, db) = runtime.block_on(locked_clean_exit(root.path(), &session));
            let invalid_time = "2099-06-15T00:00:00Z\0not a timestamp";
            db.execute(
                "UPDATE tasks SET body=json_set(body,'$.output',json(?2),'$.status',json(?3),'$.updatedAt',?4) WHERE id=?1",
                rusqlite::params![id, token, source_status, invalid_time],
            ).unwrap();
            let body = || {
                db.query_row("SELECT body FROM tasks WHERE id=?1", [&id], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap()
            };
            let before = body();
            db.execute_batch("COMMIT").unwrap();
            drop(runtime); // Join the real owner retry before inspecting the raw SQLite row.
            if should_finish {
                let row: Value = serde_json::from_str(&body()).unwrap();
                assert_eq!(row["status"], "completed");
                let updated_at =
                    chrono::DateTime::parse_from_rfc3339(row["updatedAt"].as_str().unwrap())
                        .unwrap();
                assert!(updated_at <= chrono::Utc::now());
                let summary: Summary =
                    serde_json::from_str(row["output"].as_str().unwrap()).unwrap();
                assert_eq!(summary.task_output.get().as_bytes(), token.as_bytes());
            } else {
                assert_eq!(body(), before);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn monitor_notification_sanitizes_the_entire_subject_past_a_nul() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = start_monitor(root.path(), &session, "exit 0", DEFAULT_TIMEOUT).await;
        let id = task["task_id"].as_str().unwrap();
        wait_finished(root.path(), &session, id).await;
        let subject = "before\0after<✓>";
        let edit = crate::tools::tasks::execute_for_session(
            "TaskUpdate",
            json!({"id":id,"subject":subject}),
            root.path(),
            &session,
        )
        .unwrap();
        assert_eq!(edit["subject"], subject);
        let events = collect_events(root.path(), &session).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].description, "beforeafter✓");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn task_get_and_list_read_a_readonly_database_after_a_busy_monitor_exit() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let (id, db) = locked_clean_exit(root.path(), &session).await;
        let path = root
            .path()
            .join("data/session-tasks")
            .join(format!("{session}.sqlite3"));
        let permissions = std::fs::metadata(&path).unwrap().permissions();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        db.execute_batch("ROLLBACK").unwrap();
        let read = |tool, args| {
            crate::tools::tasks::execute_for_session(tool, args, root.path(), &session).unwrap()
        };
        let get = read("TaskGet", json!({"id":id,"block":false}));
        assert_eq!(get["task_id"], id);
        let list = read("TaskList", json!({}));
        assert_eq!(list["tasks"][0]["id"], id);
        assert_eq!(list["tasks"][0]["status"], stored_task(&db, &id)["status"]);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dropping_an_unpolled_monitor_runner_finalizes_its_record() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let task = runtime.block_on(start_monitor(
            root.path(),
            &session,
            "exit 0",
            DEFAULT_TIMEOUT,
        ));
        let id = task["task_id"].as_str().unwrap();
        // No yield after synchronous startup. Shutdown drops the unpolled future.
        drop(runtime);
        let record = crate::tools::tasks::read_for_session(id, root.path(), &session).unwrap();
        assert_eq!(record["status"], "cancelled", "{record}");
    }

    /// Polls the task record until the monitor leaves pending/running.
    #[cfg(unix)]
    async fn wait_finished(root: &Path, session: &str, id: &str) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let value = crate::tools::tasks::execute_for_session(
                "TaskGet",
                json!({"task_id":id,"block":false}),
                root,
                session,
            )
            .unwrap();
            if !matches!(value["status"].as_str(), Some("pending" | "running")) {
                return value;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "monitor did not finish: {value}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn legacy_interval_begins_when_the_monitor_is_first_polled() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = start_monitor(
            root.path(),
            &session,
            "exec sleep 60",
            Duration::from_secs(30),
        )
        .await;
        let id = task["task_id"].as_str().unwrap();
        // On this current-thread runtime, advance moves the clock before the
        // spawned monitor gets its first chance to run.
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::time::resume();
        let early = tokio::time::timeout(
            Duration::from_secs(2),
            wait_finished(root.path(), &session, id),
        )
        .await;
        if early.is_err() {
            crate::tools::tasks::execute_for_session_async(
                "TaskStop",
                json!({"task_id":id}),
                root.path(),
                &session,
            )
            .await
            .unwrap();
            let stopped = wait_finished(root.path(), &session, id).await;
            assert_eq!(stopped["status"], "cancelled", "{stopped}");
        }
        assert!(
            early.is_err(),
            "legacy interval elapsed before the monitor was polled: {early:?}"
        );
    }

    #[cfg(unix)]
    async fn start_ready_renewing_tool(
        root: &Path,
        session: &str,
        timeout_ms: u64,
        cap_ms: u64,
    ) -> (Value, mpsc::UnboundedReceiver<TestObservation>, Event) {
        let script =
            r#"printf 'ready %s\n' "$$"; read -r release < release && printf 'done %s\n' "$$""#;
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        assert!(std::process::Command::new("mkfifo")
            .arg(workspace.join("release"))
            .status()
            .unwrap()
            .success());
        let (observer, mut observations) = mpsc::unbounded_channel();
        let task = TEST_OBSERVER
            .scope(
                observer,
                scope(Some((root.to_path_buf(), session.to_owned())), async {
                    crate::tools::execute_tool_async(
                        "Monitor",
                        json!({"command":script,"description":"renewal fixture","shell":"sh",
                           "timeout_ms":timeout_ms,"max_runtime_ms":cap_ms}),
                        workspace.to_str().unwrap(),
                    )
                    .await
                    .unwrap()
                }),
            )
            .await;
        let Some(TestObservation::BatchQueued(queued)) = observations.recv().await else {
            panic!("the script did not queue its ready line");
        };
        advance_past(queued + BATCH).await;
        assert_eq!(
            observations.recv().await,
            Some(TestObservation::BatchFlushed)
        );
        let events = collect_events(root, session).unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        let ready = events.into_iter().next().unwrap();
        assert_eq!(ready.kind, "output");
        assert!(ready
            .text
            .strip_prefix("ready ")
            .unwrap()
            .parse::<i32>()
            .is_ok());
        (task, observations, ready)
    }

    #[cfg(unix)]
    async fn advance_past(deadline: tokio::time::Instant) {
        // Tokio timers round the requested deadline up to millisecond ticks.
        let target = deadline + Duration::from_millis(1);
        tokio::time::advance(target - tokio::time::Instant::now()).await;
    }

    #[cfg(unix)]
    async fn release_script(root: &Path) {
        use rustix::fs::{open, Mode, OFlags};
        let fifo = root.join("workspace/release");
        let mut writer = loop {
            match open(
                &fifo,
                OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => break std::fs::File::from(fd),
                Err(rustix::io::Errno::NXIO) => tokio::task::yield_now().await,
                Err(error) => panic!("could not open the script release pipe: {error}"),
            }
        };
        writer.write_all(b"exit\n").unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn renewal_keeps_the_original_script_running_until_it_exits() {
        manual_clock::run(async {
            let root = tempfile::tempdir().unwrap();
            let root = root.path();
            let session = uuid::Uuid::new_v4().to_string();
            let started = tokio::time::Instant::now();
            let interval = Duration::from_secs(1);
            let (task, mut observations, ready) =
                start_ready_renewing_tool(root, &session, 1_000, 20_000).await;
            let id = task["task_id"].as_str().unwrap();
            assert_eq!(task["max_runtime_ms"], 20_000, "{task}");
            advance_past(started + interval).await;
            assert_eq!(
                observations.recv().await,
                Some(TestObservation::Renewed(
                    tokio::time::Instant::now() + interval
                ))
            );
            let running = crate::tools::tasks::read_for_session(id, root, &session).unwrap();
            assert_eq!(running["status"], "running", "{running}");
            assert_eq!(running["metadata"]["max_runtime_ms"], 20_000, "{running}");

            release_script(root).await;
            while observations.recv().await.unwrap() != TestObservation::Finished {}
            let finished = crate::tools::tasks::read_for_session(id, root, &session).unwrap();
            assert_eq!(finished["status"], "completed", "{finished}");
            let events = collect_events(root, &session).unwrap();
            let kinds: Vec<_> = events.iter().map(|event| event.kind.as_str()).collect();
            assert_eq!(kinds, ["renewed", "output", "exited"], "{events:?}");
            let pid = ready.text.strip_prefix("ready ").unwrap();
            assert_eq!(events[1].text, format!("done {pid}"), "{events:?}");
            assert!(events[0]
                .notification()
                .contains("<status>renewed</status>"));
            assert_eq!(ready.task_id, id);
            assert!(events.iter().all(|event| event.task_id == id));
            assert!(collect_events(root, &session).unwrap().is_empty());
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn task_stop_ends_a_renewing_monitor() {
        manual_clock::run(async {
            let root = tempfile::tempdir().unwrap();
            let root = root.path();
            let session = uuid::Uuid::new_v4().to_string();
            let started = tokio::time::Instant::now();
            let interval = Duration::from_secs(1);
            let (task, mut observations, _) =
                start_ready_renewing_tool(root, &session, 1_000, 20_000).await;
            let id = task["task_id"].as_str().unwrap();
            advance_past(started + interval).await;
            assert_eq!(
                observations.recv().await,
                Some(TestObservation::Renewed(
                    tokio::time::Instant::now() + interval
                ))
            );

            let stopped = crate::tools::tasks::execute_for_session_async(
                "TaskStop",
                json!({"task_id":id}),
                root,
                &session,
            )
            .await
            .unwrap();
            assert_eq!(stopped["status"], "running", "{stopped}");
            assert_eq!(stopped["cancellation_requested"], true, "{stopped}");
            tokio::time::advance(Duration::from_millis(51)).await;
            assert_eq!(observations.recv().await, Some(TestObservation::Ending));
            assert_eq!(observations.recv().await, Some(TestObservation::Finished));
            let finished = crate::tools::tasks::read_for_session(id, root, &session).unwrap();
            assert_eq!(finished["status"], "cancelled", "{finished}");
            let events = collect_events(root, &session).unwrap();
            let kinds: Vec<_> = events.iter().map(|event| event.kind.as_str()).collect();
            assert_eq!(kinds, ["renewed", "stopped"], "{events:?}");
            assert!(collect_events(root, &session).unwrap().is_empty());
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn runtime_cap_truncates_the_last_interval_and_short_renewals_do_not_flood() {
        manual_clock::run(async {
            let root = tempfile::tempdir().unwrap();
            let root = root.path();
            let session = uuid::Uuid::new_v4().to_string();
            let started = tokio::time::Instant::now();
            let interval = Duration::from_secs(2);
            let cap = started + Duration::from_secs(5);
            let (task, mut observations, ready) =
                start_ready_renewing_tool(root, &session, 2_000, 5_000).await;
            let id = task["task_id"].as_str().unwrap();
            #[cfg(target_os = "linux")]
            let process = {
                use rustix::process::{pidfd_open, Pid, PidfdFlags};
                let pid = ready.text.strip_prefix("ready ").unwrap().parse().unwrap();
                let fd = pidfd_open(Pid::from_raw(pid).unwrap(), PidfdFlags::empty()).unwrap();
                tokio::io::unix::AsyncFd::new(fd).unwrap()
            };
            assert_eq!(ready.task_id, id);

            advance_past(started + interval).await;
            let second = tokio::time::Instant::now() + interval;
            assert_eq!(
                observations.recv().await,
                Some(TestObservation::Renewed(second))
            );
            advance_past(second).await;
            assert_eq!(
                observations.recv().await,
                Some(TestObservation::Renewed(cap))
            );

            tokio::time::advance(cap - tokio::time::Instant::now() - Duration::from_millis(1))
                .await;
            assert!(matches!(
                observations.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            let running = crate::tools::tasks::read_for_session(id, root, &session).unwrap();
            assert_eq!(running["status"], "running", "{running}");
            advance_past(cap).await;
            assert_eq!(observations.recv().await, Some(TestObservation::Ending));
            #[cfg(target_os = "linux")]
            drop(process.readable().await.unwrap());

            // Process exit is checked with the clock held at the cap. The task
            // record and terminal event can follow once the readers finish.
            assert_eq!(observations.recv().await, Some(TestObservation::Finished));
            let finished = crate::tools::tasks::read_for_session(id, root, &session).unwrap();
            assert_eq!(finished["status"], "completed", "{finished}");
            let events = collect_events(root, &session).unwrap();
            let kinds: Vec<_> = events.iter().map(|event| event.kind.as_str()).collect();
            assert_eq!(kinds, ["renewed", "expired"], "{events:?}");
            assert!(events[1].text.contains("5000 ms runtime cap"), "{events:?}");
            assert!(collect_events(root, &session).unwrap().is_empty());
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lines_become_ordered_events_and_are_claimed_once() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = start_monitor(
            root.path(),
            &session,
            "echo one; sleep 0.5; echo two; sleep 0.5",
            DEFAULT_TIMEOUT,
        )
        .await;
        assert_eq!(task["status"], "running");
        assert_eq!(task["description"], "fixture");
        assert_eq!(
            task["timeout_ms"],
            json!(DEFAULT_TIMEOUT.as_millis() as u64)
        );
        let id = task["task_id"].as_str().unwrap().to_owned();
        let finished = wait_finished(root.path(), &session, &id).await;
        assert_eq!(finished["status"], "completed", "{finished}");
        let events = collect_events(root.path(), &session).unwrap();
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!(
            (events[0].kind.as_str(), events[0].text.as_str()),
            ("output", "one")
        );
        assert_eq!(
            (events[1].kind.as_str(), events[1].text.as_str()),
            ("output", "two")
        );
        assert_eq!(events[2].kind, "exited");
        assert!(events[2].text.contains("code 0"), "{}", events[2].text);
        assert!(events.iter().all(|event| event.task_id == id));
        assert!(collect_events(root.path(), &session).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn close_lines_are_batched() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = start_monitor(
            root.path(),
            &session,
            "echo a; echo b; echo c",
            DEFAULT_TIMEOUT,
        )
        .await;
        let id = task["task_id"].as_str().unwrap().to_owned();
        let finished = wait_finished(root.path(), &session, &id).await;
        assert_eq!(finished["status"], "completed", "{finished}");
        let events = collect_events(root.path(), &session).unwrap();
        let output: Vec<_> = events
            .iter()
            .filter(|event| event.kind == "output")
            .collect();
        assert_eq!(output.len(), 1, "{events:?}");
        assert_eq!(output[0].text, "a\nb\nc");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_expires_and_kills_the_tree() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let started = std::time::Instant::now();
        let task = start_monitor(
            root.path(),
            &session,
            "echo start; sleep 30",
            Duration::from_secs(1),
        )
        .await;
        let id = task["task_id"].as_str().unwrap().to_owned();
        let finished = wait_finished(root.path(), &session, &id).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the monitor and its child outlived the timeout: {finished}"
        );
        assert_eq!(finished["status"], "completed", "{finished}");
        let events = collect_events(root.path(), &session).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.kind == "output" && event.text == "start"),
            "{events:?}"
        );
        assert_eq!(events.last().unwrap().kind, "expired", "{events:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_file_stops_it() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = start_monitor(
            root.path(),
            &session,
            "while true; do sleep 1; done",
            DEFAULT_TIMEOUT,
        )
        .await;
        let id = task["task_id"].as_str().unwrap().to_owned();
        std::fs::write(
            root.path()
                .join("tasks/shell")
                .join(&session)
                .join(&id)
                .join("cancel"),
            b"cancel",
        )
        .unwrap();
        let finished = wait_finished(root.path(), &session, &id).await;
        assert_eq!(finished["status"], "cancelled", "{finished}");
        let events = collect_events(root.path(), &session).unwrap();
        assert_eq!(events.last().unwrap().kind, "stopped", "{events:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn flooding_stops_it() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = start_monitor(
            root.path(),
            &session,
            "i=0; while [ $i -lt 100 ]; do echo $i; i=$((i+1)); sleep 0.25; done",
            DEFAULT_TIMEOUT,
        )
        .await;
        let id = task["task_id"].as_str().unwrap().to_owned();
        let finished = wait_finished(root.path(), &session, &id).await;
        assert_eq!(finished["status"], "failed", "{finished}");
        let events = collect_events(root.path(), &session).unwrap();
        assert_eq!(events.last().unwrap().kind, "flooded", "{events:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_is_logged_not_notified() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = start_monitor(root.path(), &session, "echo err >&2", DEFAULT_TIMEOUT).await;
        let id = task["task_id"].as_str().unwrap().to_owned();
        wait_finished(root.path(), &session, &id).await;
        let events = collect_events(root.path(), &session).unwrap();
        assert!(
            events.iter().all(|event| event.kind != "output"),
            "{events:?}"
        );
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, "exited");
        let log = std::fs::read_to_string(
            root.path()
                .join("tasks/shell")
                .join(&session)
                .join(&id)
                .join("stderr.log"),
        )
        .unwrap();
        assert!(log.contains("err"), "{log:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn monitor_tool_runs_through_the_bash_path() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let cwd = root.path().to_string_lossy().into_owned();
        let (owned_root, owned_session) = (root.path().to_path_buf(), session.clone());
        let task = scope(Some((owned_root, owned_session)), async move {
            crate::tools::execute_tool_async(
                "Monitor",
                json!({"command":"echo hi","description":"probe"}),
                &cwd,
            )
            .await
            .unwrap()
        })
        .await;
        assert_eq!(task["status"], "running", "{task}");
        assert_eq!(task["description"], "probe");
        let id = task["task_id"].as_str().unwrap().to_owned();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut events = Vec::new();
        loop {
            events.extend(collect_events(root.path(), &session).unwrap());
            let output = events
                .iter()
                .any(|event| event.kind == "output" && event.text == "hi");
            let exited = events.iter().any(|event| event.kind == "exited");
            if output && exited {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "monitor events did not arrive: {events:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(events.iter().all(|event| event.task_id == id));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn task_stop_stops_a_monitor() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let cwd = root.path().to_string_lossy().into_owned();
        let (owned_root, owned_session) = (root.path().to_path_buf(), session.clone());
        let task = scope(Some((owned_root, owned_session)), async move {
            crate::tools::execute_tool_async(
                "Monitor",
                json!({"command":"while true; do sleep 1; done","description":"watch"}),
                &cwd,
            )
            .await
            .unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap().to_owned();
        let stopped = crate::tools::tasks::execute_for_session_async(
            "TaskStop",
            json!({"task_id":id}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert_eq!(stopped["cancellation_requested"], true, "{stopped}");
        assert_eq!(stopped["task_id"], id, "{stopped}");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let events = collect_events(root.path(), &session).unwrap();
            if events.iter().any(|event| event.kind == "stopped") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the monitor did not report that it stopped: {events:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let finished = wait_finished(root.path(), &session, &id).await;
        assert_eq!(finished["status"], "cancelled", "{finished}");
    }

    #[test]
    fn notification_format_is_a_task_notification() {
        let event = Event {
            task_id: "t1".into(),
            description: "build errors".into(),
            kind: "output".into(),
            text: "<b>error[E0308]</b>".into(),
        };
        assert_eq!(
            event.summary(),
            "Monitor event: \"build errors\": <b>error[E0308]</b>"
        );
        let text = event.notification();
        assert!(text.starts_with(NOTIFICATION_START), "{text}");
        assert!(text.contains("<kind>monitor</kind>"), "{text}");
        assert!(text.contains("<status>output</status>"), "{text}");
        assert!(text.contains("‹b›error[E0308]‹/b›"), "{text}");
        assert!(!text.contains("<b>"), "{text}");
        assert!(text.ends_with("</task-notification>"), "{text}");
    }
}
