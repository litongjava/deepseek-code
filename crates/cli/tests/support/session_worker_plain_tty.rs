//! Worker-owned turns driven through the three plain terminal presentations.
use super::*;

pub(super) fn provider_reply(body: &Value) -> Option<axum::response::Response> {
    use axum::response::IntoResponse;
    let last = body["messages"].as_array()?.last()?;
    if body["stream"] != true || last["role"] != "user" {
        return None;
    }
    let prompt = prompt_text(last["content"].as_str()?);
    if prompt == "request provider failure" {
        return Some(
            (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(json!({"error":{"message":"fixture authentication error"}})),
            )
                .into_response(),
        );
    }
    if prompt != "request gated write" {
        return None;
    }
    let call = json!({"index":0,"id":"gated-write","type":"function","function":{
        "name":"Write", "arguments":json!({"file_path":"gated.txt","content":"approved"}).to_string()
    }});
    let delta = json!({"choices":[{"index":0,"delta":{"tool_calls":[call]},"finish_reason":null}]});
    let end = json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
    Some(
        (
            [("content-type", "text/event-stream")],
            format!("data: {delta}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
        )
            .into_response(),
    )
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Focus,
    Reader,
    Dumb,
}

const MODES: [Mode; 3] = [Mode::Focus, Mode::Reader, Mode::Dumb];

impl Mode {
    fn line_editor(self) -> bool {
        !matches!(self, Self::Focus)
    }

    fn prompt(self) -> &'static [u8] {
        if self.line_editor() {
            b"> "
        } else {
            PROMPT
        }
    }

    async fn terminal(self, fixture: &OwnerFixture) -> Terminal {
        let mut command = Terminal::command_with_permission(
            portable_pty::CommandBuilder::new(test_cli_executable()),
            &fixture.temp.path().join("work"),
            &fixture.paths.data_dir,
            &fixture.temp.path().join("coord"),
            true,
            matches!(self, Self::Reader),
            "ask",
        );
        if matches!(self, Self::Dumb) {
            command.env("TERM", "dumb");
        }
        command.args(["--resume", &fixture.session]);
        let mut tty = Terminal::spawn(command);
        tty.wait_output(self.prompt(), true).await;
        if matches!(self, Self::Focus) {
            repl(&mut tty, "/focus on", b"focus updated").await;
        }
        tty
    }

    async fn wait_editor(self, tty: &mut Terminal, after: usize) {
        if !self.line_editor() {
            tty.wait_output_since(after, b"\x1b[?2004h", true).await;
        }
        tty.wait_output_since(after, self.prompt(), true).await;
    }

    async fn successful_turn(self, tty: &mut Terminal, prompt: &str) {
        tty.submit(prompt, self.line_editor()).await;
        tty.wait_output(b"worker result", true).await;
        let end = tty
            .transcript
            .windows(b"worker result".len())
            .position(|s| s == b"worker result")
            .unwrap()
            + b"worker result".len();
        self.wait_editor(tty, end).await;
    }
}

#[tokio::test]
async fn interrupt_at_plain_permission_prompt_cancels_the_worker() {
    for mode in MODES {
        let fixture = OwnerFixture::new().await;
        let mut tty = mode.terminal(&fixture).await;
        tty.submit("request gated write", mode.line_editor()).await;
        tty.wait_output(b"/n no)", true).await;
        let after = tty.transcript.len();
        tty.send(b"\x03");
        mode.wait_editor(&mut tty, after).await;
        let jobs = jobs::list(&fixture.paths.data_dir, Some(&fixture.session)).unwrap();
        assert_eq!(jobs.len(), 1, "{mode:?}");
        let job = &jobs[0];
        assert_eq!(job.status, "cancelled", "{mode:?}");
        assert!(!fixture
            .paths
            .layout()
            .background_tasks()
            .join(&job.id)
            .join("pending-question.json")
            .exists());
        assert!(!fixture.temp.path().join("work/gated.txt").exists());
        mode.successful_turn(&mut tty, "after interrupted approval")
            .await;
        assert!(fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["stream"] == true
                && r["messages"].as_array().unwrap().last().unwrap()["content"]
                    .to_string()
                    .contains("after interrupted approval")));
        tty.submit("/exit", mode.line_editor()).await;
        tty.wait_success().await;
        eprintln!(
            "{mode:?}: cancelled {}, question cleared, following turn reached provider",
            job.id
        );
    }
}

#[tokio::test]
async fn failed_request_prints_one_diagnostic_in_plain_modes() {
    for mode in MODES {
        let fixture = OwnerFixture::new().await;
        let mut tty = mode.terminal(&fixture).await;
        tty.submit("request provider failure", mode.line_editor())
            .await;
        tty.wait_output(b"HTTP 401", true).await;
        let text = String::from_utf8_lossy(&tty.transcript);
        let after = text.find("HTTP 401").unwrap() + "HTTP 401".len();
        mode.wait_editor(&mut tty, after).await;
        assert_eq!(
            String::from_utf8_lossy(&tty.transcript)
                .matches("HTTP 401")
                .count(),
            1,
            "{mode:?}"
        );
        let jobs = jobs::list(&fixture.paths.data_dir, Some(&fixture.session)).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, "failed");
        mode.successful_turn(&mut tty, "follow up after failure")
            .await;
        tty.submit("/exit", mode.line_editor()).await;
        tty.wait_success().await;
        eprintln!("{mode:?}: one HTTP 401 diagnostic, one failed job and a successful follow-up");
    }
}
