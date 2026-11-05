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
    let dir = worker::directory(&old);
    std::fs::write(dir.join("worker.json"), [0; 478]).unwrap();
    assert!(worker::recover_session(&paths.data_dir, &session).is_err());
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
    let recovered = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match worker::recover_session(&paths.data_dir, &session) {
                Ok(recovered) => break recovered,
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
    })
    .await
    .expect("the orphaned host must exit by itself");
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
