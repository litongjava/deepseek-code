use super::Fixture;
use deepseek_code_core::db::message_repo::insert_tool_call;
use serde_json::{json, Value};

#[path = "../support/session_sql.rs"]
mod session_sql;
use session_sql::FixtureSql;

fn store_result(
    f: &Fixture,
    message: &str,
    id: &str,
    name: &str,
    input: Value,
    output: &str,
    position: i64,
) -> anyhow::Result<()> {
    f.store.db.shard_sql(&f.id, |db| {
        insert_tool_call(
            db,
            id,
            message,
            name,
            Some(&input.to_string()),
            Some(output),
            false,
            position,
        )
    })?
}

fn tool_output(body: &Value) -> &str {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap()
}

#[tokio::test]
async fn recent_pwd_has_the_same_bytes_in_normal_and_text_summary_requests() -> anyhow::Result<()> {
    let f = Fixture::new(128_000).await;
    f.store.append_prompt(&f.id, "Check this", "typed")?;
    let message = f.store.append(&f.id, "assistant", "")?;
    let result = json!({"stdout":"recent-line\n","stderr":"","exit_code":0,"success":true,
        "shell":"bash","stdout_truncated":false,"stderr_truncated":false});
    store_result(
        &f,
        &message,
        "shell",
        "Bash",
        json!({"command":"pwd"}),
        &result.to_string(),
        0,
    )?;
    f.store.append_prompt(&f.id, "Continue", "typed")?;
    f.send_normal(None).await;
    f.compact().await?;
    {
        let requests = f.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(tool_output(&requests[0]).as_bytes(), b"recent-line\n");
        assert_eq!(
            tool_output(&requests[0]).as_bytes(),
            tool_output(&requests[1]).as_bytes()
        );
    }
    f.finish().await;
    Ok(())
}

#[tokio::test]
async fn peer_before_dispatch_recovers_the_live_window() -> anyhow::Result<()> {
    use deepseek_code_core::native_engine::token_counter::TokenCounter;
    let mut f = Fixture::new(128_000).await;
    f.store.append_prompt(&f.id, "Read this", "typed")?;
    let message = f.store.append(&f.id, "assistant", "")?;
    store_result(
        &f,
        &message,
        "current-read",
        "Read",
        json!({"file_path":"log"}),
        &"long line\n".repeat(7000),
        0,
    )?;
    let history = f.store.model_history(&f.id, &f.provider.api_format)?;
    f.provider.models[0].context_window =
        Some((TokenCounter::estimate(&history) + 4096 + 1024) as u32);
    let sibling = f.store.fork(&f.id)?;
    f.store.db.open_session(&sibling)?.send_peer(
        &f.store.db.open_session(&f.id)?,
        "live-steer",
        &"continued status ".repeat(700),
    )?;
    f.send_normal(None).await;
    let routes = f.routes.lock().unwrap().clone();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].1["x-dcl-request-kind"], "turn");
    assert!(f.requests.lock().unwrap()[0]
        .to_string()
        .contains("continued status"));
    f.finish().await;
    Ok(())
}

#[tokio::test]
async fn monitor_output_and_exit_both_reach_compaction() -> anyhow::Result<()> {
    use deepseek_code_core::tools::monitor::Event;
    let f = Fixture::new(128_000).await;
    f.append("user", "Watch build");
    let output = "error E0815: missing crate\nRemaining: rerun link stage";
    let exit = "Monitor exited with code 7 after 0m 1s (1 events)";
    for (kind, text) in [("output", output), ("exited", exit)] {
        let event = Event {
            task_id: "build".into(),
            description: "build --release".into(),
            kind: kind.into(),
            text: text.into(),
        };
        f.store
            .append_prompt(&f.id, &event.notification(), "task_notification")?;
    }
    f.append("assistant", "Received.");
    f.append("user", "Next topic");
    f.append("assistant", "Acknowledged.");
    f.compact().await?;
    {
        let requests = f.requests.lock().unwrap();
        let messages = requests[0]["messages"].as_array().unwrap();
        assert!(messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .any(|s| s.contains("quoted task data") && s.contains(output)));
        assert!(messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .any(|s| s.contains(exit)));
    }
    f.finish().await;
    Ok(())
}

