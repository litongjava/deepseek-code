//! A user frame that arrives during a stream-json turn steers that turn through
//! the same durable input queue the REPL and Desktop use.
use super::*;
use deepseek_code_core::native_engine::provider_manager::ApiFormat;
use deepseek_code_core::services::input_queue::{QueueMode, QueueState};
use serde_json::Value;
use std::path::Path;

const STEER_TOOL: &str = "Bash";

fn streamed_requests(server: &Server) -> Vec<Value> {
    server
        .fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|body| body["stream"] == true)
        .cloned()
        .collect()
}

/// Wait until the model endpoint has recorded `count` streaming requests.
async fn wait_for_requests(server: &Server, count: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let seen = server
                .fixture
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|body| body["stream"] == true)
                .count();
            if seen >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("model endpoint did not receive the expected request");
}

fn api_format(anthropic: bool) -> ApiFormat {
    if anthropic {
        ApiFormat::Anthropic
    } else {
        ApiFormat::OpenAI
    }
}

fn session_store(root: &Path) -> deepseek_code_core::runtime::SessionStore {
    let paths = deepseek_code_core::runtime::RuntimePaths::new(root.join("data")).unwrap();
    deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap())
}

/// The first message holding the tool result in either provider protocol.
fn tool_result_index(body: &Value) -> Option<usize> {
    body["messages"].as_array()?.iter().position(|message| {
        message["content"].as_array().is_some_and(|blocks| {
            blocks.iter().any(|block| block["type"] == "tool_result")
        }) || message["role"] == "tool"
    })
}

fn message_index(body: &Value, needle: &str) -> Option<usize> {
    body["messages"]
        .as_array()?
        .iter()
        .position(|message| message.to_string().contains(needle))
}

fn message_count(body: &Value, needle: &str) -> usize {
    body["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message.to_string().contains(needle))
                .count()
        })
        .unwrap_or(0)
}

fn durable_tool_index(history: &[Value]) -> Option<usize> {
    history.iter().position(|message| {
        message["role"] == "tool"
            || message["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"))
    })
}

/// The baseline probe: the host keeps stdin open and corrects a running turn.
#[tokio::test]
async fn user_frame_during_a_tool_run_reaches_the_next_model_request() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::SlowTool).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &["--allowed-tools", STEER_TOOL]);
        client.send(user("Run the slow probe")).await;
        // The tool is now executing; the host corrects the turn while it runs.
        client.frame("tool_start").await;
        client
            .send(user("Skip the rest and answer STEERED now"))
            .await;
        let delivered = client.frame("queued_input").await;
        assert_eq!(delivered["text"], "Skip the rest and answer STEERED now");
        assert!(
            delivered["origin"]
                .as_str()
                .unwrap()
                .starts_with("queue-input-"),
            "{delivered}"
        );
        let result = client.result().await;
        assert_eq!(result["is_error"], false, "{result}");
        assert_eq!(result["result"], "reply-2");
        let requests = streamed_requests(&server);
        assert_eq!(requests.len(), 2, "{requests:?}");
        let second = &requests[1];
        assert_eq!(
            second.to_string().matches("answer STEERED now").count(),
            1,
            "{second}"
        );
        let tool = tool_result_index(second).expect("tool result in the second request");
        let steer = message_index(second, "answer STEERED now").expect("steering text");
        assert!(
            steer >= tool,
            "the steering text must follow the tool result: {second}"
        );
        // Anthropic puts both blocks in the tool-result user message, so check
        // the block order there as well.
        if anthropic {
            let blocks = second["messages"][steer].to_string();
            let result = blocks.find("\"type\":\"tool_result\"").expect("tool result block");
            let steer = blocks.find("answer STEERED now").unwrap();
            assert!(result < steer, "steering text precedes the tool result: {second}");
        }
        let (status, results) = client.finish().await;
        assert!(status.success());
        assert_eq!(results.len(), 0, "the only result was already read");

        let store = session_store(root.path());
        let session_id = result["session_id"].as_str().unwrap();
        let history = store.model_history(session_id, &api_format(anthropic)).unwrap();
        let transcript = serde_json::to_string(&history).unwrap();
        assert_eq!(
            transcript.matches("answer STEERED now").count(),
            1,
            "{transcript}"
        );
        let tool = durable_tool_index(&history).expect("durable tool result");
        let steer = history
            .iter()
            .position(|message| message.to_string().contains("answer STEERED now"))
            .expect("durable steering message");
        // The assistant row aggregates the turn's text, so the steering row is
        // after it; what matters is that it follows the tool result and is the
        // last saved message of the turn.
        assert!(steer > tool, "transcript order: {transcript}");
        assert_eq!(steer, history.len() - 1, "transcript tail: {transcript}");
        let queue = deepseek_code_core::services::input_queue::list(&store.db, session_id).unwrap();
        assert_eq!(queue.len(), 1, "{queue:?}");
        assert_eq!(queue[0].mode, QueueMode::Steer);
        assert_eq!(queue[0].state, QueueState::Handoff, "{queue:?}");
    }
}

