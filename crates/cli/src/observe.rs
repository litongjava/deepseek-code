//! Read-only observation of a turn owned by another process.
//!
//! A direct `--print`/stream-json run keeps the session execution lease for its
//! whole turn and publishes bounded intermediate assistant/tool rows. Another
//! terminal may resume the same session; it must show that progress without
//! taking the lease, submitting input, running tools, or cancelling the owner.
//!
//! The observer reports semantic updates. The prompt editor renders them with
//! the same tool blocks, diff previews, Markdown and copy bindings a normal
//! foreground turn uses.
use crate::session::Session;
use anyhow::Result;
use deepseek_code_core::db::session_store::{SessionAccess, SessionRepository};
use deepseek_code_core::db::{message_repo, turn_repo};
#[cfg(test)]
use std::sync::Arc;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
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

/// One finished tool call. The editor turns this into the same expanded tool
/// block a foreground turn writes, including `#N`, the diff preview, the result
/// summary, elapsed time and copy bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUpdate {
    /// Assistant message id whose text preceded this call.
    pub segment: String,
    pub number: usize,
    pub name: String,
    pub input: Option<String>,
    pub output: String,
    pub is_error: bool,
    pub elapsed: Option<Duration>,
}

/// What the editor must render next. Updates are ordered: assistant text and
/// tools appear in publication order, and `Finish` closes any open Markdown
/// block before the terminal notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    Text { segment: String, lines: Vec<String> },
    Tool(ToolUpdate),
    Notice(String),
    Finish,
}

#[derive(Debug)]
pub struct PollResult {
    pub state: State,
    pub updates: Vec<Update>,
    /// Currently executing tool labels, shown in the pinned read-only status.
    pub activity: Option<String>,
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
    /// Calls still executing, kept with their transcript number for the
    /// "No result recorded" row if the turn ends first.
    running: BTreeMap<String, (message_repo::ToolCallRow, usize)>,
}

#[derive(Default)]
pub struct Observer {
    announced: bool,
    /// The REPL already printed its occupancy banner for this session. The
    /// first active poll renders published progress without a second notice.
    preannounced: bool,
    session: Option<String>,
    turn: Option<String>,
    segments: BTreeMap<String, Cursor>,
    idle_since: Option<Instant>,
    activity: Option<String>,
}

/// Whether some process holds execution ownership, whether a managed worker or
/// a direct print/json run. This only probes the lease; it never acquires it.
pub fn execution_lease_active(session: &Session) -> Result<bool> {
    session.selected_session()?.repository().execution_active(&session.paths.data_dir)
}

/// The truthful resume notice for a turn owned elsewhere.
pub fn running_notice(session: &Session) -> Result<String> {
    let turn = session.selected_session()?.repository().running_turn()?;
    Ok(running_notice_for(turn.as_ref()))
}

