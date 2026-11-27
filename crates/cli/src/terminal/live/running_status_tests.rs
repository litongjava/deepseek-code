use super::*;
use crossterm::event::KeyEvent;

fn dispatch(receive: &mpsc::Receiver<Update>, frame: &mut Frame, state: &mut State) {
    for event in receive.try_iter() {
        match event {
            Update::Thinking(text) => {
                let now = Instant::now();
                state.thinking_span = Some((now - Duration::from_secs(8), now));
                state.thinking_preview = text;
            }
            Update::ToolStarted(id, header, shell) => frame.start_call(state, id, header, shell),
            Update::Finished(id, name, failed, short, full) => {
                frame
                    .finish_call(state, &id, &name, failed, short, full)
                    .unwrap();
            }
            Update::Activity(activity) => state.activity = activity,
            _ => (),
        }
    }
}

fn rendered(frame: &Frame) -> Vec<String> {
    frame
        .scrollback
        .all()
        .iter()
        .chain(&frame.lines)
        .map(|line| super::super::markdown::plain_rendered(line))
        .collect()
}

fn assert_owners(rows: &[String], numbers: std::ops::RangeInclusive<usize>) {
    let mut owners = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains("⎿  Running"));
    for number in numbers {
        let (i, _) = owners.next().expect("missing running status");
        assert!(
            i > 0 && rows[i - 1].contains(&format!("● #{number} ")),
            "detached status: {rows:?}"
        );
    }
    assert!(
        owners.next().is_none(),
        "unexpected running status: {rows:?}"
    );
}

// A local scripted turn at the model/display boundary. No shell, network,
// wall-clock sleeps, or terminal queries are required to replay these frames.
fn mock_turn(count: usize, height: u16) {
    let tmp = tempfile::tempdir().unwrap();
    let mut display = TurnDisplay::new(false, "ask", &tmp.path().join("keys"), vec![]).unwrap();
    let (updates, receive) = mpsc::channel();
    let (_, actions) = async_mpsc::unbounded_channel();
    display.live = Some(LiveTurn {
        updates,
        actions,
        worker: None,
    });
    display.record_thinking(&["Checking"; 11].join("\n"));
    let calls = [
        ("Read", serde_json::json!({"file_path":"fixture"})),
        (
            "WebFetch",
            serde_json::json!({"url":"https://example.invalid"}),
        ),
        ("Bash", serde_json::json!({"command":"echo fixture"})),
    ];
    for (i, (name, input)) in calls[3 - count..].iter().enumerate() {
        display.tool_started(&i.to_string(), name, input);
    }
    let size = (80, height);
    let mut frame = Frame {
        size,
        top: Some(0),
        aligned: true,
        ..Frame::default()
    };
    let mut state = State::new("ask".into());
    dispatch(&receive, &mut frame, &mut state);
    for seconds in [2, 3] {
        for call in &mut state.running {
            call.started = Instant::now() - Duration::from_secs(seconds);
        }
        frame.draw_at(&mut state, size).unwrap();
        let rows = rendered(&frame);
        let end = rows
            .iter()
            .position(|line| line.contains("Ctrl+B to run"))
            .map(|i| i + 1)
            .unwrap_or_else(|| {
                rows.iter()
                    .rposition(|line| line.contains("⎿  Running"))
                    .unwrap()
                    + 1
            });
        println!(
            "\n{count} call(s), 80x{height}, {seconds}s:\n{}",
            rows[..end].join("\n")
        );
        assert!(rows[0].contains("Thought for 8s"));
        assert_owners(&rows, 1..=count);
    }
    // The composer leaves room for just the last call on a short screen.
    frame.draw_at(&mut state, (80, 12)).unwrap();
    assert_owners(&rendered(&frame), count..=count);

    // Complete calls in reverse start order. The result replaces that ID's
    // live rows; other numbered calls must keep their own status.
    for (i, (name, input)) in calls[3 - count..].iter().enumerate().rev() {
        display.tool_finished(&i.to_string(), name, input, "fixture result", false);
        dispatch(&receive, &mut frame, &mut state);
        frame.draw_at(&mut state, size).unwrap();
        let rows = rendered(&frame);
        assert_owners(&rows, 1..=i);
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains(&format!("● #{} ", i + 1)))
                .count(),
            1
        );
    }
}

#[test]
fn single_call_after_thought() {
    mock_turn(1, 24);
}

#[test]
fn two_parallel_calls_after_thought() {
    mock_turn(2, 15);
}

#[test]
fn three_parallel_calls_after_thought() {
    mock_turn(3, 24);
}

struct CompactReset;

impl Drop for CompactReset {
    fn drop(&mut self) {
        crate::terminal::scrollback::set_compact(None);
    }
}