#[tokio::test]
async fn parallel_tool_receipts_choose_the_latest_delivery() -> anyhow::Result<()> {
    use deepseek_code_core::services::input_queue::{QueueMode, QueueOrigin};
    let f = Fixture::new(128_000).await;
    f.append("user", "Run parallel reads");
    let message = f.store.append(&f.id, "assistant", "")?;
    for (i, id) in ["read-a", "read-b"].into_iter().enumerate() {
        store_result(
            &f,
            &message,
            id,
            "Read",
            json!({"file_path":"log"}),
            "valid",
            i as i64,
        )?;
    }
    let shard = f.store.db.open_session(&f.id)?;
    for state in ["OBSOLETE_STATE", "LATEST_STATE"] {
        shard.enqueue_cli_input(QueueOrigin::TaskNotification, &format!(
            "<task-notification>\n<task-id>one</task-id>\n<summary>{state}</summary>\n</task-notification>"
        ))?;
    }
    {
        let mut owner = shard.delivery_owner()?;
        for target in ["read-b", "read-a"] {
            assert!(owner.next_queue(QueueMode::Steer, Some(target))?.is_some());
        }
    }
    f.append("user", "Continue");
    f.append("assistant", "Reviewed.");
    let history = serde_json::to_string(&f.store.model_history(&f.id, &f.provider.api_format)?)?;
    assert!(history.find("LATEST_STATE").unwrap() < history.find("OBSOLETE_STATE").unwrap());
    f.compact().await?;
    let wire = f.requests.lock().unwrap()[0]["messages"].to_string();
    assert_eq!(wire.matches("LATEST_STATE").count(), 1);
    assert!(!wire.contains("OBSOLETE_STATE"));
    f.finish().await;
    Ok(())
}

fn later_turns(f: &Fixture) {
    for i in 0..10 {
        f.append("user", &format!("Continue {i}"));
        f.append("assistant", "Reviewed.");
    }
}

#[tokio::test]
async fn an_old_todo_read_keeps_the_full_in_progress_item() -> anyhow::Result<()> {
    let f = Fixture::new(128_000).await;
    f.append("user", "Review existing tasks");
    let message = f.store.append(&f.id, "assistant", "")?;
    let content = format!("Reconcile open invoices {}", " detail".repeat(160));
    let todos = json!({"todos":[{"content":content, "status":"in_progress"}]}).to_string();
    assert_eq!(todos.len(), 1193);
    store_result(&f, &message, "todos", "TodoRead", json!({}), &todos, 0)?;
    later_turns(&f);
    f.compact().await?;
    assert_eq!(tool_output(&f.requests.lock().unwrap()[0]), todos);
    f.finish().await;
    Ok(())
}

#[tokio::test]
async fn an_unsearchable_write_diff_keeps_the_normal_write_result() -> anyhow::Result<()> {
    let f = Fixture::new(128_000).await;
    f.append("user", "Write replacement");
    let message = f.store.append(&f.id, "assistant", "")?;
    let raw = json!({"result":"written","file_change":{"verified":true,"rows":[
        {"kind":"delete","text":format!("ORIGINAL_REMOVED_LINE {}", "before ".repeat(180))}
    ]}})
    .to_string();
    store_result(
        &f,
        &message,
        "old-write",
        "Write",
        json!({"file_path":"config.txt","content":"replacement"}),
        &raw,
        0,
    )?;
    later_turns(&f);
    let boundary = f.compact().await?;
    assert_eq!(
        tool_output(&f.requests.lock().unwrap()[0]),
        r#"{"result":"written"}"#
    );
    let found: Value = serde_json::from_str(&f.store.db.open_session(&f.id)?.context_search(json!({
        "query":"ORIGINAL_REMOVED_LINE","role":"tool","range":"before","boundary_id":boundary.message_id
    }))?)?;
    assert_eq!(found["matches"], json!([]));
    let original: String = f.store.db.shard_sql(&f.id, |c| {
        c.query_row(
            "SELECT output FROM tool_calls WHERE id='old-write'",
            [],
            |row| row.get(0),
        )
    })??;
    assert_eq!(original, raw);
    f.finish().await;
    Ok(())
}

#[tokio::test]
async fn a_tool_attached_delivery_is_not_superseded_by_an_unordered_direct_notice(
) -> anyhow::Result<()> {
    use deepseek_code_core::services::input_queue::{QueueMode, QueueOrigin};
    let f = Fixture::new(128_000).await;
    f.append("user", "Read the log");
    let message = f.store.append(&f.id, "assistant", "")?;
    store_result(
        &f,
        &message,
        "read",
        "Read",
        json!({"file_path":"log"}),
        "valid",
        0,
    )?;
    let notice = |summary| {
        format!("<task-notification>\n<task-id>one</task-id>\n<summary>{summary}</summary>\n</task-notification>")
    };
    f.store
        .append_prompt(&f.id, &notice("DIRECT_UPDATE"), "task_notification")?;
    let shard = f.store.db.open_session(&f.id)?;
    shard.enqueue_cli_input(QueueOrigin::TaskNotification, &notice("LATER_DELIVERY"))?;
    assert!(shard
        .delivery_owner()?
        .next_queue(QueueMode::Steer, Some("read"))?
        .is_some());
    f.append("user", "Continue");
    f.append("assistant", "Reviewed.");
    f.compact().await?;
    let wire = f.requests.lock().unwrap()[0]["messages"].to_string();
    for state in ["DIRECT_UPDATE", "LATER_DELIVERY"] {
        assert_eq!(wire.matches(state).count(), 1);
    }
    f.finish().await;
    Ok(())
}
