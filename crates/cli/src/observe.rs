//! Read-only observation of a turn owned by another process.
//!
//! A direct `--print`/stream-json run keeps the session execution lease for its
//! whole turn and publishes bounded intermediate assistant/tool rows. Another
//! terminal may resume the same session; it must show that progress without
//! taking the lease, submitting input, running tools, or cancelling the owner.
use crate::session::Session;
use anyhow::Result;
use deepseek_code_core::{
    db::{message_repo, turn_repo, DbManager},
    services::session_messages,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

/// An unterminated line is shown in chunks of about this many characters, so a
/// long single line still reads as live progress.
const LONG_LINE: usize = 200;
/// A finished owner releases the lease before its host writes the endpoint in
/// some paths; settle only after this long with no published progress.
const SETTLE_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No owner is executing and this observer did not watch one end.
    Idle,
    Running,
    /// The turn this observer watched reached a terminal state.
    Finished,
}

/// Per assistant segment. A turn may span several segments when a recovery or
/// a compaction replaces the answer, and each keeps its own cursor so polling
/// never replays an older segment.
#[derive(Default)]
struct Cursor {
    text_seen: usize,
    pending: String,
    /// Tool call id → whether its result has been shown.
    tools: BTreeMap<String, bool>,
}

#[derive(Default)]
pub struct Observer {
    announced: bool,
    session: Option<String>,
    turn: Option<String>,
    segments: BTreeMap<String, Cursor>,
    idle_since: Option<Instant>,
}

/// Whether some process holds execution ownership, whether a managed worker or
/// a direct print/json run. This only probes the lease; it never acquires it.
pub fn execution_lease_active(session: &Session) -> Result<bool> {
    session_messages::execution_active(&session.paths.data_dir, &session.id)
}

/// The truthful resume notice for a turn owned elsewhere.
pub fn running_notice(session: &Session) -> Result<String> {
    let turn = session
        .store
        .db
        .with_conn(|conn| turn_repo::running_turn(conn, &session.id))??;
    Ok(running_notice_for(turn.as_ref()))
}

/// Whether the last turn ended because its owner disappeared.
pub fn latest_interrupted(session: &Session) -> Result<bool> {
    let latest = session
        .store
        .db
        .with_conn(|conn| turn_repo::latest_turn(conn, &session.id))??;
    Ok(latest.is_some_and(|turn| turn.interrupted()))
}

fn running_notice_for(turn: Option<&turn_repo::TurnRecord>) -> String {
    let since = turn.and_then(|record| {
        chrono::DateTime::parse_from_rfc3339(&record.started_at)
            .ok()
            .map(|time| {
                time.with_timezone(&chrono::Local)
                    .format("%-I:%M %p")
                    .to_string()
            })
    });
    match since {
        Some(time) => format!(
            "✻ A turn is still running in another process since {time}; this terminal only observes its published progress and closing it does not stop that turn."
        ),
        None => "✻ A turn is still running in another process; this terminal only observes its published progress and closing it does not stop that turn.".into(),
    }
}

impl Observer {
    pub fn poll(&mut self, session: &Session) -> Result<(State, Vec<String>)> {
        self.poll_at(&session.paths.data_dir, &session.store.db, &session.id)
    }