#[tokio::test]
async fn two_frames_arrive_in_order_and_exactly_once() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::SlowTool).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &["--allowed-tools", STEER_TOOL]);
        client.send(user("Run the slow probe")).await;
        client.frame("tool_start").await;
        let mut first = user("STEER_ONE prefer the short answer");
        first["uuid"] = json!("steer-one");
        client.send(first).await;
        client.send(user("STEER_TWO mention A")).await;
        let one = client.frame("queued_input").await;
        let two = client.frame("queued_input").await;
        assert_eq!(one["text"], "STEER_ONE prefer the short answer");
        assert_eq!(two["text"], "STEER_TWO mention A");
        let result = client.result().await;
        assert_eq!(result["is_error"], false);
        let requests = streamed_requests(&server);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(message_count(&requests[1], "STEER_ONE"), 1, "{}", requests[1]);
        assert_eq!(message_count(&requests[1], "STEER_TWO"), 1, "{}", requests[1]);
        let body = requests[1].to_string();
        let one = body.find("STEER_ONE").unwrap();
        let two = body.find("STEER_TWO").unwrap();
        assert!(one < two, "arrival order: {body}");
        let (status, _) = client.finish().await;
        assert!(status.success());

        let store = session_store(root.path());
        let history = store
            .model_history(result["session_id"].as_str().unwrap(), &api_format(anthropic))
            .unwrap();
        let transcript = serde_json::to_string(&history).unwrap();
        assert_eq!(
            transcript.matches("STEER_ONE").count(),
            1,
            "each frame is saved once: {transcript}"
        );
        assert_eq!(transcript.matches("STEER_TWO").count(), 1, "{transcript}");
        let one = history
            .iter()
            .position(|message| message.to_string().contains("STEER_ONE"))
            .unwrap();
        let two = history
            .iter()
            .position(|message| message.to_string().contains("STEER_TWO"))
            .unwrap();
        assert!(one < two, "persisted arrival order: {transcript}");
    }
}

/// Without a boundary the frame still runs, exactly once, as the next turn.
#[tokio::test]
async fn frame_during_the_final_answer_runs_as_the_next_turn_once() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::SlowText).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(user("Answer briefly")).await;
        wait_for_requests(&server, 1).await;
        client.send(user("LATE_STEER_SENTINEL")).await;
        let (status, results, transcript) = client.finish_with_transcript().await;
        assert!(status.success(), "{transcript}");
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0]["result"], "reply-1");
        assert_eq!(results[1]["result"], "reply-2");
        let requests = streamed_requests(&server);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(message_count(&requests[0], "LATE_STEER_SENTINEL"), 0);
        assert_eq!(message_count(&requests[1], "LATE_STEER_SENTINEL"), 1);
    }
}

#[tokio::test]
async fn slash_commands_keep_their_next_turn_behavior() {
    let server = Server::start(false, Mode::SlowTool).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &["--allowed-tools", STEER_TOOL]);
    client.send(user("Run the slow probe")).await;
    client.frame("tool_start").await;
    client.send(user("/status")).await;
    let (status, results, transcript) = client.finish_with_transcript().await;
    assert!(status.success(), "{transcript}");
    let requests = streamed_requests(&server);
    assert!(requests.len() >= 2, "{requests:?}");
    assert_eq!(
        message_count(&requests[1], "/status"),
        0,
        "a slash command must not join the running turn: {}",
        requests[1]
    );
    assert_eq!(results.len(), 2, "{results:?}");
}

