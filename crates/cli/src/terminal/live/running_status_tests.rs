use super::*;

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
