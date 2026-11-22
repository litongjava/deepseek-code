use super::*;

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
    for event in receive.try_iter() {
        match event {
            Update::Thinking(text) => {
                let now = Instant::now();
                state.thinking_span = Some((now - Duration::from_secs(8), now));
                state.thinking_preview = text;
            }
            Update::ToolStarted(id, header, shell) => {
                finish_text(&mut state, &mut frame);
                state.running.push(Running {
                    id,
                    header,
                    shell,
                    started: Instant::now(),
                });
            }
            Update::Activity(activity) => state.activity = activity,
            _ => (),
        }
    }
    for seconds in [2, 3] {
        for call in &mut state.running {
            call.started = Instant::now() - Duration::from_secs(seconds);
        }
        frame.draw_at(&mut state, size).unwrap();
        let mut rows = frame.scrollback.all();
        rows.extend(frame.lines.iter().cloned());
        let rows: Vec<_> = rows
            .iter()
            .map(|line| super::super::markdown::plain_rendered(line))
            .collect();
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
        let owners: Vec<_> = rows
            .windows(2)
            .filter(|pair| pair[1].contains("⎿  Running"))
            .map(|pair| pair[0].as_str())
            .collect();
        assert_eq!(owners.len(), count);
        for (i, header) in owners.iter().enumerate() {
            assert!(
                header.contains(&format!("● #{} ", i + 1)),
                "detached status: {owners:?}"
            );
        }
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