#[tokio::test]
async fn interrupt_keeps_the_steered_frame_for_the_next_turn() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::SlowTool).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &["--allowed-tools", STEER_TOOL]);
        client.send(user("Run the slow probe")).await;
        client.frame("tool_start").await;
        client.send(user("KEEP_ME_AFTER_INTERRUPT")).await;
        client
            .send(json!({"type":"control_request","request_id":"stop","request":{"subtype":"interrupt"}}))
            .await;
        let (status, results, transcript) = client.finish_with_transcript().await;
        assert!(!status.success(), "an interrupted SDK session exits nonzero");
        assert!(
            results.len() >= 2,
            "the kept frame runs next: {results:?}\n{transcript}"
        );
        assert_eq!(results[0]["is_error"], true, "{}", results[0]);
        assert_eq!(results[1]["is_error"], false, "{}", results[1]);
        let requests = streamed_requests(&server);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(message_count(&requests[1], "KEEP_ME_AFTER_INTERRUPT"), 1);
    }
}

/// Delivery event and the replayed user envelope carry the same UUID.
#[tokio::test]
async fn replay_user_messages_emits_the_steered_envelope_on_delivery() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::SlowTool).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(
            root.path(),
            &server,
            &["--allowed-tools", STEER_TOOL, "--replay-user-messages"],
        );
        let mut first = user("Run the slow probe");
        first["uuid"] = json!("first-user-uuid");
        client.send(first).await;
        assert_eq!(client.frame("user").await["uuid"], "first-user-uuid");
        client.frame("tool_start").await;
        let mut steer = user("REPLAY_STEER_SENTINEL");
        steer["uuid"] = json!("steer-user-uuid");
        client.send(steer).await;
        let mut frames = Vec::new();
        loop {
            let frame = client.next_frame().await;
            let result = frame["type"] == "result";
            frames.push(frame);
            if result {
                break;
            }
        }
        assert_eq!(frames.last().unwrap()["result"], "reply-2");
        let delivered = frames
            .iter()
            .position(|frame| frame["type"] == "queued_input")
            .expect("delivery event");
        assert_eq!(frames[delivered]["text"], "REPLAY_STEER_SENTINEL");
        assert!(frames[delivered]["origin"]
            .as_str()
            .unwrap()
            .starts_with("queue-input-"));
        let tool = frames
            .iter()
            .position(|frame| frame["type"] == "tool_result")
            .expect("tool result event");
        assert!(tool < delivered, "delivery follows the tool result: {frames:?}");
        let replays: Vec<usize> = frames
            .iter()
            .enumerate()
            .filter(|(_, frame)| frame["type"] == "user" && frame["uuid"] == "steer-user-uuid")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            replays.len(),
            1,
            "the steered frame is replayed exactly once: {frames:?}"
        );
        assert!(
            replays[0] > delivered,
            "replay happens at delivery: {frames:?}"
        );
        let (status, extra) = client.finish().await;
        assert!(status.success());
        assert!(extra.is_empty(), "no duplicate results: {extra:?}");
        let session_id = frames.last().unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let store = session_store(root.path());
        let rows = store
            .db
            .with_conn(|conn| {
                deepseek_code_core::db::message_repo::get_messages_by_conversation(conn, &session_id)
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            rows.iter()
                .filter(|row| row.role == "user" && row.content == "REPLAY_STEER_SENTINEL")
                .count(),
            1,
            "the delivered frame is saved once: {rows:?}"
        );
        let history = store
            .model_history(&session_id, &api_format(anthropic))
            .unwrap();
        let transcript = serde_json::to_string(&history).unwrap();
        assert_eq!(
            transcript.matches("REPLAY_STEER_SENTINEL").count(),
            1,
            "a delivered steering frame appears once in the rebuilt history: {transcript}"
        );
    }
}
