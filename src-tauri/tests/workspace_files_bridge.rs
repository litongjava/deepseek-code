use deepseek_code_core::{db::project_repo, runtime::RuntimePaths};
use deepseek_desktop_tauri_lib::bridge::BridgeServer;
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test]
async fn workspace_files_require_auth_trust_and_project_relative_paths() {
    deepseek_desktop_tauri_lib::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = RuntimePaths::new(dir.path().into())
        .unwrap()
        .open_database()
        .unwrap();
    let folder = dir.path().join("source");
    let extra = dir.path().join("extra");
    std::fs::create_dir(&folder).unwrap();
    std::fs::create_dir(&extra).unwrap();
    std::fs::write(extra.join("emoji.txt"), "😀中文\r\n").unwrap();
    let folders = vec![
        folder.to_string_lossy().into_owned(),
        extra.to_string_lossy().into_owned(),
    ];
    db.with_conn(|conn| {
        project_repo::insert_project(
            conn, "project", "Fixture", None, None, None, false, "now", "now",
        )
        .unwrap();
        project_repo::set_folders(conn, "project", &folders).unwrap();
    })
    .unwrap();
    let bridge = BridgeServer::new(dir.path().into(), db.clone());
    let key = bridge.get_api_key().to_string();
    let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let server = tokio::spawn(async move { bridge.start(port).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client.get(format!("{base}/health")).send().await.is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let url = format!("{base}/api/workspace-files");
    let scope = json!({"scope":{"projectId":"project"}});
    assert_eq!(
        client
            .post(format!("{url}/roots"))
            .json(&scope)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(format!("{url}/roots"))
            .header("x-api-key", &key)
            .json(&scope)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    db.with_conn(|conn| project_repo::trust_folders(conn, "project", &folders))
        .unwrap()
        .unwrap();
    let roots: Value = client
        .post(format!("{url}/roots"))
        .header("x-api-key", &key)
        .json(&scope)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(roots["roots"].as_array().unwrap().len(), 2);
    let search: Value = client
        .post(format!("{url}/search"))
        .header("x-api-key", &key)
        .json(&json!({"scope":{"projectId":"project"},"query":"中文"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(search["matches"][0]["rootId"], "root-1");
    assert_eq!(search["matches"][0]["startColumn"], 3);
    assert_eq!(client.post(format!("{url}/read")).header("x-api-key", &key).json(&json!({"scope":{"projectId":"project"},"rootId":"root-0","relativePath":"../extra/emoji.txt"})).send().await.unwrap().status(), reqwest::StatusCode::BAD_REQUEST);
    let read: Value = client
        .post(format!("{url}/read"))
        .header("x-api-key", &key)
        .json(
            &json!({"scope":{"projectId":"project"},"rootId":"root-1","relativePath":"emoji.txt"}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(read["content"], "😀中文\r\n");
    server.abort();
    let _ = server.await;
}
