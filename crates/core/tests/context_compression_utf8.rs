use deepseek_code_core::native_engine::context_compressor::{CompressionLevel, ContextCompressor};
use serde_json::{json, Value};

async fn check_summary(user: String, assistant: String, expected: &str) {
    let mut messages = Vec::new();
    for _ in 0..8 {
        messages.push(json!({"role": "user", "content": user}));
        messages.push(json!({"role": "assistant", "content": assistant}));
    }
    messages.push(json!({"role": "user", "content": "Latest request"}));
    for level in [
        CompressionLevel::Medium,
        CompressionLevel::Heavy,
        CompressionLevel::Critical,
    ] {
        let (compressed, metadata) = ContextCompressor::new(None)
            .compress(messages.clone(), None, level, None)
            .await;
        assert!(metadata.was_compressed);
        let summary = compressed
            .iter()
            .find(|m| m["_is_summary"] == true)
            .unwrap();
        assert!(summary["content"].as_str().unwrap().contains(expected));
        assert_eq!(compressed.last().unwrap()["content"], "Latest request");
    }
}

#[tokio::test]
async fn assistant_summary_preserves_utf8_at_byte_150() {
    let prefix = "a".repeat(149);
    check_summary(
        "Question".into(),
        format!("{prefix}题{}", "文".repeat(100)),
        &format!("Assistant: {prefix}..."),
    )
    .await;
}

#[tokio::test]
async fn user_summary_preserves_utf8_at_byte_200() {
    let prefix = "a".repeat(199);
    check_summary(
        format!("{prefix}🦀{}", "文".repeat(200)),
        "Answer".into(),
        &format!("User asked: {prefix}..."),
    )
    .await;
}

#[tokio::test]
async fn critical_system_prompt_preserves_utf8_at_byte_1000() {
    let prefix = "a".repeat(999);
    let (compressed, _) = ContextCompressor::new(None)
        .compress(
            vec![json!({"role": "user", "content": "Keep this"})],
            Some(format!("{prefix}界{}", "文".repeat(400))),
            CompressionLevel::Critical,
            None,
        )
        .await;
    assert_eq!(
        compressed[0]["content"],
        format!("{prefix}... [system prompt truncated]")
    );
    assert_eq!(compressed.last().unwrap()["content"], "Keep this");
}

/// Opt-in local replay: read history without running tools, providers, or DB migrations.
#[tokio::test]
#[ignore = "requires COMPRESSION_REPLAY_DB and COMPRESSION_REPLAY_SESSION"]
async fn replay_saved_history_read_only() {
    let db = std::env::var("COMPRESSION_REPLAY_DB").unwrap();
    let session = std::env::var("COMPRESSION_REPLAY_SESSION").unwrap();
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut query = conn
        .prepare("SELECT role,content FROM messages WHERE conversation_id=?1 ORDER BY sort_order")
        .unwrap();
    let messages: Vec<Value> = query
        .query_map([session], |row| {
            Ok(json!({"role": row.get::<_, String>(0)?, "content": row.get::<_, String>(1)?}))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!messages.is_empty());
    for level in [
        CompressionLevel::Light,
        CompressionLevel::Medium,
        CompressionLevel::Heavy,
        CompressionLevel::Critical,
    ] {
        let (compressed, _) = ContextCompressor::new(None)
            .compress(messages.clone(), None, level, None)
            .await;
        assert!(!compressed.is_empty());
        serde_json::to_string(&compressed).unwrap();
    }
}
