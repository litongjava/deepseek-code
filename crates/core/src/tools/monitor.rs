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
/// Lines arriving within this window form one event.
const BATCH: Duration = Duration::from_millis(200);
/// More events than this within one minute stops the monitor.
const MAX_EVENTS_PER_MINUTE: usize = 30;
/// A single stdout line is kept at this many characters.
const MAX_LINE_CHARS: usize = 4096;
const MAX_EVENT_LINES: usize = 50;
const MAX_EVENT_BYTES: usize = 16 * 1024;

/// Starts a monitor for the owning session; used by the Monitor tool and the
/// CLI tests.
pub fn start(
    mut command: Command,
    description: &str,
    command_text: &str,
    timeout: Duration,
    guard: Option<crate::services::wsl::Guard>,
) -> Result<Value> {
    anyhow::ensure!(
        !timeout.is_zero() && timeout <= MAX_TIMEOUT,
        "Monitor timeout must be between 1 second and 60 minutes"
    );
    let (root, session) = super::background_shell::CONTEXT
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .context("Monitor requires an owning engine session")?;
    let started_ms = chrono::Utc::now().timestamp_millis();
    let task = super::tasks::execute_for_shell_owner(
        "TaskCreate",
        // `monitor` marks the record for event collection; without
        // `managed_shell` the shell completion notice stays silent.
        json!({"subject":description,"metadata":{"monitor":true,"command":command_text,"started_ms":started_ms,"timeout_ms":timeout.as_millis() as u64}}),
        &root,
        &session,
    )?;
    let id = task["id"].as_str().context("Task ID missing")?.to_string();
    let directory = root.join("shell-tasks").join(&session).join(&id);
    std::fs::create_dir_all(&directory)?;
    let stdout_log = directory.join("stdout.log");
    let stderr_log = directory.join("stderr.log");
    std::fs::write(&stdout_log, b"")?;
    std::fs::write(&stderr_log, b"")?;
    let events_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("events.jsonl"))?;
    super::tasks::execute_for_shell_owner(
        "TaskUpdate",
        json!({"id":id,"status":"running"}),
        &root,
        &session,
    )?;
    std::fs::write(directory.join("heartbeat"), b"running")?;

    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = super::tasks::execute_for_shell_owner(
                "TaskUpdate",
                json!({"id":id,"status":"failed","output":json!({"is_error":true,"error":format!("{error:#}")}).to_string()}),
                &root,
                &session,
            );
            return Err(error).context("Starting monitor command");
        }
    };
    let tree = crate::services::tool_hooks::ProcessTree::new(
        child.id().context("Monitor process ID missing")?,
    )?;
    let stdout = child.stdout.take().context("Monitor stdout missing")?;
    let stderr = child.stderr.take().context("Monitor stderr missing")?;
    let (lines, mut output) = mpsc::unbounded_channel();
    let mut out_reader = tokio::spawn(read_lines(stdout, stdout_log, Some(lines)));
    let mut err_reader = tokio::spawn(read_lines(stderr, stderr_log, None));

    let message = format!(
        "Monitor \"{description}\" started (task {id}, stops after {} min). Each output line arrives as a notification; TaskStop stops it.",
        timeout.as_secs() / 60
    );
    let monitor_id = id.clone();
    tokio::spawn(async move {
        // The WSL lease must outlive the command; dropping it cancels it.
        let _guard = guard;
        let started = tokio::time::Instant::now();
        let deadline = started + timeout;
        let mut completion = Completion {
            root,
            session,
            id: monitor_id,
            done: false,
        };
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
                            batch_start = Some(tokio::time::Instant::now());
                        }
                        pending.push(line);
                    }
                    None => stdout_open = false,
                },
                status = child.wait() => {
                    break End::Exited(status.ok().and_then(|status| status.code()));
                }
                _ = tokio::time::sleep_until(deadline) => {
                    break End::Expired;
                }
                _ = ticker.tick() => {
                    if directory.join("cancel").exists() {
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
                        if emitter.flooded() {
                            break End::Flooded;
                        }
                    }
                }
            }
        };
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
            End::Expired => (
                "expired",
                format!("Monitor expired after {minutes}m with {events} events delivered. Start it again if you still need the watch."),
                "completed",
            ),
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
        };
        if let Err(error) = emitter.event(kind, &text) {
            tracing::warn!(%error, "Monitor final event write failed");
        }
        completion.finish(status, json!({"monitor":true,"kind":kind,"events":events}));
    });
    Ok(json!({
        "task_id": id,
        "description": description,
        "status": "running",
        "timeout_ms": timeout.as_millis() as u64,
        "message": message,
    }))
}

/// Why a monitor's process stopped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Exited(Option<i32>),
    Expired,
    Stopped,
    Flooded,
}

/// Finishes the task record even when the monitor future is dropped early.
struct Completion {
    root: PathBuf,
    session: String,
    id: String,
    done: bool,
}

impl Completion {
    fn finish(&mut self, status: &str, output: Value) {
        self.done = super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":self.id,"status":status,"output":output.to_string()}),
            &self.root,
            &self.session,
        )
        .is_ok();
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if !self.done {
            self.finish(
                "cancelled",
                json!({"is_error":true,"error":"Monitor owner exited or the monitor was cancelled"}),
            );
        }
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
    /// output, exited, expired, stopped or flooded.
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
    let path = root.join("tasks").join(format!("{session}.sqlite3"));
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
            "SELECT tasks.id, json_extract(body,'$.subject') FROM tasks WHERE json_extract(body,'$.metadata.monitor')=1 ORDER BY tasks.rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut events = Vec::new();
    for (id, subject) in tasks {
        let id = uuid::Uuid::parse_str(&id)?.to_string();
        let description: String = subject
            .as_deref()
            .unwrap_or("Monitor")
            .chars()
            .filter(|ch| !ch.is_control() && !matches!(ch, '<' | '>'))
            .take(160)
            .collect();
        let file = root
            .join("shell-tasks")
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
                .join("shell-tasks")
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
                .join("shell-tasks")
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
