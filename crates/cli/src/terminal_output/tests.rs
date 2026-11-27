#[cfg(test)]
use crate::fixture_sql::FixtureSql;
use axum::{
    extract::State,
    response::{sse::Event, IntoResponse, Sse},
    routing::post,
    Json, Router,
};
use deepseek_code_core::{
    db::turn_repo::{latest_turn, TurnStatus},
    runtime::{RuntimePaths, SessionStore},
};
use futures::{stream, StreamExt};
use portable_pty::CommandBuilder;
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[path = "../../tests/support/linux_terminal.rs"]
mod linux_terminal;
use linux_terminal::{Terminal, Trigger, PROMPT};

const VISIBLE: &str = "visible-assistant-token";
const CHILD: &str = "terminal_output::tests::instrumented_cli";

fn response_text() -> String {
    // A few complete Markdown paragraphs commit to scrollback in one frame.
    let paragraph = "still streaming ".repeat(1024);
    format!(
        "{VISIBLE}\n\n{}response-tail\n",
        format!("{paragraph}\n\n").repeat(6)
    )
}

const VOLUME_ENV: &str = "DCL_TEST_SHARED_FULL_VOLUME";
const VOLUME_SIZE: i64 = 16 * 1024 * 1024;

fn leave_space_for_partial_reply(volume: &Path) {
    use std::os::fd::AsRawFd;
    let filler = fs::File::create_new(volume.join("filler")).unwrap();
    let mut stat = std::mem::MaybeUninit::uninit();
    assert_eq!(
        unsafe { libc::fstatfs(filler.as_raw_fd(), stat.as_mut_ptr()) },
        0
    );
    let stat = unsafe { stat.assume_init() };
    assert_eq!(stat.f_type, libc::TMPFS_MAGIC);
    assert_eq!(stat.f_blocks as i64 * stat.f_bsize as i64, VOLUME_SIZE);
    let bytes = stat.f_bavail as i64 * stat.f_bsize as i64 - 32 * 1024;
    assert!(bytes > 0);
    assert_eq!(
        unsafe { libc::posix_fallocate(filler.as_raw_fd(), 0, bytes) },
        0
    );
}

async fn reply(
    State(volume): State<Option<PathBuf>>,
    Json(request): Json<Value>,
) -> axum::response::Response {
    if request["stream"] != true {
        return Json(json!({"choices":[{"message":{"role":"assistant","content":"Fixture"},"finish_reason":"stop"}]})).into_response();
    }

    if let Some(volume) = volume {
        leave_space_for_partial_reply(&volume);
    }
    // The first delta exceeds the PTY buffer. Keep the request open so no
    // successful turn completion or next prompt can race the hangup.
    let delta = json!({"choices":[{"index":0,"delta":{"content": response_text()}}]});
    let first = stream::iter([Ok::<_, Infallible>(
        Event::default().data(delta.to_string()),
    )]);
    Sse::new(first.chain(stream::pending())).into_response()
}

// After master close even the panic hook's stderr is gone. This child runs
// the real CLI with a hook on every thread, so a renderer panic cannot look
// like the CLI's intentional exit 1 or hide behind a discarded JoinHandle.
#[test]
#[ignore = "launched by the streaming PTY tests"]
fn instrumented_cli() {
    if std::env::var_os("DCL_TEST_ARGV").is_none() {
        return;
    }
    std::panic::set_hook(Box::new(|info| {
        let _ = fs::write("panic.txt", info.to_string());
        std::process::exit(101);
    }));
    if let Some(path) = std::env::var_os("DCL_TEST_STDOUT") {
        use std::os::fd::AsRawFd;
        let sink = fs::OpenOptions::new().write(true).open(path).unwrap();
        assert_eq!(
            unsafe { libc::dup2(sink.as_raw_fd(), libc::STDOUT_FILENO) },
            libc::STDOUT_FILENO
        );
    }
    let status = crate::cli_main(|| {
        let args: Vec<String> =
            serde_json::from_str(&std::env::var("DCL_TEST_ARGV").unwrap()).unwrap();
        crate::Cli::try_parse_with_mode_source(args).unwrap()
    });
    if status == ExitCode::FAILURE {
        std::process::exit(1);
    }
    assert_eq!(status, ExitCode::SUCCESS);
    std::process::exit(0);
}

