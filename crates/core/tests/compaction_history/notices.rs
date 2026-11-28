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
