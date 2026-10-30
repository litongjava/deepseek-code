//! Idle parking uses real worker IPC without ever submitting a model turn.
use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    services::{
        background_sessions, session_parking, session_worker, EnvironmentCredentials,
        ServiceContext,
    },
};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn idle_park_preserves_history_and_reports_worker_shutdown_honestly() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let context = ServiceContext::open(
        RuntimePaths::new(root.path().join("data")).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap();
    let store = SessionStore::new(context.db.clone());
    let parent = store
        .create(&workspace, "fixture-model", "fixture-provider")
        .unwrap();
    store
        .append(&parent, "user", "Preserve this question")
        .unwrap();
    let child = store.fork(&parent).unwrap();
    let history = store.history(&parent).unwrap();
    let executable = root.path().join(if cfg!(windows) {
        "park-test.exe"
    } else {
        "park-test"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_dcl"), &executable).unwrap();
    for target in [&parent, &child] {
        session_parking::park(&context, &parent, target, &executable)
            .await
            .unwrap();
    }
    // Stop before assertions so a failed assertion cannot leave live workers behind.
    let live = session_parking::list(&context, &parent).await.unwrap();
    let project = deepseek_code_core::services::session_trust::inspect(&context.db, &parent).unwrap();
    context
        .db
        .with_conn(|conn| {
            conn.execute(
                "UPDATE project_folders SET trusted=0 WHERE project_id=?1",
                [&project.id],
            )
        })
        .unwrap()
        .unwrap();
    assert!(session_parking::validate_idle(&context, &parent, &parent).is_err());
    // Revoked trust blocks starting/resuming, but must not trap an idle process.

    for target in [&parent, &child] {
        session_parking::stop(&context, &parent, target)
            .await
            .unwrap();
    }
    assert_eq!(live.len(), 2);
    assert!(live
        .iter()
        .all(|row| row.worker_state == "reachable" && row.activity == "waiting_for_input"));
    assert!(background_sessions::list(&context.paths.data_dir, None)
        .unwrap()
        .is_empty());
    assert_eq!(store.history(&parent).unwrap(), history);
    assert_eq!(
        store
            .history(&child)
            .unwrap()
            .iter()
            .map(|row| row["content"].clone())
            .collect::<Vec<_>>(),
        history
            .iter()
            .map(|row| row["content"].clone())
            .collect::<Vec<_>>()
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let reports = session_parking::list(&context, &parent).await.unwrap();
            if reports.iter().all(|row| row.worker_state != "reachable") {
                assert!(reports
                    .iter()
                    .all(|row| row.activity == "session_saved_worker_unavailable"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        session_parking::registered(&context, &parent)
            .unwrap()
            .len(),
        2
    );
    assert!(
        session_worker::runtime_status(&context.paths.data_dir, &parent)
            .await
            .state
            != "reachable"
    );
}
