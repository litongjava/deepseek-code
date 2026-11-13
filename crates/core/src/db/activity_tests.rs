//! Durable conversation activity, with a real SQLite database and no credentials.
use super::SCHEMA_SQL;
use crate::db::{conversation_repo as conversations, message_repo as messages, DbManager};
use rusqlite::Connection;

const OLD: &str = "2000-01-01T00:00:00+00:00";
fn fixture() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA_SQL).unwrap();
    crate::services::migrate(&conn).unwrap();
    conversations::insert_conversation(
        &conn, "session", None, None, None, None, None, false, false, false, OLD, OLD, 0,
    )
    .unwrap();
    conn
}
fn timestamp(conn: &Connection) -> String {
    conversations::get_conversation(conn, "session")
        .unwrap()
        .unwrap()
        .updated_at
}
fn reset_clock_fixture(conn: &Connection) {
    conn.execute(
        "UPDATE conversations SET updated_at=?1 WHERE id='session'",
        [OLD],
    )
    .unwrap();
}
fn assert_activity(conn: &Connection, write: impl FnOnce()) {
    reset_clock_fixture(conn);
    let before: String = conn
        .query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    write();
    let updated = chrono::DateTime::parse_from_rfc3339(&timestamp(conn)).unwrap();
    assert!(updated >= chrono::DateTime::parse_from_rfc3339(&before).unwrap());
}
fn message(conn: &Connection) {
    messages::insert_message(
        conn,
        "assistant",
        "session",
        "assistant",
        "",
        None,
        OLD,
        false,
        0,
    )
    .unwrap();
}

#[test]
fn actual_message_tool_and_multimodal_changes_refresh_activity() {
    let conn = fixture();
    assert_activity(&conn, || message(&conn));
    assert_activity(&conn, || {
        messages::update_message_content(&conn, "assistant", "answer").unwrap()
    });
    assert_activity(&conn, || {
        conn.execute(
            "UPDATE messages SET thinking='reasoning' WHERE id='assistant'",
            [],
        )
        .unwrap();
    });
    assert_activity(&conn, || {
        messages::insert_tool_call(
            &conn,
            "tool",
            "assistant",
            "Read",
            Some("{}"),
            None,
            false,
            0,
        )
        .unwrap()
    });
    assert_activity(&conn, || {
        messages::insert_tool_call(
            &conn,
            "tool",
            "assistant",
            "Read",
            Some("{}"),
            Some("result"),
            true,
            0,
        )
        .unwrap()
    });
    assert_activity(&conn, || {
        conn.execute(
            "INSERT INTO message_user_content VALUES('assistant','[]')",
            [],
        )
        .unwrap();
    });
    assert_activity(&conn, || {
        conn.execute("UPDATE message_user_content SET content_json='[1]'", [])
            .unwrap();
    });
    assert_activity(&conn, || {
        conn.execute("DELETE FROM message_user_content", [])
            .unwrap();
    });
    assert_activity(&conn, || {
        conn.execute("DELETE FROM tool_calls WHERE id='tool'", [])
            .unwrap();
    });
    assert_activity(&conn, || {
        messages::delete_message(&conn, "assistant").unwrap()
    });
}

#[test]
fn snapshots_reads_heartbeats_and_schema_reopen_do_not_refresh_activity() {
    let conn = fixture();
    message(&conn);
    messages::insert_tool_call(
        &conn,
        "tool",
        "assistant",
        "Read",
        Some("{}"),
        Some("result"),
        false,
        0,
    )
    .unwrap();
    reset_clock_fixture(&conn);
    messages::update_message_content(&conn, "assistant", "").unwrap();
    messages::insert_tool_call(
        &conn,
        "tool",
        "assistant",
        "Read",
        Some("{}"),
        Some("result"),
        false,
        0,
    )
    .unwrap();
    messages::get_messages_by_conversation(&conn, "session").unwrap();
    conversations::list_conversations(&conn).unwrap();
    conn.execute(
        "INSERT INTO peripheral_settings(key,value) VALUES('worker-heartbeat','1')",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE peripheral_settings SET value='2' WHERE key='worker-heartbeat'",
        [],
    )
    .unwrap();
    conn.execute_batch(SCHEMA_SQL).unwrap();
    assert_eq!(timestamp(&conn), OLD);
}

#[test]
fn activity_rolls_back_with_failed_content_and_handles_interrupted_turns() {
    let conn = fixture();
    message(&conn);
    reset_clock_fixture(&conn);
    {
        let tx = crate::db::write_transaction(&conn).unwrap();
        messages::update_message_content(&tx, "assistant", "rolled back").unwrap();
        assert_ne!(timestamp(&tx), OLD);
        // Drop rather than commit, as with a failed tool-origin write.
    }
    assert_eq!(timestamp(&conn), OLD);
    assert_eq!(
        messages::get_message(&conn, "assistant")
            .unwrap()
            .unwrap()
            .content,
        ""
    );
    conn.execute("INSERT INTO execution_turns(id,conversation_id,started_at,status) VALUES('turn','session',?1,'running')", [OLD]).unwrap();
    assert_activity(&conn, || {
        crate::db::turn_repo::settle_interrupted(&conn, "session").unwrap();
    });
    reset_clock_fixture(&conn);
    assert!(crate::db::turn_repo::settle_interrupted(&conn, "session")
        .unwrap()
        .is_empty());
    assert_eq!(timestamp(&conn), OLD);
}

#[test]
fn older_activity_never_overwrites_a_newer_timestamp_even_below_a_millisecond() {
    let conn = fixture();
    let future = "2999-01-01T00:00:00.123456789+00:00";
    conversations::update_conversation_timestamp(&conn, "session", future).unwrap();
    message(&conn);
    conversations::update_conversation_timestamp(
        &conn,
        "session",
        "2999-01-01T00:00:00.123456788Z",
    )
    .unwrap();
    conversations::update_conversation_timestamp(
        &conn,
        "session",
        "2998-12-31T19:00:00.123456788-05:00",
    )
    .unwrap();
    conversations::update_conversation_title(&conn, "session", "renamed").unwrap();
    conversations::increment_message_count(&conn, "session").unwrap();
    assert_eq!(timestamp(&conn), future);
    let later = "2999-01-01T00:00:00.123456790Z";
    conversations::update_conversation_timestamp(&conn, "session", later).unwrap();
    assert_eq!(timestamp(&conn), later);
}

#[test]
fn competing_database_connections_keep_the_newer_activity() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("activity.sqlite3");
    let db = DbManager::new(path.clone()).unwrap();
    db.init().unwrap();
    db.with_conn(|conn| {
        conversations::insert_conversation(
            conn, "session", None, None, None, None, None, false, false, false, OLD, OLD, 0,
        )
    })
    .unwrap()
    .unwrap();
    let db = std::sync::Arc::new(db);
    let (committed, wait) = std::sync::mpsc::channel();
    let newer = db.clone();
    let writer = std::thread::spawn(move || {
        newer
            .with_conn(|conn| {
                conversations::update_conversation_timestamp(
                    conn,
                    "session",
                    "2999-01-01T00:10:00Z",
                )
            })
            .unwrap()
            .unwrap();
        committed.send(()).unwrap();
    });
    // This writer computed its candidate before the first committed.
    let stale_candidate = "2999-01-01T00:00:00Z";
    let stale_connection = Connection::open(path).unwrap();
    wait.recv().unwrap();
    conversations::update_conversation_timestamp(&stale_connection, "session", stale_candidate)
        .unwrap();
    writer.join().unwrap();
    assert_eq!(db.with_conn(timestamp).unwrap(), "2999-01-01T00:10:00Z");
}