/// A compact live turn. `initial` calls start before the first compact frame;
/// the rest can start later, as calls do in a real turn.
struct CompactTurn {
    _reset: CompactReset,
    _tmp: tempfile::TempDir,
    display: TurnDisplay,
    receive: mpsc::Receiver<Update>,
    calls: Vec<(&'static str, serde_json::Value)>,
    frame: Frame,
    state: State,
    size: (u16, u16),
}

impl CompactTurn {
    fn new(initial: usize, count: usize, height: u16) -> Self {
        let reset = CompactReset;
        crate::terminal::scrollback::set_compact(Some(true));
        let tmp = tempfile::tempdir().unwrap();
        let mut display =
            TurnDisplay::new(false, "ask", &tmp.path().join("keys"), vec![]).unwrap();
        let (updates, receive) = mpsc::channel();
        let (_, actions) = async_mpsc::unbounded_channel();
        display.live = Some(LiveTurn {
            updates,
            actions,
            worker: None,
        });
        display.record_thinking(&["Checking"; 11].join("\n"));
        let calls = [
            ("Read", serde_json::json!({"file_path":"fixture"})),
            (
                "WebFetch",
                serde_json::json!({"url":"https://example.invalid"}),
            ),
            ("Bash", serde_json::json!({"command":"echo fixture"})),
        ];
        let calls = calls[3 - count..].to_vec();
        let size: (u16, u16) = (80, height);
        // The live turn starts from `Frame::default()`, whose size is still
        // (0, 0); only the first `draw_at` knows the real terminal size.
        let mut frame = Frame {
            top: Some(0),
            aligned: true,
            ..Frame::default()
        };
        assert_eq!(
            frame.size,
            (0, 0),
            "the turn starts before any frame sets its size"
        );
        frame.pinned.text = "Inspect the fixture".into();
        // The turn opens below the submitted prompt, as it does in the editor.
        let width = size.0.saturating_sub(1) as usize;
        frame
            .scrollback
            .record_prompt_at_width("Inspect the fixture", width);
        let mut state = State::new("ask".into());
        state.activity = "Running Bash(cat fixture)".into();
        let mut turn = Self {
            _reset: reset,
            _tmp: tmp,
            display,
            receive,
            calls,
            frame,
            state,
            size,
        };
        turn.start(0, initial);
        turn.draw();
        turn
    }

    fn start(&mut self, from: usize, to: usize) {
        for (index, (name, input)) in self.calls.iter().enumerate().take(to).skip(from) {
            self.display.tool_started(&index.to_string(), name, input);
        }
        dispatch(&self.receive, &mut self.frame, &mut self.state);
    }

    fn draw(&mut self) {
        for call in &mut self.state.running {
            call.started = Instant::now() - Duration::from_secs(3);
        }
        self.frame.draw_at(&mut self.state, self.size).unwrap();
    }

    fn page(&self) -> usize {
        (self.size.1 as usize).saturating_sub(self.frame.composer_height)
    }

    fn visible_rows(&self) -> Vec<String> {
        self.frame
            .history_view
            .screen_rows(self.size.1 as usize, &self.frame.lines)
            .iter()
            .map(|row| super::super::markdown::plain_rendered(row))
            .collect()
    }
}

fn compact_visible_rows(count: usize, height: u16) -> Vec<String> {
    CompactTurn::new(count, count, height).visible_rows()
}

fn assert_visible_pairs(rows: &[String], count: usize) {
    let headers: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains("\u{25cf} #"))
        .map(|(index, _)| index)
        .collect();
    let statuses: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains("\u{23bf}  Running"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(headers.len(), count, "missing call headers: {rows:#?}");
    assert_eq!(statuses.len(), count, "missing running statuses: {rows:#?}");
    for status in statuses {
        assert!(
            status > 0 && rows[status - 1].contains("\u{25cf} #"),
            "detached status: {rows:#?}"
        );
    }
    for header in headers {
        assert!(
            header + 1 < rows.len() && rows[header + 1].contains("\u{23bf}  Running"),
            "header without status: {rows:#?}"
        );
    }
    assert_eq!(rows[0], "\u{276f} Inspect the fixture");
}

#[test]
fn compact_tail_keeps_two_running_statuses_with_headers_at_80x14() {
    assert_visible_pairs(&compact_visible_rows(2, 14), 2);
}

#[test]
fn compact_tail_keeps_three_running_statuses_with_headers_at_80x17() {
    assert_visible_pairs(&compact_visible_rows(3, 17), 3);
}

#[test]
fn compact_turn_start_keeps_three_running_statuses_with_headers_at_80x24() {
    assert_visible_pairs(&compact_visible_rows(3, 24), 3);
}

#[test]
fn compact_view_follows_calls_started_after_the_first_frame() {
    // The first compact frame predates some or all of the calls; the automatic
    // tail must move down as they start, without any keypress.
    for (initial, count, height) in [(0, 3, 24), (1, 2, 14), (1, 3, 17)] {
        let mut turn = CompactTurn::new(initial, count, height);
        turn.start(initial, count);
        turn.draw();
        assert_visible_pairs(&turn.visible_rows(), count);
    }
}

#[test]
fn manual_scroll_stops_the_compact_follow_until_ctrl_end() {
    let mut turn = CompactTurn::new(0, 3, 17);
    turn.start(0, 2);
    turn.draw();
    let tail = turn.frame.history_view.top();
    turn.frame
        .history_view
        .navigate(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            turn.page(),
        )
        .unwrap();
    let scrolled = turn.frame.history_view.top();
    assert!(
        scrolled < tail,
        "PageUp moved the viewport: {scrolled:?} vs {tail:?}"
    );
    turn.start(2, 3);
    turn.draw();
    assert_eq!(
        turn.frame.history_view.top(),
        scrolled,
        "new calls must not move a manually scrolled view"
    );
    let visible = turn.visible_rows();
    assert!(
        !visible.iter().any(|row| row.contains("\u{25cf} #3")),
        "the newest call stays below a manual scroll: {visible:#?}"
    );
    turn.frame
        .history_view
        .navigate(
            KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL),
            turn.page(),
        )
        .unwrap();
    turn.draw();
    assert!(
        turn.frame.history_view.top() > scrolled,
        "Ctrl+End returns to the tail"
    );
    assert_visible_pairs(&turn.visible_rows(), 3);
}
