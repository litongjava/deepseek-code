//! A Linux PTY exercises both kernel SIGHUP and descriptor-only hangup.

#[path = "../../../core/tests/support/session_sql.rs"]
mod fixture_sql;
use fixture_sql::FixtureSql;
use super::linux_terminal;
use super::{poll_rows, rows_in, write_provider, write_settings, Lifecycle};
use axum::{extract::State, routing::post, Json, Router};
use deepseek_code_core::{
    db::turn_repo::{latest_turn, TurnStatus},
    runtime::{RuntimePaths, SessionStore},
};
use linux_terminal::{Terminal, Trigger, CURSOR_QUERY, PROMPT, READY_TIMEOUT};
use portable_pty::CommandBuilder;

use std::{
    fs,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const GATED_PROMPT: &str = "Hold the terminal hangup test turn";

async fn start_model() -> (String, Arc<AtomicBool>) {
    async fn reply(
        State(in_turn): State<Arc<AtomicBool>>,
        Json(request): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        // A separate, non-streaming request generates the terminal title.
        if request["stream"] == true {
            assert!(request["messages"].to_string().contains(GATED_PROMPT));
            in_turn.store(true, Ordering::Release);
            return std::future::pending().await;
        }
        Json(serde_json::json!({
            "id": "fixture-title", "object": "chat.completion", "created": 1,
            "model": "fixture-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Local fixture"},
                         "finish_reason": "stop"}]
        }))
    }

    let in_turn = Arc::new(AtomicBool::new(false));
    let model = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(in_turn.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, model).await.unwrap() });
    (endpoint, in_turn)
}

async fn run(trigger: Trigger, controlling_terminal: bool, mid_turn: bool, screen_reader: bool) {
    let fixture = tempfile::tempdir().unwrap();
    let data = fixture.path().join("data");
    let workspace = fixture.path().join("workspace");
    let coordination = fixture.path().join("coord");
    fs::create_dir(&workspace).unwrap();
    write_settings(&data, true);
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/startup-prompts.json"),
        serde_json::json!({"trusted_folders": [&workspace]}).to_string(),
    )
    .unwrap();
    let (endpoint, in_turn) = start_model().await;
    write_provider(&data, &endpoint);
    let mut terminal = Terminal::spawn(Terminal::command(
        CommandBuilder::new(env!("CARGO_BIN_EXE_dcl")),
        &workspace,
        &data,
        &coordination,
        controlling_terminal,
        screen_reader,
    ));
    let ready: &[u8] = if screen_reader { b"\n> " } else { PROMPT };
    terminal.wait_output(ready, true).await;

    let frontend = poll_rows(&coordination)
        .into_iter()
        .find(|row| row.owner.pid == terminal.pid() && row.lifecycle == Lifecycle::Open)
        .expect("the frontend is registered at the ready prompt");
    let session = frontend.session_id.unwrap();
    if mid_turn {
        // An image attachment takes the direct engine path with a raw live
        // display, avoiding an independently persistent worker in this test.
        let prompt = if screen_reader {
            GATED_PROMPT.to_owned()
        } else {
            linux_terminal::image_prompt(&workspace, GATED_PROMPT)
        };
        terminal.submit(&prompt, screen_reader).await;
        if !screen_reader {
            terminal.wait_output(b"\x1b[?2004h", true).await;
        }
        let deadline = Instant::now() + READY_TIMEOUT;
        while !in_turn.load(Ordering::Acquire) {
            terminal.pump(true);
            assert!(
                Instant::now() < deadline,
                "the provider never received the turn: {}",
                String::from_utf8_lossy(&terminal.transcript)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    terminal.expect_exit(trigger).await;
    assert!(rows_in(&coordination)
        .iter()
        .any(|row| row.instance_id == frontend.instance_id && row.lifecycle == Lifecycle::Closed));

    if mid_turn {
        let paths = RuntimePaths::new(data).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let status = store
            .db
            .shard_sql(&session, |conn| latest_turn(conn, &session))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(status.status, TurnStatus::Cancelled);
        assert!(
            status.ended_at.is_some(),
            "cancellation persisted its own endpoint"
        );
        assert!(store.history(&session).unwrap().iter().any(|row| {
            row["role"] == "user"
                && row["content"]
                    .as_str()
                    .is_some_and(|text| text.contains(GATED_PROMPT))
        }));
    }
}

#[tokio::test]
async fn master_close_while_idle_exits_and_closes_the_registry() {
    run(Trigger::Close, true, false, false).await;
}

#[tokio::test]
async fn master_close_without_a_controlling_terminal_exits_while_idle() {
    run(Trigger::Close, false, false, false).await;
}

#[tokio::test]
async fn master_close_at_a_plain_prompt_is_a_failure() {
    run(Trigger::Close, false, false, true).await;
}

#[tokio::test]
async fn master_close_mid_turn_cancels_with_live_input() {
    run(Trigger::Close, true, true, false).await;
}

#[tokio::test]
async fn master_close_mid_turn_cancels_without_sighup_or_live_input() {
    run(Trigger::Close, false, true, true).await;
}

#[tokio::test]
async fn sighup_mid_turn_exits_cooperatively() {
    run(Trigger::Signal(libc::SIGHUP), true, true, false).await;
}

#[tokio::test]
async fn sigterm_while_idle_exits_cooperatively() {
    run(Trigger::Signal(libc::SIGTERM), true, false, false).await;
}

async fn close_during_startup(ready: &[u8], trusted: bool, controlling_terminal: bool) {
    let fixture = tempfile::tempdir().unwrap();
    let data = fixture.path().join("data");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    write_settings(&data, true);
    if trusted {
        deepseek_code_core::data_layout::write_config_file(
            data.join("config/startup-prompts.json"),
            serde_json::json!({"trusted_folders": [&workspace]}).to_string(),
        )
        .unwrap();
    }
    let (endpoint, _) = start_model().await;
    write_provider(&data, &endpoint);
    let mut terminal = Terminal::spawn(Terminal::command(
        CommandBuilder::new(env!("CARGO_BIN_EXE_dcl")),
        &workspace,
        &data,
        &fixture.path().join("coord"),
        controlling_terminal,
        false,
    ));
    terminal.wait_output(ready, false).await;
    terminal.expect_exit(Trigger::Close).await;
}

#[tokio::test]
async fn master_close_during_a_cursor_query_exits() {
    // Leave the editor waiting for its first cursor report.
    close_during_startup(CURSOR_QUERY, true, false).await;
}

#[tokio::test]
async fn master_close_in_the_startup_picker_exits() {
    // The initial trust picker reads synchronously on dcl-main.
    close_during_startup(b"Accessing workspace:", false, true).await;
}