    /// Polls the published state. Never takes the lease, submits input, answers
    /// a permission request, or executes a tool.
    pub(crate) fn poll_at(
        &mut self,
        root: &Path,
        db: &Arc<DbManager>,
        session_id: &str,
    ) -> Result<(State, Vec<String>)> {
        let mut lines = Vec::new();
        // A resumed different session starts from a clean view.
        if self.session.as_deref() != Some(session_id) {
            self.reset(Some(session_id.to_owned()));
        }
        if session_messages::execution_active(root, session_id)? {
            let turn = db.with_conn(|conn| turn_repo::running_turn(conn, session_id))??;
            self.idle_since = None;
            if !self.announced {
                self.announced = true;
                self.turn = turn.as_ref().map(|record| record.turn_id.clone());
                lines.push(running_notice_for(turn.as_ref()));
            }
            if let Some(turn) = turn {
                if self.turn.as_deref() != Some(turn.turn_id.as_str()) {
                    // A new turn starts a new cursor set; an older turn's text
                    // must not be replayed into it.
                    self.turn = Some(turn.turn_id.clone());
                    self.segments.clear();
                }
                self.render(db, &turn.turn_id, &mut lines)?;
            }
            return Ok((State::Running, lines));
        }
        if !self.announced {
            return Ok((State::Idle, lines));
        }
        // Drain whatever the owner published between the last active poll and
        // its lease disappearing; those final lines are otherwise lost.
        if let Some(turn) = self.turn.clone() {
            self.render(db, &turn, &mut lines)?;
            self.flush_all(&mut lines);
        }
        let turn = self
            .turn
            .as_deref()
            .and_then(|id| {
                db.with_conn(|conn| turn_repo::get_turn(conn, id))
                    .ok()?
                    .ok()
            })
            .flatten();
        let running = turn
            .as_ref()
            .is_some_and(|record| record.status == turn_repo::TurnStatus::Running);
        let notice = if running {
            // Do not label a still-running turn complete. Wait out the brief
            // finalization window, then settle under the lease so an owner that
            // is still alive is never marked interrupted.
            let since = *self.idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() < SETTLE_GRACE {
                return Ok((State::Running, lines));
            }
            match settle_if_abandoned(root, db, session_id) {
                Some(settled) => finished_notice(Some(&settled)),
                None => return Ok((State::Running, lines)),
            }
        } else {
            finished_notice(turn.as_ref())
        };
        self.announced = false;
        self.turn = None;
        self.segments.clear();
        self.idle_since = None;
        lines.push(notice);
        Ok((State::Finished, lines))
    }

    fn reset(&mut self, session: Option<String>) {
        self.announced = false;
        self.session = session;
        self.turn = None;
        self.segments.clear();
        self.idle_since = None;
    }

    fn render(
        &mut self,
        db: &Arc<DbManager>,
        turn_id: &str,
        lines: &mut Vec<String>,
    ) -> Result<()> {
        let segments = db.with_conn(
            |conn| -> Result<Vec<(message_repo::MessageRow, Vec<message_repo::ToolCallRow>)>> {
                let mut segments = Vec::new();
                for id in turn_repo::turn_messages(conn, turn_id)? {
                    if let Some(row) = message_repo::get_message(conn, &id)? {
                        let tools = message_repo::get_tool_calls_for_message(conn, &id)?;
                        segments.push((row, tools));
                    }
                }
                Ok(segments)
            },
        )??;
        let mut present = BTreeSet::new();
        for (row, tools) in segments {
            present.insert(row.id.clone());
            self.segments
                .entry(row.id.clone())
                .or_default()
                .refresh(&row, &tools, lines);
        }
        // A recovery removes a segment; its cursor would only replay if the id
        // ever came back.
        self.segments.retain(|id, _| present.contains(id));
        Ok(())
    }

    fn flush_all(&mut self, lines: &mut Vec<String>) {
        for cursor in self.segments.values_mut() {
            cursor.flush(lines, usize::MAX);
        }
    }
}

impl Cursor {
    fn refresh(
        &mut self,
        row: &message_repo::MessageRow,
        tools: &[message_repo::ToolCallRow],
        lines: &mut Vec<String>,
    ) {
        // The owner rewrites the row with the same accumulated prefix as the
        // turn grows, so only new bytes are public progress.
        if row.content.len() > self.text_seen && row.content.is_char_boundary(self.text_seen) {
            self.pending.push_str(&row.content[self.text_seen..]);
            self.text_seen = row.content.len();
        }
        self.flush(lines, 40);
        for tool in tools {
            match self.tools.get(&tool.id).copied() {
                None => {
                    lines.push(tool_line(tool));
                    match &tool.output {
                        Some(output) => {
                            lines.push(tool_result_line(tool, output));
                            self.tools.insert(tool.id.clone(), true);
                        }
                        None => {
                            self.tools.insert(tool.id.clone(), false);
                        }
                    }
                }
                Some(false) => {
                    if let Some(output) = &tool.output {
                        lines.push(tool_result_line(tool, output));
                        self.tools.insert(tool.id.clone(), true);
                    }
                }
                Some(true) => {}
            }
        }
    }