/// Whether the last turn ended because its owner disappeared.
pub fn latest_interrupted(session: &Session) -> Result<bool> {
    let latest = session.selected_session()?.repository().latest_turn()?;
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
    /// The caller already printed a truthful occupancy banner for this
    /// session; the first active poll must not repeat the running notice.
    pub fn preannounce(&mut self, session: &Session) -> Result<()> {
        self.preannounce_at(&session.store.db, &session.id)
    }

    fn preannounce_at(&mut self, db: &impl SessionAccess, session_id: &str) -> Result<()> {
        let selected = db.select_session(session_id)?;
        let identity = selected.observation_identity();
        // A different session starts a fresh view. The same session may already
        // have cursors seeded from a resumed screen; keep them so the banner
        // does not replay what the resume already rendered.
        if self.session.as_deref() != Some(identity) {
            self.reset(Some(identity.into()));
        }
        self.preannounced = true;
        Ok(())
    }

    /// Takes over cursors from the rich history `show_resumed` rendered. The
    /// rendered text and completed tools are not repeated; later publications
    /// continue after them.
    pub fn seed_from_resumed(&mut self, view: crate::repl::ResumedView) {
        self.reset(Some(view.identity));
        self.segments = view
            .messages
            .into_iter()
            .map(|(id, message)| {
                (
                    id,
                    Cursor {
                        text_seen: message.text_seen,
                        pending: String::new(),
                        tools: message
                            .tools
                            .into_iter()
                            .map(|tool| (tool, true))
                            .collect(),
                        running: BTreeMap::new(),
                    },
                )
            })
            .collect();
    }

    pub fn activity(&self) -> Option<&str> {
        self.activity.as_deref()
    }

    pub fn poll(&mut self, session: &Session) -> Result<PollResult> {
        self.poll_at(
            &session.paths.data_dir,
            &session.store.db,
            &session.id,
            Some(&session.workspace),
        )
    }

    /// Polls the published state. Never takes the lease, submits input, answers
    /// a permission request, or executes a tool.
    pub(crate) fn poll_at(
        &mut self,
        root: &Path,
        db: &impl SessionAccess,
        session_id: &str,
        workspace: Option<&Path>,
    ) -> Result<PollResult> {
        let mut updates = Vec::new();
        // A resumed different session starts from a clean view.
        let selected = db.select_session(session_id)?;
        if self.session.as_deref() != Some(selected.observation_identity()) {
            self.reset(Some(selected.observation_identity().into()));
        }
        if selected.execution_active(root)? {
            let turn = selected.running_turn()?;
            self.idle_since = None;
            if !self.announced {
                self.announced = true;
                self.turn = turn.as_ref().map(|record| record.turn_id.clone());
                if !self.preannounced {
                    updates.push(Update::Notice(running_notice_for(turn.as_ref())));
                }
            }
            if let Some(turn) = turn {
                if self.turn.as_deref() != Some(turn.turn_id.as_str()) {
                    // A new turn starts a new cursor set; an older turn's text
                    // must not be replayed into it.
                    self.turn = Some(turn.turn_id.clone());
                    self.segments.clear();
                }
                self.render(&selected, &turn.turn_id, workspace, &mut updates)?;
            }
            return Ok(PollResult {
                state: State::Running,
                updates,
                activity: self.activity.clone(),
            });
        }
        if !self.announced {
            self.activity = None;
            return Ok(PollResult {
                state: State::Idle,
                updates,
                activity: None,
            });
        }
        // Drain whatever the owner published between the last active poll and
        // its lease disappearing; those final lines are otherwise lost. The
        // stream is not finished yet: within the settle window the owner may
        // still publish the real tool result and the completed turn.
        if let Some(turn) = self.turn.clone() {
            self.render(&selected, &turn, workspace, &mut updates)?;
        }
        let turn = self
            .turn
            .as_deref()
            .map(|id| selected.observed_turn(id))
            .transpose()?
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
                return Ok(PollResult {
                    state: State::Running,
                    updates,
                    activity: self.activity.clone(),
                });
            }
            match selected.settle_observed_turn(root).ok().flatten() {
                Some(settled) => finished_notice(Some(&settled)),
                None => {
                    return Ok(PollResult {
                        state: State::Running,
                        updates,
                        activity: self.activity.clone(),
                    })
                }
            }
        } else {
            finished_notice(turn.as_ref())
        };
        // The turn is truly over. Only now close a partial line and account
        // for calls that never recorded a result; doing this inside the grace
        // period both preempts a result that is about to land and repeats the
        // placeholder on every poll while the owner is gone.
        self.finish_segments(&mut updates);
        self.announced = false;
        self.preannounced = false;
        self.turn = None;
        self.segments.clear();
        self.idle_since = None;
        self.activity = None;
        updates.push(Update::Finish);
        updates.push(Update::Notice(notice));
        Ok(PollResult {
            state: State::Finished,
            updates,
            activity: None,
        })
    }

    fn reset(&mut self, session: Option<String>) {
        self.announced = false;
        self.preannounced = false;
        self.session = session;
        self.turn = None;
        self.segments.clear();
        self.idle_since = None;
        self.activity = None;
    }

    fn render(
        &mut self,
        selected: &SessionRepository<'_>,
        turn_id: &str,
        workspace: Option<&Path>,
        updates: &mut Vec<Update>,
    ) -> Result<()> {
        let segments = selected.observed_segments(turn_id)?;
        let mut present = BTreeSet::new();
        let mut running = Vec::new();
        for (row, tools) in segments {
            present.insert(row.id.clone());
            let cursor = self.segments.entry(row.id.clone()).or_default();
            cursor.refresh_text(&row, updates);
            for (tool, number) in tools {
                cursor.refresh_tool(
                    &row.id,
                    tool,
                    number,
                    selected,
                    workspace,
                    updates,
                    &mut running,
                )?;
            }
        }
        // A recovery removes a segment; its cursor would only replay if the id
        // ever came back.
        self.segments.retain(|id, _| present.contains(id));
        self.activity = (!running.is_empty()).then(|| {
            let mut labels: Vec<String> = running.iter().take(3).cloned().collect();
            if running.len() > 3 {
                labels.push(format!("+{} more", running.len() - 3));
            }
            format!("Running {}", labels.join(" · "))
        });
        Ok(())
    }

    /// Closes every pending text line and submits unfinished calls as "No
    /// result recorded", exactly as a saved history renders them.
    fn finish_segments(&mut self, updates: &mut Vec<Update>) {
        for (segment, cursor) in self.segments.iter_mut() {
            cursor.flush_all(segment, updates);
            for (id, (tool, number)) in std::mem::take(&mut cursor.running) {
                cursor.tools.insert(id, true);
                updates.push(Update::Tool(ToolUpdate {
                    segment: segment.clone(),
                    number,
                    name: tool.name,
                    input: tool.input,
                    output: "No result recorded".into(),
                    is_error: false,
                    elapsed: None,
                }));
            }
        }
    }
}