#[derive(Clone, Copy)]
enum DisplayFailure {
    TerminalClose,
    Full,
    FileEio,
    SharedVolume,
    FullWithTimerFailure,
}

async fn fail_stream(screen_reader: bool, failure: DisplayFailure) {
    let fixture = tempfile::tempdir().unwrap();
    let volume = matches!(failure, DisplayFailure::SharedVolume)
        .then(|| PathBuf::from(std::env::var_os(VOLUME_ENV).unwrap()));
    let data = volume.as_deref().unwrap_or(fixture.path()).join("data");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&data).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(volume.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    linux_terminal::write_provider(&data, &endpoint);
    deepseek_code_core::data_layout::write_config_file(
        deepseek_code_core::data_layout::DataLayout::from_root(&data)
            .config_file("startup-prompts.json"),
        json!({"trusted_folders":[workspace]}).to_string(),
    )
    .unwrap();
    deepseek_code_core::data_layout::write_config_file(deepseek_code_core::data_layout::DataLayout::from_root(&data).config_file("settings.json"), json!({"hooks":{"SessionEnd":[{"hooks":[{"type":"command","command":"cat >> session-end.json"}]}]}}).to_string()).unwrap();

    let mut command = Terminal::command(
        CommandBuilder::new(std::env::current_exe().unwrap()),
        &workspace,
        &data,
        &fixture.path().join("coord"),
        false,
        screen_reader,
    );
    let session_id=uuid::Uuid::new_v4().to_string();
    command.args(["--session-id",&session_id]);
    let args: Vec<_> = command
        .get_argv()
        .iter()
        .map(|arg| arg.to_str().unwrap())
        .collect();
    let args = serde_json::to_string(&args).unwrap();
    command.env("DCL_TEST_ARGV", args);
    command.get_argv_mut().truncate(1);
    command.args(["--ignored", "--exact", CHILD, "--nocapture"]);
    let trace = workspace.join("strace.log");
    match failure {
        DisplayFailure::TerminalClose => {}
        DisplayFailure::Full | DisplayFailure::FullWithTimerFailure => {
            command.env("DCL_TEST_STDOUT", "/dev/full")
        }
        DisplayFailure::FileEio | DisplayFailure::SharedVolume => {
            let file = volume.as_deref().unwrap_or(&workspace).join("response.txt");
            fs::File::create(&file).unwrap();
            command.env("DCL_TEST_STDOUT", &file);
            let tracer = std::env::var_os("STRACE").unwrap_or_else(|| "strace".into());
            let mut prefix = vec![
                tracer,
                "-f".into(),
                "-qq".into(),
                "-yy".into(),
                "-e".into(),
                "trace=write,writev,pwrite64".into(),
                "-P".into(),
                file.into_os_string(),
                "-o".into(),
                trace.clone().into_os_string(),
            ];
            if volume.is_some() {
                prefix.extend([
                    "-P".into(),
                    data.join("data/sessions")
                        .join(&session_id)
                        .join("session.db-wal")
                        .into_os_string(),
                ]);
            } else {
                prefix.extend(["-e".into(), "inject=write:error=EIO".into()]);
            }
            command.get_argv_mut().splice(..0, prefix);
        }
    }
    let mut terminal = Terminal::spawn(command);
    terminal
        .wait_output(if screen_reader { b"\n> " } else { PROMPT }, true)
        .await;
    if matches!(failure, DisplayFailure::FullWithTimerFailure) {
        // The transcript can commit here. Only the later turn-timer update fails.
        crate::fixture_sql::shard_connection(&data,&session_id)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_turn_finish BEFORE UPDATE OF status ON execution_turns
             BEGIN SELECT RAISE(ABORT, 'turn timer write failed'); END;",
            )
            .unwrap();
    }
    let prompt = if screen_reader {
        "Exercise streamed output".to_owned()
    } else {
        linux_terminal::image_prompt(&workspace, "Read")
    };
    terminal.submit(&prompt, screen_reader).await;
    let expected_status = match failure {
        DisplayFailure::TerminalClose => {
            close_blocked_display(&mut terminal, screen_reader).await;
            TurnStatus::Cancelled
        }
        DisplayFailure::Full
        | DisplayFailure::FileEio
        | DisplayFailure::SharedVolume
        | DisplayFailure::FullWithTimerFailure => {
            let diagnostic = match failure {
                DisplayFailure::FileEio => "Input/output error",
                DisplayFailure::SharedVolume | DisplayFailure::FullWithTimerFailure => {
                    "failed printing to stdout: No space left on device"
                }
                _ => "No space left on device",
            };
            terminal.wait_output(diagnostic.as_bytes(), false).await;
            terminal.expect_exit(Trigger::OutputError).await;
            terminal.pump(false);
            let transcript = String::from_utf8_lossy(&terminal.transcript);
            assert!(
                !transcript.contains("Interrupted") && !transcript.contains("\n> "),
                "{transcript}"
            );
            if matches!(failure, DisplayFailure::FileEio) {
                assert!(fs::read_to_string(&trace)
                    .unwrap()
                    .lines()
                    .any(|line| line.contains("write(1<")
                        && line.contains("EIO")
                        && line.contains("INJECTED")));
            }
            if matches!(failure, DisplayFailure::FullWithTimerFailure) {
                TurnStatus::Running
            } else {
                TurnStatus::Failed
            }
        }
    };
    assert!(
        !workspace.join("panic.txt").exists(),
        "a dcl thread panicked"
    );
    if let Some(volume) = volume {
        let bytes = fs::metadata(volume.join("response.txt")).unwrap().len();
        assert!(bytes > 0 && bytes < response_text().len() as u64);
        let log = fs::read_to_string(trace).unwrap();
        for (syscall, path) in [
            ("write(1<", "/response.txt>"),
            ("pwrite64(", "/session.db-wal>"),
        ] {
            assert!(
                log.lines().any(|line| line.contains(syscall)
                    && line.contains(path)
                    && line.contains("= -1 ENOSPC")),
                "missing ENOSPC on {path}"
            );
        }
        return;
    }
    let hook: Value = serde_json::from_slice(
        &fs::read(workspace.join("session-end.json")).expect("SessionEnd ran"),
    )
    .unwrap();
    assert_eq!(hook["hook_event_name"], "SessionEnd");
    assert_eq!(hook["reason"], "other");
    let session = hook["session_id"].as_str().unwrap();
    let paths = RuntimePaths::new(data).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let history = store.history(session).unwrap();
    let assistant = history
        .iter()
        .find(|row| row["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["content"].as_str().unwrap(), response_text());
    let turn = store
        .db
        .shard_sql(session, |c| latest_turn(c, session))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(turn.status, expected_status);
    assert_eq!(
        turn.ended_at.is_some(),
        expected_status != TurnStatus::Running
    );
}

async fn close_blocked_display(terminal: &mut Terminal, screen_reader: bool) {
    terminal.wait_output(VISIBLE.as_bytes(), true).await;
    // Reading stops at the marker. The child must still be inside the write
    // to the expected stream when we close the master.
    let fd = if screen_reader { 1 } else { 2 };
    let deadline = std::time::Instant::now() + linux_terminal::READY_TIMEOUT;
    loop {
        let tasks = fs::read_dir(format!("/proc/{}/task", terminal.pid())).unwrap();
        if tasks.filter_map(Result::ok).any(|task| {
            let call = fs::read_to_string(task.path().join("syscall")).unwrap_or_default();
            let mut fields = call.split_whitespace();
            matches!(
                fields.next().and_then(|n| n.parse::<libc::c_long>().ok()),
                Some(libc::SYS_write | libc::SYS_writev)
            ) && fields
                .next()
                .and_then(|n| n.strip_prefix("0x"))
                .and_then(|n| u32::from_str_radix(n, 16).ok())
                == Some(fd)
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "missing blocked write on fd {fd}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    terminal.expect_exit(Trigger::Close).await;
}

#[tokio::test]
#[ignore = "requires rootless bwrap tmpfs and strace"]
async fn screen_reader_partial_write_and_full_session_volume() {
    if std::env::var_os(VOLUME_ENV).is_some() {
        fail_stream(true, DisplayFailure::SharedVolume).await;
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let volume = scratch.path().join("volume");
    fs::create_dir(&volume).unwrap();
    let status = std::process::Command::new("bwrap")
        .args([
            "--die-with-parent",
            "--ro-bind",
            "/",
            "/",
            "--dev-bind",
            "/dev",
            "/dev",
            "--bind",
        ])
        .arg(scratch.path())
        .arg(scratch.path())
        .arg("--size")
        .arg(VOLUME_SIZE.to_string())
        .arg("--tmpfs")
        .arg(&volume)
        .arg(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "terminal_output::tests::screen_reader_partial_write_and_full_session_volume",
            "--nocapture",
        ])
        .env(VOLUME_ENV, volume)
        .env("TMPDIR", scratch.path())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "bounded filesystem fixture exited with {status}"
    );
}

#[tokio::test]
async fn redirected_output_failure_survives_a_failed_turn_timer() {
    fail_stream(true, DisplayFailure::FullWithTimerFailure).await;
}

#[tokio::test]
async fn redirected_output_full_screen_reader_turn() {
    fail_stream(true, DisplayFailure::Full).await;
}

#[tokio::test]
#[ignore = "requires strace with path-filtered write injection (set STRACE or PATH)"]
async fn redirected_output_file_eio_screen_reader_turn() {
    fail_stream(true, DisplayFailure::FileEio).await;
}

#[tokio::test]
async fn screen_reader_close_during_streaming_output() {
    fail_stream(true, DisplayFailure::TerminalClose).await;
}

#[tokio::test]
async fn normal_close_during_streaming_output() {
    fail_stream(false, DisplayFailure::TerminalClose).await;
}

#[test]
fn redirected_output_requires_the_same_disconnected_terminal() {
    // Session ownership and SIGHUP disposition belong to this child alone.
    const CHILD: &str = "DCL_I18_ENDPOINT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "terminal_output::tests::redirected_output_requires_the_same_disconnected_terminal",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    fn pty() -> (OwnedFd, OwnedFd) {
        let (mut master, mut slave) = (-1, -1);
        let status = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(status, 0);
        unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
    }
    let (master, control) = pty();
    let (foreign_master, other) = pty();
    let control = control.as_raw_fd();
    let other = other.as_raw_fd();
    unsafe {
        assert_ne!(libc::setsid(), -1);
        assert_eq!(libc::ioctl(control, libc::TIOCSCTTY, 0), 0);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let file = tempfile::tempfile().unwrap();
    let stdin = file.as_raw_fd();
    let controlling = super::Endpoint::from_fd(control, stdin);
    let foreign = super::Endpoint::from_fd(other, control);
    let regular = super::Endpoint::from_fd(stdin, control);

    assert!(!controlling.hung_up_at(control, stdin));
    drop(foreign_master);
    assert!(!foreign.hung_up_at(other, control));
    drop(master);
    assert!(controlling.hung_up_at(control, stdin));
    assert!(!controlling.hung_up_at(stdin, stdin)); // fd now points at a file
    assert!(!regular.hung_up_at(stdin, control)); // input hung up independently
    assert!(!foreign.hung_up_at(other, control)); // two different dead PTYs
}
