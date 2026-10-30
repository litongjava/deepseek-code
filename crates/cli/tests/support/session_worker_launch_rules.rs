use super::*;

#[tokio::test]
async fn persistent_worker_refreshes_launch_rules_without_retaining_previous_grants() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release_model = Arc::new(tokio::sync::Notify::new());
    let app = Router::new().route("/v1/chat/completions", post({
        let count = count.clone();
        let release_model = release_model.clone();
        move || {
            let release_model = release_model.clone();
            let first = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) % 2 == 0;
            async move {
                if first { release_model.notified().await; }
                let (delta, reason) = if first {
                    (json!({"tool_calls":[{"index":0,"id":"launch-write","type":"function","function":{"name":"Write","arguments":"{\"file_path\":\"launch.txt\",\"content\":\"allowed\"}"}}]}), "tool_calls")
                } else { (json!({"content":"done"}), "stop") };
                ([("content-type", "text/event-stream")], format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}), json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]})))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let executable = temp.path().join(if cfg!(windows) {
        "launch-worker.exe"
    } else {
        "launch-worker"
    });
    std::fs::copy(test_cli_executable(), &executable).unwrap();
    let host = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap();
    let _cleanup = Cleanup(vec![host.supervisor_pid.unwrap()]);
    for (index, mode, rule, expected) in [
        (0, "bypass", None, true),
        (1, "bypass", Some("--disallowedTools"), false),
        (2, "ask", Some("--allowed-tools"), true),
        (3, "ask", None, false),
    ] {
        let file = workspace.join("launch.txt");
        if file.exists() {
            std::fs::remove_file(&file).unwrap();
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut arguments = vec![
            "--data-dir".into(),
            paths.data_dir.to_string_lossy().into_owned(),
            "--resume".into(),
            session.clone(),
            "--base-url".into(),
            endpoint.clone(),
            "--api-format".into(),
            "openai".into(),
            "--api-key-env".into(),
            "CLAUDE_RUST_CHILD_API_KEY".into(),
            "--model".into(),
            "fixture".into(),
            "--permission-mode".into(),
            mode.into(),
            "--print".into(),
            "--output-format".into(),
            "stream-json".into(),
        ];
        if let Some(rule) = rule {
            arguments.extend([rule.into(), "Write".into()]);
        }
        arguments.extend(["--".into(), format!("launch turn {index}")]);
        worker::call(
            &host,
            &worker::Request::Submit {
                id: id.clone(),
                prompt: format!("launch turn {index}"),
                arguments,
                environment: vec![("CLAUDE_RUST_CHILD_API_KEY".into(), "fixture-only".into())],
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while count.load(std::sync::atomic::Ordering::SeqCst) != index * 2 + 1 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let snapshot = worker::permission_snapshot(&paths.data_dir, &session, &id)
            .await
            .unwrap();
        assert_eq!(snapshot.job_id.as_deref(), Some(id.as_str()));
        assert_eq!(
            snapshot.permission_session_id.as_deref(),
            Some(session.as_str())
        );
        assert_eq!(snapshot.permission_mode.as_deref(), Some(mode));
        let launch = snapshot.launch_rules.unwrap();
        match rule {
            Some("--disallowedTools") => assert_eq!(
                launch.get("Write"),
                Some(&deepseek_code_core::permissions::rules::PermissionAction::Deny)
            ),
            Some("--allowed-tools") => assert_eq!(
                launch.get("Write"),
                Some(&deepseek_code_core::permissions::rules::PermissionAction::Allow)
            ),
            None => assert!(launch.is_empty()),
            _ => unreachable!(),
        }
        assert!(
            worker::permission_snapshot(&paths.data_dir, &session, "another-job")
                .await
                .is_err()
        );
        release_model.notify_one();
        if index == 3 {
            let pending = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(question) =
                        jobs::pending_question(&paths.data_dir, &id, &session).unwrap()
                    {
                        break question;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert!(!file.exists());
            jobs::answer_question(&paths.data_dir, &id, &session, &pending.request_id, "n")
                .unwrap();
        }
        // Declining the approval stops that turn, as in the reference CLI.
        let status = if index == 3 { "cancelled" } else { "completed" };
        assert_eq!(completed(&paths.data_dir, &id).await.status, status);
        assert_eq!(file.exists(), expected, "turn {index}");
    }
    // The declined turn ends without asking the model to continue.
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 7);
    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    server.abort();
}
