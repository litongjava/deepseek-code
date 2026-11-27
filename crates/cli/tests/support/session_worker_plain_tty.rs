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

#[tokio::test]
async fn frontend_session_start_delivers_once_and_rewakes_the_idle_worker() {
    const ORDINARY: &str = "frontend-ordinary-marker";
    const REWAKE: &str = "frontend-rewake-marker";
    for mode in MODES {
        let fixture = OwnerFixture::new().await;
        let work = fixture.temp.path().join("work");
        fs::write(
            work.join("ordinary.sh"),
            format!("echo '{{\"systemMessage\":\"{ORDINARY}\"}}'"),
        )
        .unwrap();
        fs::write(
            work.join("rewake.sh"),
            format!("while [ ! -e release ]; do sleep 0.05; done\necho {REWAKE} >&2\nexit 2"),
        )
        .unwrap();
        deepseek_code_core::data_layout::write_config_file(fixture.paths.layout().config_file("settings.json"),
            json!({"hooks":{"SessionStart":[{"hooks":[
                {"type":"command","command":"sh ordinary.sh","async":true,"once":true},
                {"type":"command","command":"sh rewake.sh","asyncRewake":true,"once":true,"timeout":45}
            ]}]}}).to_string()).unwrap();
        let mut tty = mode.terminal(&fixture).await;
        // Observe the actual result next to an owner lease still held by the
        // frontend process, before the worker receives its first chat request.
        let root = fixture.paths.data_dir.join("state/hook-async");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut owners = 0;
                let mut published = false;
                for entry in fs::read_dir(format!("/proc/{}/fd", tty.pid()))
                    .unwrap()
                    .flatten()
                {
                    let Ok(path) = fs::read_link(entry.path()) else {
                        continue;
                    };
                    if path.starts_with(&root)
                        && path.file_name().is_some_and(|name| name == "owner")
                    {
                        owners += 1;
                        published |= fs::read_to_string(path.with_file_name("result"))
                            .is_ok_and(|s| s.contains(ORDINARY));
                    }
                }
                if owners == 2 && published {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("frontend hook leases and published ordinary result");
        mode.successful_turn(&mut tty, "first explicit turn").await;
        mode.successful_turn(&mut tty, "second explicit turn").await;
        fs::write(work.join("release"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                tty.pump(true);
                let jobs = jobs::list(&fixture.paths.data_dir, Some(&fixture.session)).unwrap();
                if jobs.len() >= 3 && jobs.iter().all(|j| j.status == "completed") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("idle hook job did not finish");
        // Allow another idle poll to expose a replay.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        {
            let requests = fixture.requests.lock().unwrap();
            let wire: Vec<_> = requests
                .iter()
                .filter(|r| r["stream"] == true)
                .map(|r| r["messages"].to_string())
                .collect();
            assert_eq!(wire.len(), 3, "{mode:?}");
            assert!(wire[0].contains("first explicit turn"));
            assert!(wire[1].contains("second explicit turn"));
            assert_eq!(wire[0].matches(ORDINARY).count(), 1);
            assert_eq!(wire.join("").matches(ORDINARY).count(), 1);
            assert_eq!(wire[2].matches(REWAKE).count(), 1);
            assert_eq!(wire.join("").matches(REWAKE).count(), 1);
            assert!(wire[2].contains("Stop hook blocking error from command"));
        }
        let db = fixture.paths.open_database().unwrap();
        let authority = deepseek_code_core::services::permission_authority::history(
            &db,
            &fixture.session,
            10000,
        )
        .unwrap();
        assert_eq!(
            authority,
            [
                json!({"role":"user","content":"first explicit turn"}),
                json!({"role":"user","content":"second explicit turn"})
            ]
        );
        tty.submit("/exit", mode.line_editor()).await;
        tty.wait_success().await;
        eprintln!("{mode:?}: frontend published before first turn, ordinary result delivered once, one idle hook request and two human authority entries");
    }
}
