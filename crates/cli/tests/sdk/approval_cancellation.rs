//! Public SDK cancellation must finalize every tool, not just ApplyPatch.
use super::*;
use deepseek_code_core::{
    db::{message_repo, turn_repo},
    runtime::RuntimePaths,
};

pub(super) fn assert_cancelled_call(root: &Path, session: &str, occurrence: &str, name: &str) {
    let calls = apply_patch_permissions::persisted_tool_calls(root, session);
    let matching: Vec<_> = calls.iter().filter(|call| call.name == name).collect();
    assert_eq!(matching.len(), 1, "{calls:?}");
    let call = matching[0];
    assert!(call.id.ends_with(&format!(":{occurrence}")), "{call:?}");
    assert!(call.is_error, "cancelled call must be an error: {call:?}");
    assert_eq!(
        call.output.as_deref(),
        Some(message_repo::UNFINISHED_TOOL_RESULT)
    );
    let db = RuntimePaths::new(root.join("data"))
        .unwrap()
        .open_database()
        .unwrap();
    let turn = db
        .with_conn(|conn| turn_repo::get_message_turn(conn, &call.message_id))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(turn.status, turn_repo::TurnStatus::Cancelled, "{turn:?}");
    assert!(turn.ended_at.is_some(), "{turn:?}");
    println!(
        "CANCEL_RECORD session={session} call={} name={name} output={:?} is_error={} turn={:?}",
        call.id, call.output, call.is_error, turn.status
    );
}

async fn cancelled_approval(name: &str, interrupt: bool) {
    for anthropic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mode = if name == "Bash" {
            Mode::ShellApproval
        } else {
            Mode::Approval
        };
        let server = Server::start(anthropic, mode).await;
        let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
        client.send(user("Wait for approval")).await;
        let pending = client.frame("control_request").await;
        assert_eq!(pending["request"]["tool_name"], name, "{pending}");
        let occurrence = pending["request"]["tool_use_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let marker = root.path().join(if name == "Bash" {
            "approval-shell.txt"
        } else {
            "approval.txt"
        });
        assert!(!marker.exists());
        let result = if interrupt {
            client.send(json!({"type":"control_request","request_id":"stop","request":{"subtype":"interrupt"}})).await;
            assert_eq!(
                client.frame("control_cancel_request").await["request_id"],
                pending["request_id"]
            );
            assert_eq!(
                client.frame("control_response").await["response"]["request_id"],
                "stop"
            );
            let result = client.result().await;
            assert_eq!(result["is_error"], true, "{result}");
            assert_cancelled_call(
                root.path(),
                result["session_id"].as_str().unwrap(),
                &occurrence,
                name,
            );
            // The same session remains usable after the cancelled occurrence.
            client.send(user("Continue without tools")).await;
            let next = client.result().await;
            assert_eq!(next["is_error"], false, "{next}");
            assert_eq!(next["result"], "reply-2");
            let (status, extra) = client.finish().await;
            assert!(!status.success());
            assert!(extra.is_empty(), "duplicate terminal result: {extra:?}");
            result
        } else {
            let (status, results) = client.finish().await;
            assert!(!status.success());
            assert_eq!(results.len(), 1, "{results:?}");
            results[0].clone()
        };
        assert_eq!(result["is_error"], true, "{result}");
        assert!(!marker.exists(), "cancelled {name} ran");
        assert_cancelled_call(
            root.path(),
            result["session_id"].as_str().unwrap(),
            &occurrence,
            name,
        );
        assert_eq!(
            server.fixture.requests.lock().unwrap().len(),
            if interrupt { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn sdk_eof_cancels_write_approval_record() {
    cancelled_approval("Write", false).await;
}

#[tokio::test]
async fn sdk_eof_cancels_bash_approval_record() {
    cancelled_approval("Bash", false).await;
}

#[tokio::test]
async fn sdk_interrupt_cancels_write_approval_record() {
    cancelled_approval("Write", true).await;
}

#[tokio::test]
async fn sdk_interrupt_cancels_bash_approval_record() {
    cancelled_approval("Bash", true).await;
}
