// Context-status regressions: a request proves which context it used by the
// boundary it recorded, overhead must be fully measurable, and a legacy branch
// never claims its ancestor's compactions.
use deepseek_code_core::{
    db::message_repo,
    runtime::{RuntimePaths, SessionStore},
    services::{compaction, context_usage},
};
use serde_json::json;

fn fixture() -> (tempfile::TempDir, SessionStore, String, String) {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::new(
        RuntimePaths::new(temp.path().join("data"))
            .unwrap()
            .open_database()
            .unwrap(),
    );
    let id = store
        .create(temp.path(), "fixture-model", "fixture")
        .unwrap();
    store.append(&id, "user", "Task").unwrap();
    let message = store.append(&id, "assistant", "Result").unwrap();
    (temp, store, id, message)
}

#[test]
fn unknown_prompt_overhead_must_not_be_reported_as_complete() {
    let (_temp, store, id, message) = fixture();
    let estimate = |unknown| json!({"serialized_utf8_bytes":300,"estimated_tokens":100,"unknown_parts":unknown});
    let usage = json!({"version":1,"history":estimate(0),"instructions":estimate(1),"tools":estimate(0),"output_schema":estimate(0),
        "configured_context_window":128000,"requested_output_budget":1000,"sent_output_reserve":1000,"estimated_input_tokens":null});
    store
        .db
        .with_conn(|c| {
            message_repo::save_request_snapshots(
                c,
                &message,
                &json!([{"source":"native_engine","providerId":"fixture","model":"fixture-model","startedAt":1,"contextUsage":usage}]),
            )
        })
        .unwrap()
        .unwrap();
    compaction::commit(
        &store.db,
        compaction::prepare(&store.db, &id).unwrap(),
        "summary",
    )
    .unwrap();
    let estimated =
        context_usage::estimated_after_compaction(&store, &id, "fixture-model", Some("fixture"))
            .unwrap()
            .unwrap();
    assert!(
        !estimated.complete,
        "Unknown instruction overhead is discarded: {estimated:?}"
    );
}

#[test]
fn pre_boundary_request_in_same_millisecond_must_not_be_current() {
    let (_temp, store, id, message) = fixture();
    compaction::commit(
        &store.db,
        compaction::prepare(&store.db, &id).unwrap(),
        "summary",
    )
    .unwrap();
    let boundary = context_usage::latest_boundary(&store.db, &id)
        .unwrap()
        .unwrap();
    // A pre-boundary message with a late-persisted request, started earlier in
    // the same millisecond, and without the boundary it never used.
    store
        .db
        .with_conn(|c| {
            message_repo::save_request_snapshots(
                c,
                &message,
                &json!([{"source":"native_engine","providerId":"fixture","model":"fixture-model","startedAt":boundary.created_at_ms,"usage":{"prompt_tokens":90000}}]),
            )
        })
        .unwrap()
        .unwrap();
    let current = context_usage::last_request_since(&store.db, &id, &boundary.id).unwrap();
    assert!(
        current.is_none(),
        "Pre-boundary request is accepted by timestamp equality: {current:?}"
    );
}

#[test]
fn legacy_branch_must_not_inherit_parent_compaction_count() {
    let (_temp, store, id, _message) = fixture();
    compaction::commit(
        &store.db,
        compaction::prepare(&store.db, &id).unwrap(),
        "summary",
    )
    .unwrap();
    let fork = store.fork(&id).unwrap();
    // Simulate a branch created before the count key was introduced.
    store
        .db
        .with_conn(|c| {
            c.execute(
                "DELETE FROM peripheral_settings WHERE key=?1",
                [format!("compaction.count.{fork}")],
            )
        })
        .unwrap()
        .unwrap();
    let count = compaction::count(&store.db, &fork).unwrap();
    assert_eq!(
        count,
        compaction::CompactionCount::LegacyAfterUpgrade(0),
        "Copied ancestor boundaries are not branch executions"
    );
    assert_eq!(count.proven(), 0);
    assert!(
        count.to_string().contains("legacy history"),
        "an unprovable total must be labelled: {count}"
    );
}