    /// Emits complete lines, then chunks of an unterminated one so long text
    /// still shows progress instead of being buffered indefinitely.
    fn flush(&mut self, lines: &mut Vec<String>, limit: usize) {
        let mut emitted = 0;
        while emitted < limit {
            let Some(index) = self.pending.find('\n') else {
                break;
            };
            let line: String = self.pending.drain(..=index).collect();
            let line = line.trim_end();
            if !line.is_empty() {
                lines.push(format!("  {line}"));
            }
            emitted += 1;
        }
        while emitted < limit && self.pending.chars().count() >= LONG_LINE {
            let cut = self
                .pending
                .char_indices()
                .nth(LONG_LINE)
                .map(|(index, _)| index)
                .unwrap_or(self.pending.len());
            let chunk: String = self.pending.drain(..cut).collect();
            let chunk = chunk.trim_end();
            if !chunk.is_empty() {
                lines.push(format!("  {chunk}…"));
            }
            emitted += 1;
        }
        if limit == usize::MAX && !self.pending.trim().is_empty() {
            let line = self.pending.trim_end().to_owned();
            lines.push(format!("  {line}"));
            self.pending.clear();
        }
    }
}

/// Settles a turn whose owner vanished. The lease is acquired first, so a live
/// owner is never marked interrupted, and released immediately after.
fn settle_if_abandoned(
    root: &Path,
    db: &Arc<DbManager>,
    session_id: &str,
) -> Option<turn_repo::TurnRecord> {
    let lease = session_messages::ExecutionLease::acquire(root, session_id).ok()??;
    let settled = db
        .with_conn(|conn| -> Result<Option<turn_repo::TurnRecord>> {
            turn_repo::settle_interrupted(conn, session_id)?;
            Ok(turn_repo::latest_turn(conn, session_id)?)
        })
        .ok()?
        .ok()?;
    drop(lease);
    settled
}

/// A one-line hint from the public tool input; never the whole payload.
fn tool_hint(input: Option<&str>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return String::new();
    };
    for key in [
        "file_path",
        "path",
        "command",
        "pattern",
        "url",
        "description",
        "prompt",
    ] {
        if let Some(text) = value.get(key).and_then(|value| value.as_str()) {
            let line = text.lines().next().unwrap_or_default();
            let mut short: String = line.chars().take(80).collect();
            if line.chars().count() > 80 {
                short.push_str("...");
            }
            return short;
        }
    }
    String::new()
}

fn tool_line(tool: &message_repo::ToolCallRow) -> String {
    let hint = tool_hint(tool.input.as_deref());
    if hint.is_empty() {
        format!("● {}", tool.name)
    } else {
        format!("● {}  {hint}", tool.name)
    }
}

fn tool_result_line(tool: &message_repo::ToolCallRow, output: &str) -> String {
    if tool.is_error {
        return format!("  ✗ {} · error", tool.name);
    }
    let lines = output.lines().count().max(1);
    format!(
        "  ✓ {} · ok · {lines} line{}",
        tool.name,
        if lines == 1 { "" } else { "s" }
    )
}

fn finished_notice(turn: Option<&turn_repo::TurnRecord>) -> String {
    use turn_repo::TurnStatus;
    let Some(turn) = turn else {
        return "✻ Observed turn ended.".into();
    };
    match turn.status {
        TurnStatus::Completed => format!(
            "✻ Observed turn finished · Worked for {} · done",
            duration(turn.elapsed_ms)
        ),
        TurnStatus::Failed => format!(
            "✻ Observed turn failed · Worked for {}",
            duration(turn.elapsed_ms)
        ),
        TurnStatus::Cancelled => {
            if turn.interrupted() {
                "✻ Observed turn ended · interrupted · duration unknown".into()
            } else {
                "✻ Observed turn ended · interrupted".into()
            }
        }
        TurnStatus::Running => "✻ Observed turn ended.".into(),
    }
}

