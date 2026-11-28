//! HTTP approval over the real dcl stream-json protocol. A different page on
//! every call prevents the page cache from hiding a missing or duplicate fetch.
use super::*;
use axum::{extract::OriginalUri, routing::get};
use std::sync::atomic::{AtomicUsize, Ordering};

const SCOPES: [&str; 4] = ["http_once", "http_session", "http_project", "http_user"];

struct HttpFixture {
    origin: String,
    hits: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl HttpFixture {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let pages = hits.clone();
        let base = origin.clone();
        let turn = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route("/page/{id}", get(move |OriginalUri(uri): OriginalUri| {
            pages.lock().unwrap().push(uri.path().to_owned());
            async move { format!("SDK_PAGE_BODY:{}", uri.path()) }
        })).route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let messages = body["messages"].as_array().unwrap();
            let fetch = messages.last().unwrap()["role"] == "user" && body["stream"] == true;
            let delta = if fetch {
                let n = turn.fetch_add(1, Ordering::SeqCst) + 1;
                json!({"tool_calls":[{"index":0,"id":"reused-fetch-id","type":"function","function":{"name":"WebFetch","arguments":json!({"url":format!("{base}/page/{n}"),"prompt":"Return the page text"}).to_string()}}]})
            } else { json!({"content":"done"}) };
            async move {
                if body["stream"] != true {
                    return Json(json!({"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]})).into_response();
                }
                let chunk = json!({"choices":[{"index":0,"delta":delta,"finish_reason":if fetch {"tool_calls"} else {"stop"}}]});
                ([("content-type","text/event-stream")], format!("data: {chunk}\n\ndata: [DONE]\n\n")).into_response()
            }
        }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { origin, hits, task }
    }

    fn client(&self, root: &Path, args: &[&str]) -> Client {
        Client::launch(root, Some((&self.origin, "openai")), args)
    }

    async fn prompt(&self, client: &mut Client, page: usize) -> Value {
        let pending = client.frame("control_request").await;
        let req = &pending["request"];
        assert_eq!(req["subtype"], "can_use_tool");
        assert_eq!(req["tool_name"], "WebFetchHttp", "{pending}");
        assert_eq!(req["httpScopes"], json!(SCOPES));
        assert_eq!(req["input"]["origin"], self.origin);
        assert_eq!(req["input"]["local"], true);
        assert_eq!(req["input"]["url"], format!("{}/page/{page}", self.origin));
        assert!(!self.hits.lock().unwrap().contains(&format!("/page/{page}")));
        pending
    }
}

fn reply(pending: &Value, decision: Value) -> Value {
    json!({"type":"control_response","response":{"subtype":"success","request_id":pending["request_id"],"response":decision}})
}

async fn completed(client: &mut Client, page: usize, allowed: bool) -> Value {
    let mut results = vec![];
    loop {
        let f = client.next_frame().await;
        assert_ne!(
            f["type"], "control_request",
            "unexpected additional prompt: {f}"
        );
        if f["type"] == "user" {
            results.extend(
                f["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == "tool_result")
                    .cloned(),
            );
        }
        if f["type"] == "result" {
            assert_eq!(f["is_error"], false, "{f}");
            assert_eq!(results.len(), 1, "{results:?}");
            assert_eq!(
                results[0]["is_error"].as_bool().unwrap_or(false),
                !allowed,
                "{results:?}"
            );
            if allowed {
                assert!(
                    results[0]
                        .to_string()
                        .contains(&format!("SDK_PAGE_BODY:/page/{page}")),
                    "{results:?}"
                );
            }
            return f;
        }
    }
}

async fn scope_case(scope: &str) {
    let root = tempfile::tempdir().unwrap();
    let fixture = HttpFixture::start().await;
    let mut client = fixture.client(root.path(), &[]);
    client.send(user("Fetch the page")).await;
    let first = fixture.prompt(&mut client, 1).await;
    client
        .send(reply(&first, json!({"behavior":"allow","httpScope":scope})))
        .await;
    completed(&mut client, 1, true).await;
    let mut expected = vec!["/page/1".to_owned()];
    let other = root.path().join("another-project");
    std::fs::create_dir(&other).unwrap();
    for page in 2..=4 {
        let reused = match page {
            2 => scope != "http_once",
            3 => matches!(scope, "http_project" | "http_user"),
            _ => scope == "http_user",
        };
        if page > 2 {
            let (status, extras) = client.finish().await;
            assert!(
                status.success() && extras.is_empty(),
                "{status}: {extras:?}"
            );
            let other_args = ["--path", other.to_str().unwrap()];
            client = fixture.client(root.path(), if page == 4 { &other_args } else { &[] });
        }
        client.send(user("Fetch another page")).await;
        if !reused {
            let next = fixture.prompt(&mut client, page).await;
            assert_ne!(next["request_id"], first["request_id"]);
            client.send(reply(&next, json!({"behavior":"deny"}))).await;
        } else {
            expected.push(format!("/page/{page}"));
        }
        completed(&mut client, page, reused).await;
        assert_eq!(
            *fixture.hits.lock().unwrap(),
            expected,
            "{scope} page {page}"
        );
    }
    let (status, extra) = client.finish().await;
    assert!(status.success() && extra.is_empty());
}

#[tokio::test]
async fn once() {
    scope_case("http_once").await;
}
#[tokio::test]
async fn session() {
    scope_case("http_session").await;
}
#[tokio::test]
async fn project() {
    scope_case("http_project").await;
}
#[tokio::test]
async fn user_scope() {
    scope_case("http_user").await;
}

#[tokio::test]
async fn explicit_deny_and_invalid_allows_never_fetch_or_grant() {
    let root = tempfile::tempdir().unwrap();
    let fixture = HttpFixture::start().await;
    let mut client = fixture.client(root.path(), &[]);
    for (i, decision) in [json!({"behavior":"deny"}),json!({"behavior":"allow"}),json!({"behavior":"allow","httpScope":"invalid"}),json!({"behavior":"allow","httpScope":"http_user","updatedInput":{"url":"http://localhost"}}),json!({"behavior":"allow","httpScope":"http_user","updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"WebFetch"}]}]}),json!({"behavior":"deny"})].into_iter().enumerate() {
        client.send(user("Fetch the page")).await;
        let pending = fixture.prompt(&mut client, i+1).await;
        client.send(reply(&pending, decision)).await;
        completed(&mut client, i+1, false).await;
        assert!(fixture.hits.lock().unwrap().is_empty());
    }
    let (status, extras) = client.finish().await;
    assert!(status.success() && extras.is_empty());
}

async fn cancelled(interrupt: bool) {
    let root = tempfile::tempdir().unwrap();
    let fixture = HttpFixture::start().await;
    let mut client = fixture.client(root.path(), &[]);
    client.send(user("Fetch the page")).await;
    let pending = fixture.prompt(&mut client, 1).await;
    let result = if interrupt {
        client.send(json!({"type":"control_request","request_id":"stop","request":{"subtype":"interrupt"}})).await;
        assert_eq!(
            client.frame("control_cancel_request").await["request_id"],
            pending["request_id"]
        );
        assert_eq!(
            client.frame("control_response").await["response"]["request_id"],
            "stop"
        );
        let result = client.result().await;
        client
            .send(reply(
                &pending,
                json!({"behavior":"allow","httpScope":"http_user"}),
            ))
            .await;
        client.send(user("Try fetching again")).await;
        let retry = fixture.prompt(&mut client, 2).await;
        assert_ne!(retry["request_id"], pending["request_id"]);
        client.send(reply(&retry, json!({"behavior":"deny"}))).await;
        completed(&mut client, 2, false).await;
        let (status, extra) = client.finish().await;
        assert!(!status.success() && extra.is_empty());
        result
    } else {
        let (status, results) = client.finish().await;
        assert!(!status.success());
        assert_eq!(results.len(), 1);
        results[0].clone()
    };
    assert_eq!(result["is_error"], true);
    assert!(fixture.hits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn eof_without_approver() {
    cancelled(false).await;
}
#[tokio::test]
async fn interrupt_then_late_answer() {
    cancelled(true).await;
}
