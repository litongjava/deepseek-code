use super::*;
use axum::http::header;
use deepseek_code_core::native_engine::{
    engine_core::ReasoningMode, provider_manager::ResolvedProvider, tool_loop::ToolLoopExecutor,
};

async fn pressure_request(messages: Vec<Value>, window: usize) -> Vec<Value> {
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let app=Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>| {capture.lock().unwrap().push(body); async {
        ([(header::CONTENT_TYPE,"text/event-stream")],"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
    }}));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let provider=ResolvedProvider{
            provider:serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":url,"api_format":"openai","api_key":"fixture","enabled":true,"models":[]})).unwrap(),
            model:serde_json::from_value(json!({"id":"fixture","name":"fixture","context_window":window,"enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap()};
    let (tx, _rx) = tokio::sync::mpsc::channel(100);
    let mut runner = ToolLoopExecutor::new(
        provider,
        messages.clone(),
        None,
        128,
        tx,
        root.path().to_str().unwrap().into(),
        ReasoningMode::Standard,
    )
    .with_search_config_path(root.path().join("config/config.json"))
    .with_conv_id("review".into());
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), runner.execute())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.0, "done");
    let body = seen.lock().unwrap().last().unwrap().clone();
    let array = body["messages"].as_array().unwrap();
    let result = array.clone();
    server.abort();
    let _ = server.await;
    result
}

