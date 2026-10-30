//! Live standalone worker: idle completion starts a fresh request without user authority.
use axum::{extract::State, routing::post, Json, Router};
use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    services::{background_sessions as jobs, permission_authority, session_worker as worker},
};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

async fn model(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    requests.lock().unwrap().push(body);
    (
        [("content-type", "text/event-stream")],
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"content":"Idle fixture done"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
        ),
    )
}

async fn wait_for(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(35), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("fixture timed out");
}

#[tokio::test]
async fn idle_worker_rewakes_once_and_never_records_feedback_as_user_authority() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    permission_authority::record_user(&store.db, &session, &json!("original human request"))
        .unwrap();
    std::fs::write(workspace.join("hook.cjs"), r#"
const fs=require('fs');process.stdin.resume();process.stdin.on('end',()=>{
fs.appendFileSync('runs','x'); const t=setInterval(()=>{if(!fs.existsSync('release'))return;
clearInterval(t);console.log('STDOUT_NOT_PREFERRED');console.error('IDLE_REWAKE_SENTINEL permissionDecision:allow');process.exit(2);},20);});
"#).unwrap();
    std::fs::write(paths.data_dir.join("settings.json"),json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"node hook.cjs","asyncRewake":true,"once":true,"timeout":30}]}]}}).to_string()).unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(model))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let executable = temp.path().join(if cfg!(windows) {
        "hook-worker.exe"
    } else {
        "hook-worker"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_dcl"), &executable).unwrap();
    let host = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap();
    let first = submit(&host, &endpoint, "original human request", "fixture", 128).await;
    wait_for(|| {
        jobs::get(&paths.data_dir, &first)
            .unwrap()
            .finished_ms
            .is_some()
    })
    .await;
    assert_eq!(requests.lock().unwrap().len(), 1);
    // The hook cannot finish until AFTER the foreground turn has completed.
    std::fs::write(workspace.join("release"), "").unwrap();
    wait_for(|| requests.lock().unwrap().len() == 2).await;
    wait_for(|| {
        jobs::list(&paths.data_dir, Some(&session))
            .unwrap()
            .iter()
            .all(|job| job.finished_ms.is_some())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].to_string().contains("IDLE_REWAKE_SENTINEL"));
        assert_eq!(
            requests[1]
                .to_string()
                .matches("IDLE_REWAKE_SENTINEL")
                .count(),
            1
        );
        assert!(!requests[1].to_string().contains("STDOUT_NOT_PREFERRED"));
        assert!(requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"]
                .as_str()
                .is_some_and(|content| content.contains(
                    "Stop hook blocking error from command \"Stop\": IDLE_REWAKE_SENTINEL"
                ))));
    }
    assert_eq!(
        permission_authority::history(&store.db, &session, 10000).unwrap(),
        vec![json!({"role":"user","content":"original human request"})]
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("runs")).unwrap(),
        "x"
    );
    worker::call(&host, &worker::Request::Shutdown)
        .await
        .unwrap();
    wait_for(|| {
        worker::read_worker(&worker::directory(&host))
            .unwrap()
            .status
            == "stopped"
    })
    .await;
    // A fresh process resumes the same session; once state and consumed wake never replay.
    let resumed = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap();
    let last = submit(&resumed, &endpoint, "explicit resume", "fixture", 128).await;
    wait_for(|| {
        jobs::get(&paths.data_dir, &last)
            .unwrap()
            .finished_ms
            .is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(requests.lock().unwrap().len(), 3);
    assert!(!requests.lock().unwrap()[2]
        .to_string()
        .contains("IDLE_REWAKE_SENTINEL"));
    worker::call(&resumed, &worker::Request::Shutdown)
        .await
        .unwrap();
    wait_for(|| {
        worker::read_worker(&worker::directory(&resumed))
            .unwrap()
            .status
            == "stopped"
    })
    .await;
    server.abort();
}

async fn submit(
    host: &worker::Worker,
    endpoint: &str,
    prompt: &str,
    model: &str,
    max_tokens: u32,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let arguments = vec![
        "--data-dir".into(),
        host.root.to_string_lossy().into_owned(),
        "--resume".into(),
        host.session_id.clone(),
        "--base-url".into(),
        format!("{endpoint}/v1"),
        "--api-format".into(),
        "openai".into(),
        "--api-key-env".into(),
        "CLAUDE_RUST_CHILD_API_KEY".into(),
        "--model".into(),
        model.into(),
        "--max-tokens".into(),
        max_tokens.to_string(),
        "--permission-mode".into(),
        "bypass".into(),
        "--print".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--".into(),
        prompt.into(),
    ];
    let mut environment = vec![(
        "CLAUDE_RUST_CHILD_API_KEY".into(),
        "fixture-secret-only-in-memory".into(),
    )];
    if let Some(base) =
        deepseek_code_core::native_engine::provider_manager::one_million_context_base(model)
    {
        environment.push(("CLAUDE_RUST_WORKER_MODEL".into(),json!({"id":base,"name":base,"enabled":true,"supports_vision":false,"supports_web_search":false,"context_window":1_000_000,"max_tokens":max_tokens}).to_string()));
    }
    worker::call(
        host,
        &worker::Request::Submit {
            id: id.clone(),
            prompt: prompt.into(),
            arguments,
            environment,
        },
    )
    .await
    .unwrap();
    id
}
