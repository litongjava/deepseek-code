use super::*;

#[tokio::test]
async fn power_loss_metadata_damage_allows_new_request_without_replay() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state((requests.clone(), temp.path().join("unused.js")));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let executable = test_cli_executable();
    let old = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap();
    let mut cleanup = Cleanup(vec![old.supervisor_pid.unwrap()]);
    let old_job = submit(&old, &endpoint, "slow interrupted request", "fixture", 128).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while requests.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    // Corruption while the executor is alive must not create a second executor.
    // The live supervisor rewrites worker.json every heartbeat, so this damage
    // can already be repaired by the time recovery looks: both "refused because
    // the record is damaged" and "nothing to recover, the record is valid again"
    // are correct. What must never happen is reporting a recovery while the host
    // that owns the session is still running.
    let dir = worker::directory(&old);
    std::fs::write(dir.join("worker.json"), [0; 478]).unwrap();
    let refusal = worker::recover_session(&paths.data_dir, &session);
    assert!(
        !matches!(refusal, Ok(true)),
        "recovery reported success while the executor was alive: {refusal:?} \
         supervisor_pid={:?} child_pid={:?}",
        old.supervisor_pid,
        old.child_pid
    );
    assert!(
        !worker::process_exited(old.child_pid.unwrap()),
        "the first turn's host must still be running when recovery is refused"
    );
    drop(Cleanup(vec![old.supervisor_pid.unwrap()]));
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(dir.join("worker.json"), [0; 478]).unwrap();
    std::fs::write(dir.join("active.json"), [0; 38]).unwrap();
    std::fs::write(
        paths
            .data_dir
            .join("background-tasks")
            .join(&old_job)
            .join("state.json"),
        [0; 409],
    )
    .unwrap();
    std::fs::write(dir.parent().unwrap().join("current.json"), [0; 38]).unwrap();
    // A Unix host keeps its own process group, so it outlives the supervisor
    // until it notices the lost owner and exits by itself. Recovery stays refused
    // until then rather than starting a second executor.
    let deadline = Duration::from_secs(30);
    let mut last_recover_error = None;
    let recovered = tokio::time::timeout(deadline, async {
        loop {
            match worker::recover_session(&paths.data_dir, &session) {
                Ok(recovered) => break recovered,
                Err(error) => {
                    last_recover_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the orphaned host must exit by itself: last_recover_error={last_recover_error:?} \
             supervisor_pid={:?} child_pid={:?}",
            old.supervisor_pid, old.child_pid
        )
    });
    assert!(recovered);
    let fresh = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap();
    cleanup.0.push(fresh.supervisor_pid.unwrap());
    assert_ne!(fresh.id, old.id);
    let next = submit(&fresh, &endpoint, "new question", "fixture", 128).await;
    assert_eq!(completed(&paths.data_dir, &next).await.status, "completed");
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "old request must not be replayed"
    );
    assert_eq!(
        std::fs::read(dir.join("worker.json")).unwrap(),
        vec![0; 478]
    );
    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    server.abort();
}