impl Cursor {
    fn refresh_text(&mut self, row: &message_repo::MessageRow, updates: &mut Vec<Update>) {
        // The owner rewrites the row with the same accumulated prefix as the
        // turn grows, so only new bytes are public progress.
        if row.content.len() > self.text_seen && row.content.is_char_boundary(self.text_seen) {
            self.pending.push_str(&row.content[self.text_seen..]);
            self.text_seen = row.content.len();
        }
        self.flush_limited(&row.id, updates, 40);
    }

    fn refresh_tool(
        &mut self,
        segment: &str,
        tool: message_repo::ToolCallRow,
        number: usize,
        selected: &SessionRepository<'_>,
        workspace: Option<&Path>,
        updates: &mut Vec<Update>,
        running: &mut Vec<String>,
    ) -> Result<()> {
        if self.tools.get(&tool.id).copied() == Some(true) {
            self.running.remove(&tool.id);
            return Ok(());
        }
        match tool.output.as_deref() {
            Some(output) => {
                let elapsed = selected
                    .observed_tool_duration(&tool.id)?
                    .map(Duration::from_millis);
                updates.push(Update::Tool(ToolUpdate {
                    segment: segment.to_owned(),
                    number,
                    name: tool.name.clone(),
                    input: tool.input.clone(),
                    output: output.to_owned(),
                    is_error: tool.is_error,
                    elapsed,
                }));
                self.tools.insert(tool.id.clone(), true);
                self.running.remove(&tool.id);
            }
            None => {
                self.tools.entry(tool.id.clone()).or_insert(false);
                let label = crate::terminal::tool_view::label(&tool.name, &tool_input(&tool), workspace);
                running.push(label);
                self.running.insert(tool.id.clone(), (tool, number));
            }
        }
        Ok(())
    }

    /// Emits complete lines, then chunks of an unterminated one so long text
    /// still shows progress instead of being buffered indefinitely.
    fn flush_limited(&mut self, segment: &str, updates: &mut Vec<Update>, limit: usize) {
        let mut lines = Vec::new();
        let mut emitted = 0;
        while emitted < limit {
            let Some(index) = self.pending.find('\n') else {
                break;
            };
            let line: String = self.pending.drain(..=index).collect();
            let line = line.trim_end_matches(['\r', '\n']).to_owned();
            lines.push(line);
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
            lines.push(format!("{}…", chunk.trim_end()));
            emitted += 1;
        }
        if !lines.is_empty() {
            updates.push(Update::Text {
                segment: segment.to_owned(),
                lines,
            });
        }
    }

