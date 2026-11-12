//! Reference SDK message frames next to the legacy event lines.
use super::*;

fn of_type<'a>(frames: &'a [Value], kind: &str) -> Vec<&'a Value> {
    frames.iter().filter(|frame| frame["type"] == kind).collect()
}

async fn transcript(anthropic: bool, extra: &[&str]) -> Vec<Value> {
    let server = Server::start(anthropic, Mode::ReadThenText).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, extra);
    client.send(user("Read the fixture")).await;
    let (status, _, transcript) = client.finish_with_transcript().await;
    assert!(status.success(), "{transcript}");
    transcript.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

#[tokio::test]
async fn stream_json_emits_init_assistant_tool_result_and_full_result() {
    for anthropic in [false, true] {
        let frames = transcript(anthropic, &[]).await;
        let init = of_type(&frames, "system");
        assert_eq!(init.len(), 1, "{frames:?}");
        assert_eq!(init[0]["subtype"], "init");
        assert_eq!(init[0]["permissionMode"], "acceptEdits");
        assert!(init[0]["tools"].as_array().unwrap().iter().any(|tool| tool == "Read"));
        let session = init[0]["session_id"].as_str().unwrap();
        let assistant = of_type(&frames, "assistant");
        let tool_use = assistant.iter().find(|frame| frame["message"]["content"][0]["type"] == "tool_use").expect("tool_use message");
        assert_eq!(tool_use["message"]["content"][0]["name"], "Read");
        assert_eq!(tool_use["message"]["content"][0]["input"]["file_path"], "fixture.txt");
        let results = of_type(&frames, "user");
        assert_eq!(results.len(), 1);
        let block = &results[0]["message"]["content"][0];
        assert_eq!(block["type"], "tool_result");
        // The assistant call and the result use the same occurrence identity.
        assert_eq!(block["tool_use_id"], tool_use["message"]["content"][0]["id"]);
        assert!(block["content"].as_str().unwrap().contains("SDK_READ_SENTINEL"));
        let text = assistant.iter().find(|frame| frame["message"]["content"][0]["type"] == "text").expect("text message");
        assert_eq!(text["message"]["content"][0]["text"], "reply-2");
        assert_ne!(tool_use["message"]["id"], text["message"]["id"], "each model round is its own message");
        for frame in assistant.iter().chain(results.iter()) {
            assert_eq!(frame["session_id"], session);
            assert!(frame["parent_tool_use_id"].is_null());
            assert!(frame["uuid"].is_string());
        }
        let result = of_type(&frames, "result");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["subtype"], "success");
        assert_eq!(result[0]["num_turns"], 2);
        assert_eq!(result[0]["result"], "reply-2");
        assert!(result[0]["duration_ms"].is_u64());
        assert!(result[0]["total_cost_usd"].is_number());
        assert!(of_type(&frames, "stream_event").is_empty());
        // Legacy event lines remain for existing consumers.
        assert!(!of_type(&frames, "tool_start").is_empty());
    }
}

#[tokio::test]
async fn repeated_role_chunks_stay_one_assistant_message() {
    let server = Server::start(false, Mode::RoleEveryChunk).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &[]);
    client.send(user("Think then answer")).await;
    let (status, _, transcript) = client.finish_with_transcript().await;
    assert!(status.success(), "{transcript}");
    let frames = transcript
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect::<Vec<Value>>();
    let assistant = of_type(&frames, "assistant");
    assert!(!assistant.is_empty(), "{frames:?}");
    let id = &assistant[0]["message"]["id"];
    assert!(assistant.iter().all(|frame| &frame["message"]["id"] == id), "{assistant:?}");
    let thinking = assistant
        .iter()
        .filter(|frame| frame["message"]["content"][0]["type"] == "thinking")
        .collect::<Vec<_>>();
    assert_eq!(thinking.len(), 1, "{assistant:?}");
    assert_eq!(thinking[0]["message"]["content"][0]["thinking"], "think-athink-b");
    let text = assistant
        .iter()
        .filter(|frame| frame["message"]["content"][0]["type"] == "text")
        .collect::<Vec<_>>();
    assert_eq!(text.len(), 1, "{assistant:?}");
    assert_eq!(text[0]["message"]["content"][0]["text"], "reply-1");
    let result = of_type(&frames, "result");
    assert_eq!(result.len(), 1, "{frames:?}");
    assert_eq!(result[0]["num_turns"], 1);
}

#[tokio::test]
async fn partial_messages_stream_content_block_events_in_order() {
    for anthropic in [false, true] {
        let frames = transcript(anthropic, &["--include-partial-messages", "--permission-prompt-tool", "stdio"]).await;
        let events = of_type(&frames, "stream_event").into_iter().map(|frame| frame["event"].clone()).collect::<Vec<_>>();
        let kinds = events.iter().map(|event| event["type"].as_str().unwrap()).collect::<Vec<_>>();
        assert_eq!(kinds.iter().filter(|kind| **kind == "message_start").count(), 2, "{kinds:?}");
        assert_eq!(kinds.iter().filter(|kind| **kind == "message_stop").count(), 2, "{kinds:?}");
        let text = events.iter().filter(|event| event["delta"]["type"] == "text_delta").map(|event| event["delta"]["text"].as_str().unwrap()).collect::<String>();
        assert_eq!(text, "reply-2");
        assert!(events.iter().any(|event| event["delta"]["type"] == "input_json_delta"));
        let start = kinds.iter().position(|kind| *kind == "content_block_start").unwrap();
        let stop = kinds.iter().position(|kind| *kind == "content_block_stop").unwrap();
        assert!(start < stop);
    }
}

#[tokio::test]
async fn partial_messages_require_stream_json_output() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args(["-p", "hi", "--include-partial-messages"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--include-partial-messages requires --output-format stream-json"));
}

#[tokio::test]
async fn reference_default_session_label_is_accepted() {
    let server = Server::start(false, Mode::Text).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &[]);
    let mut frame = user("labelled");
    frame["session_id"] = json!("default");
    frame["parent_tool_use_id"] = Value::Null;
    client.send(frame).await;
    assert_eq!(client.result().await["is_error"], false);
    let (status, _) = client.finish().await;
    assert!(status.success());
}

#[tokio::test]
async fn host_denials_are_listed_in_the_result() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Approval).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
        client.send(user("Write the fixture file")).await;
        let request = client.frame("control_request").await;
        client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":{"behavior":"deny","message":"HOST_SAYS_NO"}}})).await;
        let result = client.result().await;
        assert_eq!(result["is_error"], false, "{result}");
        let denial = &result["permission_denials"][0];
        assert_eq!(denial["tool_name"], "Write");
        // The denial and the SDK permission request share one identity.
        assert_eq!(denial["tool_use_id"], request["request"]["tool_use_id"]);
        assert_eq!(denial["tool_input"], json!({"file_path":"approval.txt","content":"ACTUAL_APPROVED_WRITE"}));
        client.finish().await;
    }
}
