use super::*;

#[tokio::test]
async fn running_session_reloads_extensions_on_the_next_request() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    let mcp = paths.data_dir.join("mcp-servers.json");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/live-mcp.cjs");
    let configure = |revision, enabled| {
        std::fs::write(
            &mcp,
            json!({"mcpServers":{"live":{"command":"node", "args":[fixture],
            "env":{"REVISION":revision}, "enabled":enabled}}})
            .to_string(),
        )
        .unwrap();
    };
    std::fs::write(&mcp, r#"{"mcpServers":{}}"#).unwrap();
    let skill = workspace.join(".claude/skills/live-skill/SKILL.md");
    let write_skill = |body| {
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(
            &skill,
            format!("---\nname: live-skill\ndescription: local fixture\n---\n{body}\n"),
        )
        .unwrap();
    };
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state((requests.clone(), workspace.clone()));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let exe = temp.path().join(if cfg!(windows) {
        "reload.exe"
    } else {
        "reload"
    });
    std::fs::copy(test_cli_executable(), &exe).unwrap();
    let host = worker::ensure_worker(&paths.data_dir, &exe, &session, &workspace)
        .await
        .unwrap();
    let _cleanup = Cleanup(vec![host.supervisor_pid.unwrap()]);
    let mut first_user = None;
    for turn in 0..5 {
        match turn {
            1 => {
                configure("initial schema", true);
                write_skill("initial instructions");
            }
            2 => {
                configure("replacement schema", true);
                write_skill("edited instructions");
            }
            3 => {
                std::fs::write(
                    paths.data_dir.join("mcp-states.json"),
                    r#"{"servers":{"live":"on-demand"}}"#,
                )
                .unwrap();
            }
            4 => {
                configure("replacement schema", false);
                std::fs::write(
                    paths.data_dir.join("skill-states.json"),
                    r#"{"skills":{"live-skill":"off"}}"#,
                )
                .unwrap();
            }
            _ => {}
        }
        let job = submit(
            &host,
            &endpoint,
            &format!("check extension {turn}"),
            "fixture",
            128,
        )
        .await;
        let result = completed(&paths.data_dir, &job).await;
        assert_eq!(
            result.status, "completed",
            "turn {turn}: {:?}",
            result.error
        );
        let (_, _, events) = jobs::events_since(&paths.data_dir, &job, &session, 0).unwrap();
        let notices: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "notice")
            .filter_map(|e| e["message"].as_str())
            .collect();
        if let Some(expected) = match turn {
            1 => Some("MCP: live started"),
            2 => Some("MCP: live restarted"),
            4 => Some("MCP: live disabled"),
            _ => None,
        } {
            assert!(
                notices.contains(&expected) && notices.contains(&"Skills reloaded"),
                "turn {turn}: {notices:?}"
            );
        }
        let observed = requests.lock().unwrap();
        assert_eq!(observed.len(), turn + 1);
        let request = &observed[turn];
        let messages = request["messages"].as_array().unwrap();
        let users: Vec<_> = messages.iter().filter(|m| m["role"] == "user").collect();
        assert_eq!(
            first_user.get_or_insert_with(|| users[0].clone()),
            users[0],
            "first user message changed on turn {turn}"
        );
        let latest = users.last().unwrap()["content"].as_str().unwrap();
        let tool = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| &t["function"])
            .find(|f| f["name"] == "mcp__live__probe");
        match turn {
            1 | 2 => {
                assert!(
                    latest.contains(if turn == 1 {
                        "New MCP tools: mcp__live__probe"
                    } else {
                        "Updated MCP tools: mcp__live__probe"
                    }),
                    "{latest}"
                );
                assert_eq!(
                    tool.expect("resident probe")["description"],
                    if turn == 1 {
                        "initial schema"
                    } else {
                        "replacement schema"
                    }
                );
                assert!(
                    latest.contains(if turn == 1 {
                        "New skills: live-skill"
                    } else {
                        "Updated skills: live-skill"
                    }),
                    "{latest}"
                );
            }
            3 => {
                assert!(tool.is_none());
                assert!(
                    latest.contains("Now loadable: mcp__live__probe"),
                    "{latest}"
                );
            }
            4 => {
                assert!(tool.is_none());
                assert!(
                    latest.contains("Removed skills: live-skill")
                        && latest.contains("No longer available: mcp__live__probe"),
                    "{latest}"
                );
            }
            _ => assert!(tool.is_none()),
        }
    }
    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    server.abort();
    let _ = server.await;
}