fn duration(ms: Option<u64>) -> String {
    match ms {
        Some(ms) => crate::terminal::progress::human_duration(std::time::Duration::from_millis(ms)),
        None => "an unknown duration".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Arc<DbManager>, String) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DbManager::new(temp.path().join("obs.sqlite")).unwrap());
        db.init().unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(db.clone());
        let session = store.create(temp.path(), "model", "provider").unwrap();
        (temp, db, session)
    }

    fn write_text(db: &Arc<DbManager>, message: &str, content: &str) {
        db.with_conn(|conn| message_repo::update_message_content(conn, message, content))
            .unwrap()
            .unwrap();
    }

    fn save_tool(
        db: &Arc<DbManager>,
        session: &str,
        message: &str,
        id: &str,
        output: Option<&str>,
    ) {
        let content = db
            .with_conn(|conn| message_repo::get_message(conn, message))
            .unwrap()
            .unwrap()
            .unwrap()
            .content;
        db.with_conn(|conn| {
            message_repo::save_stream_tool_result(
                conn,
                session,
                message,
                &content,
                &message_repo::ToolCallRow {
                    id: format!("{message}:{id}"),
                    message_id: message.to_owned(),
                    name: "Read".into(),
                    input: Some("{\"file_path\":\"fixture.txt\"}".into()),
                    output: output.map(str::to_owned),
                    is_error: false,
                    sort_order: 0,
                },
            )
        })
        .unwrap()
        .unwrap();
    }

    #[test]
    fn published_progress_is_rendered_once_and_completion_reports_the_measured_turn() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::start(db.clone(), &session_id).unwrap();
        let message = timer.ensure_segment().unwrap();
        write_text(&db, &message, "first line\nsecond");
        save_tool(&db, &session_id, &message, "read-1", None);
        // The owner's lease: the observer only probes it.
        let lease = session_messages::ExecutionLease::acquire(root, &session_id)
            .unwrap()
            .unwrap();
        let mut observer = Observer::default();
        let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(state, State::Running);
        assert!(lines[0].contains("still running in another process"));
        assert!(lines.iter().any(|line| line.contains("first line")));
        assert!(lines
            .iter()
            .any(|line| line.contains("● Read") && line.contains("fixture.txt")));
        assert!(
            !lines.iter().any(|line| line.contains("second")),
            "partial lines wait"
        );

        // Growing text and a finished tool appear on the next poll only.
        write_text(&db, &message, "first line\nsecond line\n");
        save_tool(
            &db,
            &session_id,
            &message,
            "read-1",
            Some("fixture contents\n"),
        );
        let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(state, State::Running);
        assert!(lines.iter().any(|line| line.contains("second line")));
        assert!(
            !lines.iter().any(|line| line.contains("first line")),
            "already shown"
        );
        assert!(lines.iter().any(|line| line.contains("✓ Read")));

        // Idle polls repeat nothing.
        let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(state, State::Running);
        assert!(lines.is_empty(), "{lines:?}");

        // The owner exits: the same poll reports the measured completion once.
        drop(lease);
        timer.finish(turn_repo::TurnStatus::Completed).unwrap();
        let turn_id = timer.record().turn_id.clone();
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE execution_turns SET elapsed_ms=128000 WHERE id=?1",
                [&turn_id],
            )
        })
        .unwrap()
        .unwrap();
        let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(state, State::Finished);
        assert!(lines
            .iter()
            .any(|line| line.contains("Observed turn finished")));
        assert!(lines[0].contains("2m 8s"));
        let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(state, State::Idle);
        assert!(lines.is_empty(), "a finished observation is not repeated");
    }

    #[test]
    fn multiple_segments_keep_their_own_cursors_and_never_replay() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::start(db.clone(), &session_id).unwrap();
        let first = timer.ensure_segment().unwrap();
        write_text(&db, &first, "first segment\n");
        save_tool(&db, &session_id, &first, "read-1", Some("one\n"));
        // A recovery replaced the answer with a second segment of the same turn.
        let second = timer.new_segment().unwrap();
        write_text(&db, &second, "second segment\n");
        save_tool(&db, &session_id, &second, "read-2", None);
        let lease = session_messages::ExecutionLease::acquire(root, &session_id)
            .unwrap()
            .unwrap();

        let mut observer = Observer::default();
        let (_, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert!(lines.iter().any(|line| line.contains("first segment")));
        assert!(lines.iter().any(|line| line.contains("✓ Read")));
        assert!(lines.iter().any(|line| line.contains("second segment")));
        assert!(lines.iter().any(|line| line.contains("● Read")));

        // Repeated idle polls must not replay either segment.
        for _ in 0..3 {
            let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
            assert_eq!(state, State::Running);
            assert!(lines.is_empty(), "replayed: {lines:?}");
        }

        // New progress in the current segment appears exactly once.
        write_text(&db, &second, "second segment\nthird line\n");
        save_tool(&db, &session_id, &second, "read-2", Some("two\n"));
        let (_, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("third line"))
                .count(),
            1,
            "{lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|line| line.contains("✓ Read")).count(),
            1,
            "{lines:?}"
        );
        let (_, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert!(lines.is_empty(), "{lines:?}");
        drop(lease);
    }

    #[test]
    fn completion_drains_the_final_text_and_tool_result_written_after_the_last_live_poll() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::start(db.clone(), &session_id).unwrap();
        let message = timer.ensure_segment().unwrap();
        write_text(&db, &message, "started\n");
        let lease = session_messages::ExecutionLease::acquire(root, &session_id)
            .unwrap()
            .unwrap();
        let mut observer = Observer::default();
        let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(state, State::Running);
        assert!(lines.iter().any(|line| line.contains("started")));

        // Everything below is written after the last poll that saw a live owner.
        write_text(&db, &message, "started\nfinal answer\n");
        save_tool(
            &db,
            &session_id,
            &message,
            "read-1",
            Some("fixture contents\n"),
        );
        drop(lease);
        timer.finish(turn_repo::TurnStatus::Completed).unwrap();
        let turn_id = timer.record().turn_id.clone();
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE execution_turns SET elapsed_ms=42000 WHERE id=?1",
                [&turn_id],
            )
        })
        .unwrap()
        .unwrap();

        let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(state, State::Finished);
        assert!(
            lines.iter().any(|line| line.contains("final answer")),
            "final text was dropped: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("✓ Read")),
            "final tool result was dropped: {lines:?}"
        );
        assert!(lines.iter().any(|line| line.contains("42s")));
    }

    #[test]
    fn a_dead_owner_observed_live_becomes_interrupted_without_replay() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::start(db.clone(), &session_id).unwrap();
        let message = timer.ensure_segment().unwrap();
        write_text(&db, &message, "partial answer\n");
        let lease = session_messages::ExecutionLease::acquire(root, &session_id)
            .unwrap()
            .unwrap();
        let mut observer = Observer::default();
        let (state, _) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(state, State::Running);

        // The process is killed: no endpoint is ever written, the lease is gone.
        drop(lease);
        std::mem::forget(timer);
        let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(
            state,
            State::Running,
            "the finalization window is respected"
        );
        assert!(lines.is_empty(), "{lines:?}");

        // After the grace period the turn is settled as interrupted, not complete.
        observer.idle_since = Some(Instant::now() - SETTLE_GRACE - Duration::from_millis(1));
        let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(state, State::Finished);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("interrupted · duration unknown")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("finished")),
            "a dead owner is not reported as completed: {lines:?}"
        );
        let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(state, State::Idle, "never stays running");
        assert!(lines.is_empty(), "never replays: {lines:?}");
        let record = db
            .with_conn(|conn| turn_repo::latest_turn(conn, &session_id))
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(record.interrupted());
        assert_eq!(record.elapsed_ms, None, "duration stays unknown");
    }

    #[test]
    fn a_live_turn_is_never_settled_while_its_lease_is_held() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::start(db.clone(), &session_id).unwrap();
        let message = timer.ensure_segment().unwrap();
        write_text(&db, &message, "working\n");
        let lease = session_messages::ExecutionLease::acquire(root, &session_id)
            .unwrap()
            .unwrap();
        let mut observer = Observer::default();
        observer.poll_at(root, &db, &session_id).unwrap();
        observer.idle_since = Some(Instant::now() - SETTLE_GRACE - Duration::from_millis(1));
        // The owner is alive: the probe must not settle its turn.
        assert!(
            session_messages::execution_active(root, &session_id).unwrap(),
            "the owner holds the lease"
        );
        drop(lease);
        let (state, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert_eq!(state, State::Finished);
        assert!(lines
            .iter()
            .any(|line| line.contains("interrupted · duration unknown")));
        drop(timer);
    }

    #[test]
    fn a_long_unterminated_line_still_shows_progress() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::start(db.clone(), &session_id).unwrap();
        let message = timer.ensure_segment().unwrap();
        let long = "x".repeat(LONG_LINE + 50);
        write_text(&db, &message, &long);
        let _lease = session_messages::ExecutionLease::acquire(root, &session_id)
            .unwrap()
            .unwrap();
        let mut observer = Observer::default();
        let (_, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("  xxxx") && line.ends_with('…')),
            "long text is buffered forever: {lines:?}"
        );
        write_text(&db, &message, &format!("{long}\nsecond"));
        let (_, lines) = observer.poll_at(root, &db, &session_id).unwrap();
        assert!(
            lines.iter().any(|line| line.contains("xxxxxxxx")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("second")),
            "partial line still waits: {lines:?}"
        );
    }

    #[test]
    fn switching_to_an_idle_session_does_not_replay_the_observed_turn() {
        let (temp, db, first) = fixture();
        let root = temp.path();
        let store = deepseek_code_core::runtime::SessionStore::new(db.clone());
        let second = store.create(temp.path(), "model", "provider").unwrap();
        let mut timer = turn_repo::TurnTimer::start(db.clone(), &first).unwrap();
        let message = timer.ensure_segment().unwrap();
        write_text(&db, &message, "first session text\n");
        let lease = session_messages::ExecutionLease::acquire(root, &first)
            .unwrap()
            .unwrap();
        let mut observer = Observer::default();
        let (_, lines) = observer.poll_at(root, &db, &first).unwrap();
        assert!(lines.iter().any(|line| line.contains("first session text")));

        // /resume to an idle session shows nothing from the observed one.
        let (state, lines) = observer.poll_at(root, &db, &second).unwrap();
        assert_eq!(state, State::Idle);
        assert!(lines.is_empty(), "{lines:?}");

        // Back to the first session: a fresh view re-announces it once.
        let (state, lines) = observer.poll_at(root, &db, &first).unwrap();
        assert_eq!(state, State::Running);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("first session text"))
                .count(),
            1
        );
        let (_, lines) = observer.poll_at(root, &db, &first).unwrap();
        assert!(lines.is_empty(), "{lines:?}");
        drop(lease);
        drop(timer);
    }

    #[test]
    fn an_idle_observer_stays_silent_and_a_settled_turn_is_interrupted() {
        let (temp, db, session_id) = fixture();
        let mut observer = Observer::default();
        let (state, lines) = observer.poll_at(temp.path(), &db, &session_id).unwrap();
        assert_eq!(state, State::Idle);
        assert!(lines.is_empty());

        let mut timer = turn_repo::TurnTimer::start(db.clone(), &session_id).unwrap();
        let message = timer.ensure_segment().unwrap();
        // Process loss: no Drop, no duration, no live owner.
        db.with_conn(|conn| {
            assert_eq!(
                turn_repo::settle_interrupted(conn, &session_id)
                    .unwrap()
                    .len(),
                1
            );
            let record = turn_repo::get_message_turn(conn, &message)
                .unwrap()
                .unwrap();
            assert!(record.interrupted());
            assert_eq!(record.status, turn_repo::TurnStatus::Cancelled);
            assert_eq!(record.elapsed_ms, None);
            assert_eq!(record.ended_at, None);
        })
        .unwrap();
        std::mem::forget(timer);
        let turn = db
            .with_conn(|conn| turn_repo::get_message_turn(conn, &message))
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(super::finished_notice(Some(&turn)).contains("interrupted"));
    }
}