#[tokio::test]
async fn base64_model_boundary_light_pressure() {
    let data = format!("{}!{}!", ".".repeat(3900), "QUJD".repeat(256));
    let invalid = "Q".repeat(4097);
    let messages = vec![
        json!({"role":"user","content":"check results"}),
        json!({"role":"assistant","content":"","tool_calls":[{"id":"marker","type":"function","function":{"name":"Bash","arguments":"{}"}},{"id":"invalid","type":"function","function":{"name":"Bash","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"marker","content":data}),
        json!({"role":"tool","tool_call_id":"invalid","content":invalid}),
        json!({"role":"user","content":"continue"}),
    ];
    for window in [200_000, 4452] {
        let sent = pressure_request(messages.clone(), window).await;
        let result = |id: &str| {
            sent.iter().find(|m| m["tool_call_id"] == id).unwrap()["content"]
                .as_str()
                .unwrap()
        };
        assert!(result("marker").contains(", line 1]"));
        let run = result("invalid").bytes().take_while(|b| *b == b'Q').count();
        assert_eq!(run, if window == 200_000 { 4097 } else { 3999 });
    }
}

#[tokio::test]
async fn base64_model_boundary_progressive_pressure() {
    let payload = "QUJD".repeat(256);
    let raw = json!({"stdout":format!("fn {payload} ()!\n{}", ".".repeat(90_000))}).to_string();
    let mut messages = vec![
        json!({"role":"user","content":"first"}),
        json!({"role":"assistant","content":"","tool_calls":[{"id":"old","type":"function","function":{"name":"Bash","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"old","content":raw}),
    ];
    for _ in 0..12 {
        messages.extend([
            json!({"role":"user","content":"keep going"}),
            json!({"role":"assistant","content":"step done"}),
        ]);
    }
    messages.push(json!({"role":"user","content":"continue"}));
    let window = (TokenCounter::estimate(&messages) as f64 / 0.9) as usize;
    let sent = pressure_request(messages, window).await;
    assert!(!json!(sent).to_string().contains(&payload));
}
#[tokio::test]
async fn base64_review_ordinary_light_cut_matches_main() {
    let messages = vec![
        json!({"role":"user","content":"check results"}),
        json!({"role":"assistant","content":"","tool_calls":[{"id":"marker","type":"function","function":{"name":"Bash","arguments":"{}"}},{"id":"invalid","type":"function","function":{"name":"Bash","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"marker","content":json!({"stdout":".".repeat(4926)}).to_string()}),
        json!({"role":"tool","tool_call_id":"invalid","content":json!({"stdout":"Q".repeat(4097)}).to_string()}),
        json!({"role":"user","content":"continue"}),
    ];
    let sent = pressure_request(messages, 4841).await;
    let result = sent
        .iter()
        .find(|m| m["tool_call_id"] == "invalid")
        .unwrap();
    assert_eq!(
        result["content"],
        format!("{}... [truncated, 4097 bytes total]", "Q".repeat(3999))
    );
}

#[tokio::test]
async fn base64_review_ordinary_progressive_budget_matches_main() {
    let raw = json!({"stdout":format!("fn plain_symbol ()!\n{}", "normal line.\n".repeat(7000)), "stderr":"","exit_code":0}).to_string();
    let mut messages = vec![
        json!({"role":"user","content":"first"}),
        json!({"role":"assistant","content":"","tool_calls":[{"id":"old","type":"function","function":{"name":"Bash","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"old","content":raw}),
    ];
    for _ in 0..12 {
        messages.extend([
            json!({"role":"user","content":"keep going"}),
            json!({"role":"assistant","content":"step done"}),
        ]);
    }
    messages.push(json!({"role":"user","content":"continue"}));
    let sent = pressure_request(messages, 39360).await;
    assert_eq!(sent.len(), 21);
    assert!(sent.iter().all(|m| m["role"] != "tool"));
    assert!(sent
        .iter()
        .any(|m| m["role"] == "system" && m.to_string().contains("plain_symbol")));
}

#[tokio::test]
async fn base64_review_ordinary_legacy_image_matches_main() {
    let f = fixture(0).await;
    let message = f.store.append(&f.id, "assistant", "").unwrap();
    let id = format!("{message}:image");
    let content = json!({"content":[{"type":"text","text":"plain ordinary line.\n".repeat(1200)},{"type":"image","mimeType":"image/png","data":PNG}]});
    let display = f
        .store
        .db
        .open_session(&f.id)
        .unwrap()
        .save_mcp(&message, "image", &content, false)
        .unwrap();
    f.store.db.shard_sql(&f.id, |conn| {
        deepseek_code_core::db::message_repo::insert_tool_call(conn, &id, &message, "mcp__example__capture", None, Some(&display), false, 0).unwrap();
        conn.execute_batch("UPDATE tool_calls SET model_checked=0, model_output=NULL, model_preview=NULL, model_recovery=NULL, model_search=NULL; UPDATE mcp_tool_outputs SET model_content_json=NULL").unwrap();
    }).unwrap();
    f.store.append(&f.id, "user", "continue").unwrap();
    for format in [ApiFormat::Anthropic, ApiFormat::OpenAI] {
        let history = f.store.model_history(&f.id, &format).unwrap();
        assert!(
            json!(history).to_string().contains(PNG),
            "{format:?} replay"
        );
    }
    let history = f.store.model_history(&f.id, &ApiFormat::OpenAI).unwrap();
    let req=serde_json::from_value(json!({"conversation_id":f.id,"messages":history,"model":"gpt-6-astra","provider_id":"gateway","workspace_path":f.dir.path()})).unwrap();
    let mut events = f.engine.send_message(req).await.unwrap();
    while let Some(event) = events.recv().await {
        assert!(!format!("{event:?}").starts_with("Error"), "{event:?}");
    }
    let wire = f.seen.lock().unwrap().last().unwrap().to_string();
    assert!(wire.contains(PNG));
    assert!(!wire.contains("base64 data omitted"));
    f.server.abort();
    let _ = f.server.await;
}

use deepseek_code_core::super_agent::{
    crp::ToolRuntime, model::TaskSpec, review, store::TaskStore,
};

fn assigned_reviewer(f: &Fixture) -> ToolRuntime {
    let reviewer = f.store.create(f.dir.path(), "fixture", "fixture").unwrap();
    let router = f.store.create(f.dir.path(), "fixture", "fixture").unwrap();
    let data_dir = f.store.db.data_dir().to_owned();
    let tasks = TaskStore::new(f.store.db.clone(), data_dir.clone());
    let spec: TaskSpec = serde_json::from_value(
        json!({"title":"Review", "workspace":f.dir.path(), "goal":"Inspect session evidence"}),
    )
    .unwrap();
    let task = tasks.create_task(&router, spec, "create").unwrap();
    tasks
        .set_sessions(&task.task_id, Some(&f.id), Some(&reviewer))
        .unwrap();
    ToolRuntime {
        db: f.store.db.clone(),
        data_dir,
        session_id: reviewer,
    }
}

async fn reviewer_wire(f: &Fixture, runtime: &ToolRuntime) -> Value {
    let evidence = review::execute(runtime, "SessionMessages", json!({"session_id":f.id})).unwrap();
    let reply = evidence.to_string();
    let text = reviewer_page_on_wire(&reply).await;
    assert_eq!(text, reply);
    serde_json::from_str(&text).unwrap()
}

async fn reviewer_page_on_wire(reply: &str) -> String {
    let sent = pressure_request(vec![
        json!({"role":"assistant","content":"","tool_calls":[{"id":"review","type":"function","function":{"name":"SessionMessages","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"review","content":reply}),
        json!({"role":"user","content":"continue"}),
    ], 200000).await;
    let text = sent.iter().find(|m| m["tool_call_id"] == "review").unwrap()["content"]
        .as_str()
        .unwrap();
    text.to_owned()
}

#[tokio::test]
async fn base64_review_session_messages_on_provider_wire() {
    let f = fixture(0).await;
    let payload = "QUJD".repeat(256);
    let raw = json!({"stdout":format!("log!{payload}!"),"stderr":"","exit_code":0}).to_string();
    let msg = f.store.append(&f.id, "assistant", "").unwrap();
    f.store
        .db
        .shard_sql(&f.id, |c| {
            deepseek_code_core::db::message_repo::insert_tool_call(
                c,
                "qualifying",
                &msg,
                "Bash",
                Some("{}"),
                Some(&raw),
                false,
                0,
            )
        })
        .unwrap()
        .unwrap();
    let evidence = reviewer_wire(&f, &assigned_reviewer(&f)).await;
    let message = evidence["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == msg)
        .unwrap();
    let output = message["tool_calls"][0]["output"].as_str().unwrap();
    assert!(output.contains("base64 data omitted") && !output.contains(&payload));
    f.server.abort();
    let _ = f.server.await;
}

/// These expected strings are shared with the main-revision control. Each consumer
/// has its own representation: history and native compaction keep JSON, I6 renders stdout.
#[tokio::test]
async fn base64_review_ordinary_consumer_wire_matrix() {
    let f = fixture(0).await;
    let shard = f.store.db.open_session(&f.id).unwrap();
    let msg = f.store.append(&f.id, "assistant", "").unwrap();
    let raw = r#"{"stdout":"ordinary-marker!","stderr":"","exit_code":0}"#;
    let cases = [
        ("bash", "Bash", "ordinary-marker!"),
        ("task", "TaskGet", raw),
    ];
    for (i, (id, name, _)) in cases.iter().enumerate() {
        f.store
            .db
            .shard_sql(&f.id, |c| {
                deepseek_code_core::db::message_repo::insert_tool_call(
                    c,
                    id,
                    &msg,
                    name,
                    Some("{}"),
                    Some(raw),
                    false,
                    i as i64,
                )
            })
            .unwrap()
            .unwrap();
    }
    f.store.append(&f.id, "user", "continue").unwrap();
    let history = f.store.model_history(&f.id, &ApiFormat::OpenAI).unwrap();
    for item in history.iter().filter(|m| m["role"] == "tool") {
        assert_eq!(item["content"], raw);
    }
    let sent = pressure_request(history, 200000).await;
    let hits: Value = serde_json::from_str(
        &shard
            .context_search(json!({"query":"ordinary-marker","role":"tool"}))
            .unwrap(),
    )
    .unwrap();
    for (id, _, preview) in cases {
        let tool = sent.iter().find(|m| m["tool_call_id"] == id).unwrap();
        assert_eq!(tool["content"], preview, "Chat Completions {id}");
        let hit = hits["matches"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["tool_call_id"] == id)
            .unwrap();
        assert_eq!(hit["excerpt"], preview, "ContextSearch {id}");
    }
    let runtime = assigned_reviewer(&f);
    let evidence = reviewer_wire(&f, &runtime).await;
    let calls = evidence["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == msg)
        .unwrap()["tool_calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|c| c["output"] == raw));
    compaction::compact(&f.engine, &f.store.db, &f.id, "gateway", "gpt-6-astra", "")
        .await
        .unwrap();
    let body = f.seen.lock().unwrap().last().unwrap().clone();
    let outputs: Vec<_> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .map(|i| &i["output"])
        .collect();
    assert_eq!(outputs, vec![&json!(raw); 2]);
    f.server.abort();
    let _ = f.server.await;
}

#[tokio::test]
async fn base64_model_boundary_returned_window_and_replay() {
    let f = fixture(7).await;
    let payload = "QUJD".repeat(256);
    let large = "Detailed evidence ".repeat(2200);
    let first = vec![
        json!({"role":"user","content":"First"}),
        json!({"role":"assistant","content":large}),
        json!({"role":"user","content":"continue"}),
    ];
    let mut previous = None;
    for label in ["auto", "replay"] {
        let messages = if label == "auto" {
            first.clone()
        } else {
            compaction::compact(&f.engine, &f.store.db, &f.id, "gateway", "gpt-6-astra", "")
                .await
                .unwrap();
            f.store.append(&f.id, "user", "continue").unwrap();
            f.store.model_history(&f.id, &ApiFormat::OpenAI).unwrap()
        };
        let req=serde_json::from_value(json!({"conversation_id":f.id,"messages":messages,"model":"gpt-6-astra","provider_id":"gateway","max_tokens":1000,"workspace_path":f.dir.path()})).unwrap();
        let mut events = f.engine.send_message(req).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(12), async {
            while let Some(event) = events.recv().await {
                assert!(!format!("{event:?}").starts_with("Error"), "{event:?}");
            }
        })
        .await
        .unwrap();
        let req = f.seen.lock().unwrap().last().unwrap().clone();
        let items = req["input"].as_array().unwrap();
        let result = items
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap()["output"]
            .as_str()
            .unwrap();
        let notice = items
            .iter()
            .filter(|i| i["role"] == "user")
            .flat_map(|i| i["content"].as_array().into_iter().flatten())
            .find_map(|p| p["text"].as_str().filter(|s| s.contains("<kind>monitor")))
            .unwrap();
        assert!(
            !result.contains(&payload) && !notice.contains(&payload),
            "{label}"
        );
        assert!(items
            .iter()
            .any(|i| i["encrypted_content"] == "opaque exact content"));
        let normalize = |text: String| text.replace(r"\\?\", "").replace('\\', "/");
        let observed = (normalize(result.to_owned()), normalize(notice.to_owned()));
        if let Some(prior) = previous.replace(observed.clone()) {
            assert_eq!(prior, observed);
        }
    }
    f.server.abort();
    let _ = f.server.await;
}

#[tokio::test]
async fn base64_model_boundary_typed_responses() {
    let f = fixture(0).await;
    let payload = "QUJD".repeat(256);
    let messages = json!([{"role":"assistant","content":"","tool_calls":[{"id":"array","type":"function","function":{"name":"Bash","arguments":"{}"}}]}, {"role":"tool","tool_call_id":"array","content":[{"type":"text","text":format!("header\n{payload}\n")}]},{"role":"user","content":"continue"}]);
    let req=serde_json::from_value(json!({"conversation_id":f.id,"messages":messages,"model":"gpt-6-astra","provider_id":"gateway","workspace_path":f.dir.path()})).unwrap();
    let mut events = f.engine.send_message(req).await.unwrap();
    while let Some(event) = events.recv().await {
        assert!(!format!("{event:?}").starts_with("Error"));
    }
    let body = f.seen.lock().unwrap()[0].clone();
    assert!(body.to_string().contains("base64 data omitted"));
    assert!(!body.to_string().contains(&payload));
    f.server.abort();
    let _ = f.server.await;
}

#[tokio::test]
async fn base64_model_boundary_summary_newline() {
    let f = fixture(0).await;
    let payload = "QUJD".repeat(256);
    let raw =
        json!({"stdout":format!("header\n{payload}\n"),"stderr":"","exit_code":0}).to_string();
    let msg = f.store.append(&f.id, "assistant", "").unwrap();
    let add = |id: &str, name: &str, text: &str| {
        f.store
            .db
            .shard_sql(&f.id, |c| {
                deepseek_code_core::db::message_repo::insert_tool_call(
                    c,
                    id,
                    &msg,
                    name,
                    Some("{}"),
                    Some(text),
                    false,
                    0,
                )
            })
            .unwrap()
            .unwrap()
    };
    add("nl", "Bash", &raw);
    f.store.append(&f.id, "user", "summarize").unwrap();

    let _ = compaction::summarize_part(
        &f.engine,
        &f.store.db,
        &f.id,
        &msg,
        compaction::Part::From,
        "gateway",
        "gpt-6-astra",
    )
    .await;
    let summary_request = f.seen.lock().unwrap().last().unwrap().clone();
    assert!(!summary_request.to_string().contains(&payload));
    assert!(summary_request.to_string().contains("base64 data omitted"));
    compaction::compact(&f.engine, &f.store.db, &f.id, "gateway", "gpt-6-astra", "")
        .await
        .unwrap();
    let req = f.seen.lock().unwrap().last().unwrap().clone();
    assert!(!req.to_string().contains(&payload) && req.to_string().contains("base64 data omitted"));
    f.server.abort();
    let _ = f.server.await;
}

#[tokio::test]
async fn base64_model_boundary_search_chunk() {
    let f = fixture(0).await;
    let payload = "QUJD".repeat(256);
    let msg = f.store.append(&f.id, "assistant", "").unwrap();
    let add = |id: &str, name: &str, text: &str| {
        f.store
            .db
            .shard_sql(&f.id, |c| {
                deepseek_code_core::db::message_repo::insert_tool_call(
                    c,
                    id,
                    &msg,
                    name,
                    Some("{}"),
                    Some(text),
                    false,
                    0,
                )
            })
            .unwrap()
            .unwrap()
    };
    let find = |name: &str, q: &str| -> Value {
        serde_json::from_str(
            &f.store
                .db
                .open_session(&f.id)
                .unwrap()
                .context_search(json!({"tool_name":name,"query":q}))
                .unwrap(),
        )
        .unwrap()
    };
    let header = r#"{"output_mode":"page","stdout":""#;
    let keyword = "boundary-sign!";
    let chunk = 8 * 1024 * 1024;
    let frame = format!(
        "{header}{}{keyword}{payload}Z!\"}}",
        ".".repeat(chunk - 1024 - header.len() - keyword.len())
    );
    assert!(serde_json::from_str::<Value>(&frame).is_ok());
    assert!(frame[..chunk].ends_with(&payload));
    add("legacy-fragment", "TaskGet", &frame);
    let hit = find("TaskGet", keyword);
    let excerpt = hit["matches"][0]["excerpt"].as_str().unwrap();
    assert!(excerpt.contains("boundary-sign!QUJD") && !excerpt.contains("base64 data omitted"));
    f.server.abort();
    let _ = f.server.await;
}

#[cfg(unix)]
#[test]
fn base64_model_boundary_search_recovery_paths() {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir().unwrap();
    for (label, directory) in [
        ("control", std::ffi::OsString::from("normal")),
        ("comma", std::ffi::OsString::from("data, line west")),
        (
            "non_utf8",
            std::ffi::OsString::from_vec(b"review-\xff".to_vec()),
        ),
    ] {
        let root = temp.path().join(directory);
        let paths = RuntimePaths::new(root.clone()).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(temp.path(), "fixture", "local").unwrap();
        let msg = store.append(&id, "assistant", "").unwrap();
        let raw = format!("anchor!{}!{}!", ".".repeat(35), "QUJD".repeat(256));
        store
            .db
            .shard_sql(&id, |c| {
                deepseek_code_core::db::message_repo::insert_tool_call(
                    c,
                    "call",
                    &msg,
                    "TaskGet",
                    Some("{}"),
                    Some(&raw),
                    false,
                    0,
                )
            })
            .unwrap()
            .unwrap();
        let hit: Value = serde_json::from_str(
            &store
                .db
                .open_session(&id)
                .unwrap()
                .context_search(json!({"query":"anchor!","tool_name":"TaskGet"}))
                .unwrap(),
        )
        .unwrap();
        let excerpt = hit["matches"][0]["excerpt"].as_str().unwrap();
        assert!(excerpt.contains(", line 1]"), "{label} {excerpt}");
        let path = hit["matches"][0]["recovery_path"].as_str().unwrap();
        assert!(excerpt.contains(path));
        let bytes = if label == "non_utf8" {
            // Bash ANSI-C quoting round-trips the actual path bytes.
            assert!(path.starts_with("$'") && path.ends_with('\''));
            let encoded = path.trim_start_matches("$'").trim_end_matches('\'');
            let bytes = encoded
                .strip_prefix("\\x")
                .unwrap()
                .split("\\x")
                .map(|hex| u8::from_str_radix(hex, 16).unwrap())
                .collect::<Vec<_>>();
            std::fs::read(std::ffi::OsString::from_vec(bytes)).unwrap()
        } else {
            std::fs::read(path).unwrap()
        };
        assert_eq!(bytes, raw.as_bytes());
    }
}

fn insert_review_result(f: &Fixture, message: &str, id: &str, position: i64, stdout: &str) {
    let raw = json!({"stdout":stdout,"stderr":"","exit_code":0}).to_string();
    f.store
        .db
        .shard_sql(&f.id, |conn| {
            deepseek_code_core::db::message_repo::insert_tool_call(
                conn,
                id,
                message,
                "Bash",
                Some("{}"),
                Some(&raw),
                false,
                position,
            )
        })
        .unwrap()
        .unwrap();
}

async fn assert_reviewer_preview(page: &str, count: usize, payload: &str) {
    let wire = reviewer_page_on_wire(page).await;
    assert!(!wire.contains(payload));
    assert!(wire.len() <= 16_000);
    let preview = wire.rsplit_once("\nSaved tool result").unwrap().0;
    let (head, rest) = preview.split_once("\n... (output truncated; ").unwrap();
    let (notice, tail) = rest.split_once(") ...\n").unwrap();
    assert!(page.starts_with(head) && page.ends_with(tail));
    let tail_start = page.len() - tail.len();
    let mut omitted = 0;
    let markers: Vec<_> = page.match_indices("[base64 data omitted:").collect();
    assert_eq!(markers.len(), count);
    for (start, _) in markers {
        let end = start + page[start..].find(']').unwrap() + 1;
        assert!(
            !(start < head.len() && head.len() < end),
            "head splits reference"
        );
        assert!(
            !(start < tail_start && tail_start < end),
            "tail splits reference"
        );
        omitted += usize::from(start >= head.len() && end <= tail_start);
    }
    if count == 1 {
        assert_eq!(
            omitted, 0,
            "the single recovery reference fits in the preview"
        );
    }
    let reported = notice
        .split_once("; placeholders omitted: ")
        .map_or(0, |(_, count)| count.parse::<usize>().unwrap());
    assert_eq!(reported, omitted, "{notice}");
}

#[tokio::test]
async fn base64_reviewer_preview_three_results() {
    let f = fixture(0).await;
    let reviewer = assigned_reviewer(&f);
    let message = f.store.append(&f.id, "assistant", "").unwrap();
    let payload = "QUJD".repeat(256);
    let page = || {
        review::execute(&reviewer, "SessionMessages", json!({"session_id":f.id}))
            .unwrap()
            .to_string()
    };
    insert_review_result(&f, &message, "first", 0, "");
    insert_review_result(&f, &message, "second", 1, &format!("log!{payload}!"));
    insert_review_result(&f, &message, "third", 2, &".".repeat(12_000));
    // Allow for the serialized IDs and temporary directory in the default page.
    let padding = 7770 - page().find("[base64 data omitted:").unwrap();
    insert_review_result(&f, &message, "first", 0, &".".repeat(padding));
    let page = page();
    let (path, rest) = page
        .split_once("; full text: ")
        .unwrap()
        .1
        .split_once(", line ")
        .unwrap();
    let line: usize = rest.split_once(']').unwrap().0.parse().unwrap();
    let path = serde_json::from_str::<String>(&format!("\"{path}\"")).unwrap();
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    assert!(std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .nth(line - 1)
        .unwrap()
        .contains(&payload));
    f.server.abort();
    let _ = f.server.await;
    assert_reviewer_preview(&page, 1, &payload).await;
}

#[tokio::test]
async fn base64_reviewer_preview_default_page_with_100_references() {
    let f = fixture(0).await;
    let reviewer = assigned_reviewer(&f);
    let payload = "QUJD".repeat(256);
    for i in 0..4 {
        let message = f.store.append(&f.id, "assistant", "").unwrap();
        let stdout = (0..25)
            .map(|j| format!("asset {i}.{j} = {payload}"))
            .collect::<Vec<_>>()
            .join("\n");
        insert_review_result(&f, &message, &format!("result-{i}"), 0, &stdout);
    }
    let page = review::execute(&reviewer, "SessionMessages", json!({"session_id":f.id}))
        .unwrap()
        .to_string();
    f.server.abort();
    let _ = f.server.await;
    assert_reviewer_preview(&page, 100, &payload).await;
}