    fn flush_all(&mut self, segment: &str, updates: &mut Vec<Update>) {
        let mut lines = Vec::new();
        let remainder = self.pending.trim_end().to_owned();
        if !remainder.is_empty() {
            lines.push(remainder);
            self.pending.clear();
        }
        if !lines.is_empty() {
            updates.push(Update::Text {
                segment: segment.to_owned(),
                lines,
            });
        }
    }
}

fn tool_input(tool: &message_repo::ToolCallRow) -> serde_json::Value {
    tool.input
        .as_deref()
        .and_then(|input| serde_json::from_str(input).ok())
        .unwrap_or_default()
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

    use crate::fixture_sql::FixtureSql;
    use deepseek_code_core::db::session_store::Stores;

    fn fixture() -> (tempfile::TempDir, Arc<Stores>, String) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(Stores::open(temp.path()).unwrap());
        let store = deepseek_code_core::runtime::SessionStore::new(db.clone());
        let session = store.create(temp.path(), "model", "provider").unwrap();
        (temp, db, session)
    }

    fn write_text(db: &Arc<Stores>, session: &str, message: &str, content: &str) {
        db.shard_sql(session, |conn| message_repo::update_message_content(conn, message, content))
            .unwrap()
            .unwrap();
    }

    fn save_tool(
        db: &Arc<Stores>,
        session: &str,
        message: &str,
        id: &str,
        name: &str,
        input: &str,
        output: Option<&str>,
    ) {
        let content = db
            .shard_sql(session, |conn| message_repo::get_message(conn, message))
            .unwrap()
            .unwrap()
            .unwrap()
            .content;
        db.shard_sql(session, |conn| {
            message_repo::save_stream_tool_result(
                conn,
                session,
                message,
                &content,
                &message_repo::ToolCallRow {
                    id: format!("{message}:{id}"),
                    message_id: message.to_owned(),
                    name: name.into(),
                    input: Some(input.into()),
                    output: output.map(str::to_owned),
                    is_error: false,
                    sort_order: 0,
                },
            )
        })
        .unwrap()
        .unwrap();
    }

    fn text_lines(updates: &[Update]) -> Vec<String> {
        updates
            .iter()
            .flat_map(|update| match update {
                Update::Text { lines, .. } => lines.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    fn notices(updates: &[Update]) -> Vec<&str> {
        updates
            .iter()
            .filter_map(|update| match update {
                Update::Notice(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn tools(updates: &[Update]) -> Vec<&ToolUpdate> {
        updates
            .iter()
            .filter_map(|update| match update {
                Update::Tool(tool) => Some(tool),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn published_progress_is_rendered_once_and_completion_reports_the_measured_turn() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&session_id).unwrap()).unwrap();
        let message = timer.ensure_segment().unwrap();
        write_text(&db, &session_id, &message, "first line\nsecond");
        save_tool(&db, &session_id, &message, "read-1", "Read", "{\"file_path\":\"fixture.txt\"}", None);
        // The owner's lease: the observer only probes it.
        let lease = db.open_session(&session_id).unwrap().acquire_execution().unwrap();
        let mut observer = Observer::default();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Running);
        assert!(notices(&result.updates)[0].contains("still running in another process"));
        let text = text_lines(&result.updates);
        assert!(text.iter().any(|line| line.contains("first line")));
        assert!(
            !text.iter().any(|line| line.contains("second")),
            "partial lines wait"
        );
        assert!(
            tools(&result.updates).is_empty(),
            "a running call is not written as an orphan header: {:?}",
            result.updates
        );
        assert!(
            result.activity.as_deref().is_some_and(|activity| activity.contains("Read(fixture.txt)")),
            "{:?}",
            result.activity
        );

        // Growing text and a finished tool appear on the next poll only.
        write_text(&db, &session_id, &message, "first line\nsecond line\n");
        save_tool(
            &db,
            &session_id,
            &message,
            "read-1",
            "Read",
            "{\"file_path\":\"fixture.txt\"}",
            Some("fixture contents\n"),
        );
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Running);
        let text = text_lines(&result.updates);
        assert!(text.iter().any(|line| line.contains("second line")));
        assert!(
            !text.iter().any(|line| line.contains("first line")),
            "already shown"
        );
        let tools = tools(&result.updates);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "Read");
        assert_eq!(tools[0].number, 1);
        assert_eq!(tools[0].output, "fixture contents\n");
        assert_eq!(tools[0].elapsed, None);
        assert!(result.activity.is_none(), "the call finished");

        // Idle polls repeat nothing.
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Running);
        assert!(result.updates.is_empty(), "{:?}", result.updates);

        // The owner exits: the same poll reports the measured completion once.
        drop(lease);
        timer.finish(turn_repo::TurnStatus::Completed).unwrap();
        let turn_id = timer.record().turn_id.clone();
        db.shard_sql(&session_id, |conn| {
            conn.execute(
                "UPDATE execution_turns SET elapsed_ms=128000 WHERE id=?1",
                [&turn_id],
            )
        })
        .unwrap()
        .unwrap();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Finished);
        assert!(result.updates.contains(&Update::Finish));
        assert!(notices(&result.updates)
            .iter()
            .any(|line| line.contains("Observed turn finished")));
        assert!(notices(&result.updates)[0].contains("2m 8s"));
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Idle);
        assert!(result.updates.is_empty(), "a finished observation is not repeated");
    }

    #[test]
    fn a_caller_banner_suppresses_the_first_running_notice() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&session_id).unwrap()).unwrap();
        let message = timer.ensure_segment().unwrap();
        write_text(&db, &session_id, &message, "visible progress\n");
        let mut observer = Observer::default();
        // The REPL printed observation_banner before the owner's lease was
        // visible. It must not settle that empty view as a finished turn.
        observer.preannounce_at(&db, &session_id).unwrap();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Idle);
        assert!(result.updates.is_empty(), "{:?}", result.updates);

        let lease = db.open_session(&session_id).unwrap().acquire_execution().unwrap();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Running);
        assert!(
            !notices(&result.updates)
                .iter()
                .any(|line| line.contains("still running in another process")),
            "the caller's banner was repeated: {:?}",
            result.updates
        );
        assert!(text_lines(&result.updates)
            .iter()
            .any(|line| line.contains("visible progress")));
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert!(result.updates.is_empty(), "{:?}", result.updates);

        // Completion still reports the measured turn exactly once.
        drop(lease);
        timer.finish(turn_repo::TurnStatus::Completed).unwrap();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Finished);
        assert!(notices(&result.updates)
            .iter()
            .any(|line| line.contains("Observed turn finished")));
        assert!(
            !notices(&result.updates)
                .iter()
                .any(|line| line.contains("still running in another process")),
            "{:?}",
            result.updates
        );
    }

    #[test]
    fn multiple_segments_keep_their_own_cursors_and_never_replay() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&session_id).unwrap()).unwrap();
        let first = timer.ensure_segment().unwrap();
        write_text(&db, &session_id, &first, "first segment\n");
        save_tool(&db, &session_id, &first, "read-1", "Read", "{\"file_path\":\"one.txt\"}", Some("one\n"));
        // A recovery replaced the answer with a second segment of the same turn.
        let second = timer.new_segment().unwrap();
        write_text(&db, &session_id, &second, "second segment\n");
        save_tool(&db, &session_id, &second, "read-2", "Read", "{\"file_path\":\"two.txt\"}", None);
        let lease = db.open_session(&session_id).unwrap().acquire_execution().unwrap();

        let mut observer = Observer::default();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert!(text_lines(&result.updates)
            .iter()
            .any(|line| line.contains("first segment")));
        assert!(text_lines(&result.updates)
            .iter()
            .any(|line| line.contains("second segment")));
        assert_eq!(tools(&result.updates).len(), 1);
        assert_eq!(tools(&result.updates)[0].number, 1);
        assert!(result
            .activity
            .as_deref()
            .is_some_and(|activity| activity.contains("Read(two.txt)")));

        // Repeated idle polls must not replay either segment.
        for _ in 0..3 {
            let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
            assert_eq!(result.state, State::Running);
            assert!(result.updates.is_empty(), "replayed: {:?}", result.updates);
        }

        // New progress in the current segment appears exactly once.
        write_text(&db, &session_id, &second, "second segment\nthird line\n");
        save_tool(&db, &session_id, &second, "read-2", "Read", "{\"file_path\":\"two.txt\"}", Some("two\n"));
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(
            text_lines(&result.updates)
                .iter()
                .filter(|line| line.contains("third line"))
                .count(),
            1,
            "{:?}",
            result.updates
        );
        let tools = tools(&result.updates);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].number, 2, "numbering follows the saved conversation");
        assert_eq!(tools[0].output, "two\n");
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert!(result.updates.is_empty(), "{:?}", result.updates);
        drop(lease);
    }

    #[test]
    fn completion_drains_the_final_text_and_tool_result_written_after_the_last_live_poll() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&session_id).unwrap()).unwrap();
        let message = timer.ensure_segment().unwrap();
        write_text(&db, &session_id, &message, "started\n");
        let lease = db.open_session(&session_id).unwrap().acquire_execution().unwrap();
        let mut observer = Observer::default();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Running);
        assert!(text_lines(&result.updates)
            .iter()
            .any(|line| line.contains("started")));

        // Everything below is written after the last poll that saw a live owner.
        write_text(&db, &session_id, &message, "started\nfinal answer\n");
        save_tool(
            &db,
            &session_id,
            &message,
            "read-1",
            "Read",
            "{\"file_path\":\"fixture.txt\"}",
            Some("fixture contents\n"),
        );
        drop(lease);
        timer.finish(turn_repo::TurnStatus::Completed).unwrap();
        let turn_id = timer.record().turn_id.clone();
        db.shard_sql(&session_id, |conn| {
            conn.execute(
                "UPDATE execution_turns SET elapsed_ms=42000 WHERE id=?1",
                [&turn_id],
            )
        })
        .unwrap()
        .unwrap();

        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Finished);
        assert!(
            text_lines(&result.updates)
                .iter()
                .any(|line| line.contains("final answer")),
            "final text was dropped: {:?}",
            result.updates
        );
        assert_eq!(tools(&result.updates).len(), 1);
        assert_eq!(tools(&result.updates)[0].output, "fixture contents\n");
        assert!(notices(&result.updates).iter().any(|line| line.contains("42s")));
    }

    #[test]
    fn unfinished_calls_are_committed_with_no_result_recorded_at_turn_end() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&session_id).unwrap()).unwrap();
        let message = timer.ensure_segment().unwrap();
        save_tool(&db, &session_id, &message, "read-1", "Read", "{\"file_path\":\"fixture.txt\"}", None);
        let lease = db.open_session(&session_id).unwrap().acquire_execution().unwrap();
        let mut observer = Observer::default();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert!(tools(&result.updates).is_empty());
        assert!(result.activity.is_some());

        drop(lease);
        std::mem::forget(timer);
        observer.idle_since = Some(Instant::now() - SETTLE_GRACE - Duration::from_millis(1));
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Finished);
        let tools = tools(&result.updates);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].output, "No result recorded");
        assert_eq!(tools[0].number, 1);
    }

    #[test]
    fn a_dead_owner_observed_live_becomes_interrupted_without_replay() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&session_id).unwrap()).unwrap();
        let message = timer.ensure_segment().unwrap();
        write_text(&db, &session_id, &message, "partial answer\n");
        let lease = db.open_session(&session_id).unwrap().acquire_execution().unwrap();
        let mut observer = Observer::default();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Running);

        // The process is killed: no endpoint is ever written, the lease is gone.
        drop(lease);
        std::mem::forget(timer);
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(
            result.state,
            State::Running,
            "the finalization window is respected"
        );
        assert!(result.updates.is_empty(), "{:?}", result.updates);

        // After the grace period the turn is settled as interrupted, not complete.
        observer.idle_since = Some(Instant::now() - SETTLE_GRACE - Duration::from_millis(1));
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Finished);
        assert!(
            notices(&result.updates)
                .iter()
                .any(|line| line.contains("interrupted · duration unknown")),
            "{:?}",
            result.updates
        );
        assert!(
            !notices(&result.updates).iter().any(|line| line.contains("finished")),
            "a dead owner is not reported as completed: {:?}",
            result.updates
        );
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Idle, "never stays running");
        assert!(result.updates.is_empty(), "never replays: {:?}", result.updates);
        let record = db
            .shard_sql(&session_id, |conn| turn_repo::latest_turn(conn, &session_id))
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(record.interrupted());
        assert_eq!(record.elapsed_ms, None, "duration stays unknown");
    }

    #[test]
    fn the_settle_grace_never_preempts_a_result_that_lands_before_the_turn_closes() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&session_id).unwrap()).unwrap();
        let message = timer.ensure_segment().unwrap();
        save_tool(&db, &session_id, &message, "read-1", "Read", "{\"file_path\":\"fixture.txt\"}", None);
        let lease = db.open_session(&session_id).unwrap().acquire_execution().unwrap();
        let mut observer = Observer::default();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Running);
        assert!(tools(&result.updates).is_empty());
        assert!(result.activity.is_some());

        // The owner released the lease but has not finished writing the turn:
        // the call stays running instead of being settled as an orphan.
        drop(lease);
        for _ in 0..3 {
            let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
            assert_eq!(result.state, State::Running, "the grace period is respected");
            assert!(
                tools(&result.updates).is_empty(),
                "an unfinished call was preempted: {:?}",
                result.updates
            );
        }

        // The finalization publishes the real result and completes the turn.
        save_tool(
            &db,
            &session_id,
            &message,
            "read-1",
            "Read",
            "{\"file_path\":\"fixture.txt\"}",
            Some("fixture contents\n"),
        );
        timer.finish(turn_repo::TurnStatus::Completed).unwrap();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Finished);
        let tools = tools(&result.updates);
        assert_eq!(tools.len(), 1, "the result must be published exactly once: {:?}", result.updates);
        assert_eq!(tools[0].output, "fixture contents\n");
        assert!(
            !result.updates.iter().any(|update| matches!(
                update,
                Update::Tool(tool) if tool.output == "No result recorded"
            )),
            "an orphan placeholder contradicted the real result: {:?}",
            result.updates
        );
    }

    #[test]
    fn an_interrupted_turn_records_an_unfinished_call_exactly_once() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&session_id).unwrap()).unwrap();
        let message = timer.ensure_segment().unwrap();
        save_tool(&db, &session_id, &message, "read-1", "Read", "{\"file_path\":\"fixture.txt\"}", None);
        let lease = db.open_session(&session_id).unwrap().acquire_execution().unwrap();
        let mut observer = Observer::default();
        observer.poll_at(root, &db, &session_id, Some(root)).unwrap();

        // The process is killed without settling its turn or its call.
        drop(lease);
        std::mem::forget(timer);
        for _ in 0..3 {
            let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
            assert_eq!(result.state, State::Running);
            assert!(
                tools(&result.updates).is_empty(),
                "the placeholder repeated inside the grace period: {:?}",
                result.updates
            );
        }

        observer.idle_since = Some(Instant::now() - SETTLE_GRACE - Duration::from_millis(1));
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Finished);
        let tools = tools(&result.updates);
        assert_eq!(tools.len(), 1, "the orphan is reported exactly once: {:?}", result.updates);
        assert_eq!(tools[0].output, "No result recorded");
        assert!(notices(&result.updates)
            .iter()
            .any(|line| line.contains("interrupted · duration unknown")));

        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Idle);
        assert!(result.updates.is_empty(), "never replays: {:?}", result.updates);
    }

    #[test]
    fn a_live_turn_is_never_settled_while_its_lease_is_held() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&session_id).unwrap()).unwrap();
        let message = timer.ensure_segment().unwrap();
        write_text(&db, &session_id, &message, "working\n");
        let lease = db.open_session(&session_id).unwrap().acquire_execution().unwrap();
        let mut observer = Observer::default();
        observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        observer.idle_since = Some(Instant::now() - SETTLE_GRACE - Duration::from_millis(1));
        // The owner is alive: the probe must not settle its turn.
        assert!(
            db.select_session(&session_id).unwrap().execution_active(root).unwrap(),
            "the owner holds the lease"
        );
        drop(lease);
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert_eq!(result.state, State::Finished);
        assert!(notices(&result.updates)
            .iter()
            .any(|line| line.contains("interrupted · duration unknown")));
        drop(timer);
    }

    #[test]
    fn a_long_unterminated_line_still_shows_progress() {
        let (temp, db, session_id) = fixture();
        let root = temp.path();
        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&session_id).unwrap()).unwrap();
        let message = timer.ensure_segment().unwrap();
        let long = "x".repeat(LONG_LINE + 50);
        write_text(&db, &session_id, &message, &long);
        let _lease = db.open_session(&session_id).unwrap().acquire_execution().unwrap();
        let mut observer = Observer::default();
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert!(
            text_lines(&result.updates)
                .iter()
                .any(|line| line.starts_with("xxxx") && line.ends_with('…')),
            "long text is buffered forever: {:?}",
            result.updates
        );
        write_text(&db, &session_id, &message, &format!("{long}\nsecond"));
        let result = observer.poll_at(root, &db, &session_id, Some(root)).unwrap();
        assert!(
            text_lines(&result.updates)
                .iter()
                .any(|line| line.contains("xxxxxxxx")),
            "{:?}",
            result.updates
        );
        assert!(
            !text_lines(&result.updates)
                .iter()
                .any(|line| line.contains("second")),
            "partial line still waits: {:?}",
            result.updates
        );
    }

    #[test]
    fn switching_to_an_idle_session_does_not_replay_the_observed_turn() {
        let (temp, db, first) = fixture();
        let root = temp.path();
        let store = deepseek_code_core::runtime::SessionStore::new(db.clone());
        let second = store.create(temp.path(), "model", "provider").unwrap();
        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&first).unwrap()).unwrap();
        let message = timer.ensure_segment().unwrap();
        write_text(&db, &first, &message, "first session text\n");
        let lease = db.open_session(&first).unwrap().acquire_execution().unwrap();
        let mut observer = Observer::default();
        let result = observer.poll_at(root, &db, &first, Some(root)).unwrap();
        assert!(text_lines(&result.updates)
            .iter()
            .any(|line| line.contains("first session text")));

        // /resume to an idle session shows nothing from the observed one.
        let result = observer.poll_at(root, &db, &second, Some(root)).unwrap();
        assert_eq!(result.state, State::Idle);
        assert!(result.updates.is_empty(), "{:?}", result.updates);

        // Back to the first session: a fresh view re-announces it once.
        let result = observer.poll_at(root, &db, &first, Some(root)).unwrap();
        assert_eq!(result.state, State::Running);
        assert_eq!(
            text_lines(&result.updates)
                .iter()
                .filter(|line| line.contains("first session text"))
                .count(),
            1
        );
        let result = observer.poll_at(root, &db, &first, Some(root)).unwrap();
        assert!(result.updates.is_empty(), "{:?}", result.updates);
        drop(lease);
        drop(timer);
    }

    #[test]
    fn an_idle_observer_stays_silent_and_a_settled_turn_is_interrupted() {
        let (temp, db, session_id) = fixture();
        let mut observer = Observer::default();
        let result = observer.poll_at(temp.path(), &db, &session_id, Some(temp.path())).unwrap();
        assert_eq!(result.state, State::Idle);
        assert!(result.updates.is_empty());

        let mut timer = turn_repo::TurnTimer::in_session(db.open_session(&session_id).unwrap()).unwrap();
        let message = timer.ensure_segment().unwrap();
        // Process loss: no Drop, no duration, no live owner.
        db.shard_sql(&session_id, |conn| {
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
            .shard_sql(&session_id, |conn| turn_repo::get_message_turn(conn, &message))
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(super::finished_notice(Some(&turn)).contains("interrupted"));
    }
}
