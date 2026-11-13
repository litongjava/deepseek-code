//! Bounded recovery when a summary hits its output limit, against local fixtures.
use axum::{extract::DefaultBodyLimit, routing::post, Json, Router};
use deepseek_code_core::{
    cache::FileCache,
    native_engine::{
        provider_manager::{Provider, ProviderManager},
        request_context::ContextUsage,
        NativeEngine,
    },
    runtime::{self, RuntimePaths, SessionStore},
    services::compaction,
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const FRAGMENT_PROMPT: &str = "Summarize this consecutive fragment of the supplied conversation";
const MERGE_PROMPT: &str = "Merge these summaries of consecutive conversation fragments";
const FINAL_PROMPT: &str = "Write one continuation summary of the conversation above";

/// How the fixture answers: 0 finished, 1 output limit reached, 2 other stop.
#[derive(Clone, Copy)]
enum Behavior {
    /// Truncate inputs above this many tokens; smaller inputs finish.
    TruncateAbove(u64),
    /// Truncate requests whose configured output budget is below this value.
    /// A request already using the selected model's cap finishes.
    TruncateBelowBudget(u64),
    /// Truncate like `TruncateAbove`, answering with a 32k-character summary.
    LongSummaryAbove(u64),
    /// Truncate like `TruncateAbove`, answering with the widest summary the
    /// character limit accepts, so merging two of them can exceed the
    /// single-request input ceiling even inside a very large window.
    WideSummaryAbove(u64),
    /// Always report the output limit.
    AlwaysTruncate,
    /// Truncate every request carrying this marker, including the children a
    /// split produces, so one fragment spends its own allowance and fails.
    TruncateContaining(&'static str),
    /// Truncate only the very first request at its configured budget.
    TruncateFirst,
    /// Truncate only the first merge request; the split recovery finishes.
    TruncateMergeOnce,
    /// Truncate every merge until the flag is cleared, so a retry can reuse
    /// the completed fragment summaries.
    TruncateMergeSwitch,
    /// Never truncate; answer every request with a wide summary, so merge
    /// groups are bounded by the input limit instead of by the group size.
    LongSummaryAlways,
    /// Answer the very first request with an empty body, then finish.
    EmptyFirst,
    /// Answer every request with an empty body.
    EmptyAlways,
    /// Answer empty bodies for inputs above this many tokens; smaller inputs
    /// finish, so the retry and then the split can make progress.
    EmptyAbove(u64),
    /// Answer the first request for an input above this many tokens with an
    /// empty body; the same-budget retry finishes.
    EmptyOnceAbove(u64),
    /// Answer empty bodies for every request carrying this marker, including
    /// the children a split produces, so one fragment spends its own allowance.
    EmptyContaining(&'static str),
    /// Answer inputs above this many tokens with 32,001 characters, one over
    /// the limit; smaller inputs finish.
    OversizedAbove(u64),
}

/// The stage a request body belongs to, from the prompt it carries.
fn stage_of(body: &str) -> &'static str {
    if body.contains(FRAGMENT_PROMPT) {
        "fragment"
    } else if body.contains(MERGE_PROMPT) {
        "merge"
    } else if body.contains(FINAL_PROMPT) {
        "final"
    } else {
        "other"
    }
}

#[derive(Clone, Copy)]
enum History {
    /// Small history that fits one final summary.
    Short,
    /// Marked halves whose order the merge must preserve.
    Marked,
    /// Repeated evidence that must be fragmented.
    Evidence(usize),
    /// Distinct lines, so equal-sized fragments never share a cache key.
    Unique(usize),
    /// One identifiable block that fills the first planned fragment, followed
    /// by distinct notes, so a fixture can fail exactly that fragment.
    Poisoned { poison: usize, rest: usize },
}

struct Config {
    format: &'static str,
    window: u32,
    max_tokens: u32,
    behavior: Behavior,
    history: History,
    /// Milliseconds each request holds before answering, to widen race windows.
    delay: u64,
    /// Return a non-truncation stop at this request number; 0 disables it.
    stop_at: usize,
}

impl Config {
    fn new(
        format: &'static str,
        window: u32,
        max_tokens: u32,
        behavior: Behavior,
        history: History,
    ) -> Self {
        Self {
            format,
            window,
            max_tokens,
            behavior,
            history,
            delay: 0,
            stop_at: 0,
        }
    }
}

/// One request the fixture answered, with the facts the new policy asserts on.
#[derive(Clone, Debug, PartialEq)]
struct Sent {
    stage: &'static str,
    /// `original` for a planned fragment, `recovery-child` for a half a
    /// truncation produced, empty for other stages.
    origin: &'static str,
    tokens: u64,
    budget: u64,
    truncated: bool,
}

struct Fixture {
    dir: tempfile::TempDir,
    store: SessionStore,
    id: String,
    engine: Option<NativeEngine>,
    requests: Arc<Mutex<Vec<Value>>>,
    sent: Arc<Mutex<Vec<Sent>>>,
    stop_at: Arc<AtomicUsize>,
    hang_at: Arc<AtomicUsize>,
    /// 1 while `SplitThenMergeSwitch` still truncates merges.
    merge_truncate: Arc<AtomicUsize>,
    config: Config,
    base_url: String,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    fn engine(&self) -> &NativeEngine {
        self.engine.as_ref().unwrap()
    }

    /// Close the engine and reopen the database and engine from the same data
    /// directory, as after a restart. The previous connection is no longer used.
    fn reopen(&mut self) -> (SessionStore, NativeEngine) {
        self.engine = None;
        let paths = RuntimePaths::new(self.dir.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let engine = runtime::create_engine(
            &paths,
            store.db.clone(),
            Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
                vec![make_provider(
                    self.config.format,
                    self.base_url.clone(),
                    self.config.window,
                    self.config.max_tokens,
                )],
                paths.data_dir.join("providers.json"),
            ))),
            runtime::default_permissions(),
            Arc::new(FileCache::new()),
        );
        (store, engine)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn make_provider(format: &str, base_url: String, window: u32, max_tokens: u32) -> Provider {
    serde_json::from_value(json!({
        "id": "fixture",
        "name": "fixture",
        "base_url": base_url,
        "api_key": "fixture",
        "api_format": format,
        "enabled": true,
        "models": [{
            "id": "gpt-6-astra",
            "name": "fixture",
            "enabled": true,
            "supports_vision": false,
            "supports_web_search": false,
            "context_window": window,
            "max_tokens": max_tokens,
        }],
    }))
    .unwrap()
}

fn history(history: History) -> String {
    match history {
        History::Short => "evidence ".repeat(20_000),
        History::Marked => format!(
            "{}{}{}",
            "ALPHA ".repeat(30_000),
            "BETA ".repeat(30_000),
            "OMEGA ".repeat(400)
        ),
        History::Evidence(count) => "evidence ".repeat(count),
        History::Unique(count) => (0..count).map(|index| format!("note{index} ")).collect(),
        History::Poisoned { poison, rest } => {
            let mut text = "ALPHA ".repeat(poison);
            text.extend((0..rest).map(|index| format!("note{index} ")));
            text
        }
    }
}

/// The widest summary the engine's 32,000-character result limit accepts,
/// mixing ASCII, Chinese and emoji. Two of them exceed the 64k request
/// ceiling while each one alone still fits it.
fn wide_summary() -> String {
    let mut text = "🙂".repeat(27_000);
    text.push_str(&"中".repeat(2_500));
    text.push_str(&"abc ".repeat(625));
    text
}

async fn start(config: Config) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(dir.path(), "gpt-6-astra", "fixture").unwrap();
    store
        .append(&id, "user", "Original request: keep 中文🙂 evidence")
        .unwrap();
    store
        .append(&id, "assistant", &history(config.history))
        .unwrap();
    store
        .append(&id, "user", "Latest correction: finish the pending tests")
        .unwrap();

    let requests = Arc::new(Mutex::new(Vec::new()));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let parent = Arc::new(Mutex::new(None));
    let children = Arc::new(AtomicUsize::new(0));
    let stop_at = Arc::new(AtomicUsize::new(config.stop_at));
    let hang_at = Arc::new(AtomicUsize::new(0));
    let merge_truncate = Arc::new(AtomicUsize::new(1));
    let captured = requests.clone();
    let captured_sent = sent.clone();
    let captured_parent = parent.clone();
    let captured_children = children.clone();
    let captured_stop = stop_at.clone();
    let captured_hang = hang_at.clone();
    let captured_merge = merge_truncate.clone();
    let anthropic = config.format == "anthropic";
    let window = u64::from(config.window);
    let (behavior, delay) = (config.behavior, config.delay);
    let handler = move |Json(body): Json<Value>| {
        let captured = captured.clone();
        let captured_sent = captured_sent.clone();
        let captured_parent = captured_parent.clone();
        let captured_children = captured_children.clone();
        let captured_stop = captured_stop.clone();
        let captured_hang = captured_hang.clone();
        let captured_merge = captured_merge.clone();
        async move {
            assert!(body
                .get("tools")
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)));
            let usage = ContextUsage::from_body(
                if anthropic {
                    "anthropic-messages"
                } else {
                    "openai-completions"
                },
                &body,
                Some(window as u32),
                4096,
            );
            let tokens = usage.text_subtotal();
            let budget = body["max_tokens"].as_u64().unwrap();
            // Every request must stay inside the summary path's safe line. The
            // wire adds a few framing bytes per message that the estimator does
            // not model, far below the line's own margin.
            let safe = (window * 9 / 10).saturating_sub(budget + 1024);
            assert!(
                tokens <= safe + 128,
                "wire input {tokens} exceeds the safe budget {safe} for output {budget}"
            );
            let number = {
                let mut requests = captured.lock().unwrap();
                requests.push(body.clone());
                requests.len()
            };
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if captured_hang.load(Ordering::SeqCst) == number {
                // Hold this request so the caller can cancel the compaction.
                std::future::pending::<()>().await;
            }
            let sent = body.to_string();
            let stage = stage_of(&sent);
            // A fragment request belongs to the recovery of the last truncated
            // fragment when it is one of the two halves a split produces.
            let parent = *captured_parent.lock().unwrap();
            let origin = if stage != "fragment" {
                ""
            } else if matches!(parent, Some((size, _)) if tokens > size) {
                // Larger than the fragment it is compared with: a new planned
                // fragment, and any pending halves belong to a finished branch.
                captured_children.store(0, Ordering::SeqCst);
                "original"
            } else if captured_children.load(Ordering::SeqCst) > 0 {
                captured_children.fetch_sub(1, Ordering::SeqCst);
                "recovery-child"
            } else {
                "original"
            };
            let merge_flag = captured_merge.load(Ordering::SeqCst) == 1;
            let truncate = match behavior {
                Behavior::TruncateAbove(limit) => tokens > limit,
                Behavior::TruncateBelowBudget(limit) => budget < limit,
                Behavior::LongSummaryAbove(limit) => tokens > limit,
                Behavior::WideSummaryAbove(limit) => tokens > limit,
                Behavior::AlwaysTruncate => true,
                Behavior::TruncateContaining(marker) => sent.contains(marker),
                Behavior::TruncateFirst => number == 1,
                // Only the first merge request is truncated; its split
                // recovery finishes at the same configured budget.
                Behavior::TruncateMergeOnce => {
                    stage == "merge" && merge_flag && {
                        captured_merge.store(0, Ordering::SeqCst);
                        true
                    }
                }
                Behavior::TruncateMergeSwitch => stage == "merge" && merge_flag,
                Behavior::LongSummaryAlways => false,
                Behavior::EmptyFirst
                | Behavior::EmptyAlways
                | Behavior::EmptyAbove(_)
                | Behavior::EmptyOnceAbove(_)
                | Behavior::EmptyContaining(_)
                | Behavior::OversizedAbove(_) => false,
            };
            if truncate && stage == "fragment" {
                // Every truncation is recovered by splitting, so two children
                // follow at the same configured budget.
                captured_children.store(2, Ordering::SeqCst);
                *captured_parent.lock().unwrap() = Some((tokens, budget));
            } else if stage == "fragment" {
                *captured_parent.lock().unwrap() = None;
            }
            {
                let mut log = captured_sent.lock().unwrap();
                log.push(Sent {
                    stage,
                    origin,
                    tokens,
                    budget,
                    truncated: truncate,
                });
            }
            // A finished response the summarizer cannot use. It is never
            // reported as an output limit, so recovery cannot repair it with a
            // larger budget; it is retried once at the same budget or split.
            let empty = match behavior {
                Behavior::EmptyFirst => number == 1 && !truncate,
                Behavior::EmptyAlways => !truncate,
                Behavior::EmptyAbove(limit) => tokens > limit && !truncate,
                Behavior::EmptyOnceAbove(limit) => {
                    tokens > limit
                        && !truncate
                        && captured_sent
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|entry| entry.tokens == tokens && entry.budget == budget)
                            .count()
                            == 1
                }
                Behavior::EmptyContaining(marker) => sent.contains(marker) && !truncate,
                _ => false,
            };
            let oversized =
                matches!(behavior, Behavior::OversizedAbove(limit) if tokens > limit) && !truncate;
            let finish = if captured_stop.load(Ordering::SeqCst) == number {
                2
            } else if truncate {
                1
            } else {
                0
            };
            let label = if sent.contains("BETA") || sent.contains("OMEGA") {
                "summary-second"
            } else {
                "summary-first"
            };
            let text = if empty {
                String::new()
            } else if oversized {
                "s".repeat(32_001)
            } else {
                match (finish, behavior) {
                    // Truncated text must never reach the cache or the history.
                    (1, _) => format!("TRUNCATED #{number}"),
                    (_, Behavior::LongSummaryAbove(_)) => "s".repeat(32_000),
                    // The widest text the character limit accepts: about 39k tokens
                    // each, so merging two exceeds the 64k request ceiling while a
                    // vast model window would still accept it.
                    (_, Behavior::WideSummaryAbove(_)) => wide_summary(),
                    // Distinguishable, near the character limit: about 10.7k tokens,
                    // so only five fit one merge request next to its prompt.
                    (_, Behavior::LongSummaryAlways) => {
                        format!("summary{number} {}", "s".repeat(31_980))
                    }
                    // Long enough that merging the two children is a real input
                    // which can itself be split after another truncation.
                    (_, Behavior::TruncateMergeSwitch) if stage == "fragment" => {
                        format!("{label} #{number} {}", "s".repeat(1_400))
                    }
                    _ => format!("{label} #{number}"),
                }
            };
            Json(if anthropic {
                json!({"id":"r","type":"message","role":"assistant","model":"gpt-6-astra","content":[{"type":"text","text":text}],"stop_reason":match finish { 1 => "max_tokens", 2 => "refusal", _ => "end_turn" },"usage":{"input_tokens":1,"output_tokens":1}})
            } else {
                json!({"id":"r","object":"chat.completion","created":0,"model":"gpt-6-astra","choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":match finish { 1 => "length", 2 => "content_filter", _ => "stop" }}]})
            })
        }
    };
    let app = Router::new()
        .route(
            if anthropic {
                "/v1/messages"
            } else {
                "/v1/chat/completions"
            },
            post(handler),
        )
        // A whole-transcript request can reach a few megabytes; the default 2 MB
        // limit would answer 413 before the fixture could record it.
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let engine = runtime::create_engine(
        &paths,
        store.db.clone(),
        Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
            vec![make_provider(
                config.format,
                base_url.clone(),
                config.window,
                config.max_tokens,
            )],
            paths.data_dir.join("providers.json"),
        ))),
        runtime::default_permissions(),
        Arc::new(FileCache::new()),
    );
    Fixture {
        dir,
        store,
        id,
        engine: Some(engine),
        requests,
        sent,
        stop_at,
        hang_at,
        merge_truncate,
        config,
        base_url,
        server,
    }
}

fn bodies(fixture: &Fixture) -> Vec<Value> {
    fixture.requests.lock().unwrap().clone()
}

/// Every request the fixture answered, with its stage, origin and sizes.
fn sent_log(fixture: &Fixture) -> Vec<Sent> {
    fixture.sent.lock().unwrap().clone()
}

fn prompt_of(body: &Value) -> String {
    body["messages"].to_string()
}

/// The stage sequence a fixture saw, so a test can tell original fragments,
/// recovery children and merges apart.
fn stages(sent: &[Value]) -> Vec<&'static str> {
    sent.iter().map(|body| stage_of(&prompt_of(body))).collect()
}

/// Attach one oversized tool result, as a real turn would record it.
fn attach_tool_result(fixture: &Fixture, name: &str, output: &str) {
    let message_id = fixture
        .store
        .db
        .with_conn(|conn| {
            Ok::<String, anyhow::Error>(
                conn.query_row(
                    "SELECT id FROM messages WHERE conversation_id=?1 AND role='assistant'",
                    [&fixture.id],
                    |row| row.get(0),
                )
                .unwrap(),
            )
        })
        .unwrap()
        .unwrap();
    fixture
        .store
        .db
        .with_conn(|conn| {
            deepseek_code_core::db::message_repo::insert_tool_call(
                conn,
                &format!("{message_id}:tool-1"),
                &message_id,
                name,
                Some("{}"),
                Some(output),
                false,
                0,
            )
        })
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn final_summary_starts_at_the_configured_budget_and_keeps_its_target() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::TruncateBelowBudget(16_384),
            History::Short,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let sent = bodies(&fixture);
        // The first request already uses the configured cap, so the fixture's
        // "truncate below the cap" rule never fires and no staircase exists.
        assert_eq!(sent.len(), 1, "{format}");
        assert_eq!(sent[0]["max_tokens"], 16_384, "{format}");
        assert!(prompt_of(&sent[0]).contains(FINAL_PROMPT));
        assert!(
            prompt_of(&sent[0]).contains("Aim for about 4096 tokens"),
            "{format}"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1
        );
    }
}

#[tokio::test]
async fn configured_32k_generation_budget_completes_in_one_request() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            32_000,
            Behavior::TruncateBelowBudget(32_000),
            History::Short,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let sent = bodies(&fixture);
        // A step staircase at 4096/8192 would be truncated by this fixture; the
        // configured 32k budget finishes in one request.
        assert_eq!(sent.len(), 1, "{format}");
        assert_eq!(sent[0]["max_tokens"], 32_000, "{format}");
        assert!(prompt_of(&sent[0]).contains("Aim for about 4096 tokens"));
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1
        );
    }
}

#[tokio::test]
async fn configured_128k_budget_is_preserved_when_the_window_allows() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            1_000_000,
            128_000,
            Behavior::TruncateBelowBudget(128_000),
            History::Short,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let sent = bodies(&fixture);
        // The selected model's own 128k cap is honored, not a global 32k.
        assert_eq!(sent.len(), 1, "{format}");
        assert_eq!(sent[0]["max_tokens"], 128_000, "{format}");
        assert!(prompt_of(&sent[0]).contains("Aim for about 4096 tokens"));
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1
        );
    }
}

#[tokio::test]
async fn a_cap_too_large_for_the_window_is_refused_before_any_request() {
    let fixture = start(Config::new(
        "openai",
        2_000,
        32_000,
        Behavior::TruncateAbove(u64::MAX),
        History::Short,
    ))
    .await;
    let error = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap_err();
    let text = format!("{error:#}");
    // No number is invented to make the request fit; the configured budget is
    // named and no request is sent.
    assert!(
        text.contains("Compaction output budget 32000 leaves less than 1024 input tokens"),
        "{text}"
    );
    assert!(text.contains("no request was sent"), "{text}");
    assert!(text.contains("original history retained"), "{text}");
    assert!(bodies(&fixture).is_empty());
}

#[tokio::test]
async fn truncated_final_summary_splits_at_the_configured_budget() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::TruncateFirst,
            History::Short,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let sent = bodies(&fixture);
        let log = sent_log(&fixture);
        // Truncated final, its two halves and their merge, all at the same
        // configured budget. There is no 8192/16384 staircase.
        assert_eq!(log.len(), 4, "{format}: {log:?}");
        assert!(
            log.iter().all(|entry| entry.budget == 16_384),
            "{format}: {log:?}"
        );
        assert_eq!(
            (log[0].stage, log[1].stage, log[2].stage, log[3].stage),
            ("final", "final", "final", "merge"),
            "{format}"
        );
        assert!(prompt_of(&sent[0]).contains("Aim for about 4096 tokens"));
        assert!(prompt_of(&sent[3]).contains("Aim for about 2048 tokens"));
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1
        );
    }
}

#[tokio::test]
async fn fragment_requests_start_at_the_configured_budget_and_keep_the_target() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::TruncateAbove(u64::MAX),
            History::Evidence(60_000),
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let sent = bodies(&fixture);
        let log = sent_log(&fixture);
        assert!(!log.is_empty(), "{format}");
        assert!(
            log.iter()
                .filter(|entry| entry.stage == "fragment")
                .all(|entry| entry.budget == 16_384),
            "{format}: every fragment uses the configured cap: {log:?}"
        );
        for body in sent
            .iter()
            .filter(|body| prompt_of(body).contains(FRAGMENT_PROMPT))
        {
            assert!(
                prompt_of(body).contains("Aim for about 2048 tokens"),
                "{format}"
            );
        }
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1
        );
    }
}

#[tokio::test]
async fn truncation_at_the_model_cap_splits_by_tokens_and_merges_in_order() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            4096,
            Behavior::TruncateAbove(60_000),
            History::Marked,
        ))
        .await;
        // One oversized tool result must survive the same split intact.
        attach_tool_result(
            &fixture,
            "Bash",
            &format!("TOOLRESULT {}", "x".repeat(40_000)),
        );
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let sent = bodies(&fixture);
        let log = sent_log(&fixture);
        assert!(log.len() > 3, "{format}");
        assert!(
            log.iter().all(|entry| entry.budget == 4096),
            "{format}: the model cap must not be raised"
        );
        assert!(
            log.iter().all(|entry| entry.tokens <= 64_000),
            "{format}: every request stays inside the single-request ceiling"
        );
        // The truncated fragment is summarized as two smaller, ordered halves.
        let children: Vec<usize> = log
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.origin == "recovery-child")
            .map(|(index, _)| index)
            .collect();
        let truncated: Vec<usize> = log
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.origin == "original" && entry.truncated)
            .map(|(index, _)| index)
            .collect();
        assert!(!truncated.is_empty(), "{format}: {log:?}");
        // Every truncated fragment is summarized as two smaller halves, and
        // the merge of those halves follows them.
        assert_eq!(children.len(), truncated.len() * 2, "{format}: {log:?}");
        let merge = log
            .iter()
            .position(|entry| entry.stage == "merge")
            .expect("a merge request follows the split halves");
        assert!(merge > children[1], "{format}: {log:?}");
        let merged = prompt_of(&sent[merge]);
        let first = merged.find("summary-first").expect("first half summary");
        let second = merged.find("summary-second").expect("second half summary");
        assert!(first < second, "{format}: the merge keeps the half order");
        // Seeded Unicode and the oversized tool result cross the wire intact,
        // never as a replacement character.
        assert!(
            sent.iter().any(|body| body.to_string().contains("中文🙂")),
            "{format}: the transcript must keep its UTF-8 text"
        );
        assert!(
            sent.iter()
                .any(|body| body.to_string().contains("TOOLRESULT xxxx")),
            "{format}: the oversized tool result must reach the fragments"
        );
        assert!(
            !sent
                .iter()
                .any(|body| body.to_string().contains('\u{fffd}')),
            "{format}"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1
        );
    }
}

#[tokio::test]
async fn a_truncated_merge_splits_at_the_configured_budget_and_reports_its_own_stage() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::TruncateMergeOnce,
            History::Unique(30_000),
        ))
        .await;
        std::fs::write(
            fixture.dir.path().join("data/settings.json"),
            r#"{"requestRecording":{"enabled":true}}"#,
        )
        .unwrap();
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let sent = bodies(&fixture);
        let log = sent_log(&fixture);
        // Two planned fragments, the truncated merge, its two halves, their
        // merge and the final summary, all at the configured budget.
        assert_eq!(log.len(), 7, "{format}: {log:?}");
        assert_eq!(
            stages(&sent),
            ["fragment", "fragment", "merge", "merge", "merge", "merge", "final"],
            "{format}"
        );
        assert!(
            log.iter().all(|entry| entry.budget == 16_384),
            "{format}: no staircase: {log:?}"
        );
        assert!(log[2].truncated, "{format}: {log:?}");
        assert!(!log[3].truncated && !log[4].truncated, "{format}: {log:?}");
        assert!(
            prompt_of(&sent[2]).contains("Aim for about 2048 tokens"),
            "{format}"
        );
        assert!(
            prompt_of(&sent[3]).contains("Aim for about 2048 tokens"),
            "{format}"
        );
        let merged = prompt_of(&sent[2]);
        let first = merged.find("summary-first #1").expect("first summary");
        let second = merged.find("summary-first #2").expect("second summary");
        assert!(
            first < second,
            "{format}: the merge keeps the summary order"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1
        );
        let dir = fixture
            .dir
            .path()
            .join("data/request-records")
            .join(&fixture.id);
        let mut records: Vec<Value> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().ends_with(".meta.json"))
            .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
            .collect();
        records.sort_by_key(|r| r["sequence"].as_u64().unwrap());
        assert_eq!(records.len(), 7);
        assert_eq!(
            records
                .iter()
                .map(|r| r["compaction_stage"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["segment", "segment", "merge", "recovery", "recovery", "recovery", "final"]
        );
        assert!(records
            .iter()
            .all(|r| r["purpose"] == "compaction" && r["stream_completed"] == true));
        let compact: Value =
            serde_json::from_slice(&std::fs::read(dir.join("compaction-0001.json")).unwrap())
                .unwrap();
        assert_eq!(compact["request_sequences"], json!([1, 2, 3, 4, 5, 6, 7]));
        assert_eq!(compact["completed"], true);
        assert_eq!(compact["trigger"], "manual");
    }
}

#[tokio::test]
async fn a_persistently_truncated_merge_stops_at_the_depth_limit_and_reuses_fragments() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::TruncateMergeSwitch,
            History::Unique(30_000),
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        let error = compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        // The merge itself hits the depth limit; the report keeps the merge
        // stage, its own node allowance and the last budget actually sent.
        assert!(text.contains(" merge "), "{text}");
        assert!(text.contains("depth limit"), "{text}");
        assert!(text.contains("2 of 8 recovery generations"), "{text}");
        assert!(
            text.contains("still exceeded its 16384-token output limit"),
            "{text}"
        );
        assert!(text.contains("original history retained"), "{text}");
        assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
        let first_run = sent_log(&fixture);
        let planned = first_run
            .iter()
            .filter(|entry| entry.origin == "original" && entry.stage == "fragment")
            .count();
        assert_eq!(planned, 2, "{first_run:?}");
        assert!(first_run
            .iter()
            .any(|entry| entry.stage == "merge" && entry.truncated));

        // Once merges finish again, the completed fragment summaries come from
        // the checkpoint instead of the model, and the original history is
        // replaced by one summary only after the whole compaction succeeds.
        fixture.merge_truncate.store(0, Ordering::SeqCst);
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let after = sent_log(&fixture);
        let replays = after[first_run.len()..]
            .iter()
            .filter(|entry| entry.stage == "fragment")
            .count();
        assert_eq!(
            replays, 0,
            "{format}: the completed fragment summaries must be reused"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1,
            "{format}"
        );
    }
}

#[tokio::test]
async fn persistent_truncation_stops_within_each_fragment_allowance_and_never_caches_truncated_text(
) {
    let fixture = start(Config::new(
        "openai",
        64_000,
        4096,
        Behavior::AlwaysTruncate,
        History::Short,
    ))
    .await;
    let before = fixture.store.history(&fixture.id).unwrap();
    let error = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("output limit"), "{text}");
    assert!(text.contains("recovery generations"), "{text}");
    assert!(text.contains("original history retained"), "{text}");
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
    // Two fragments, each bounded by its own eight recovery generations.
    assert!(
        bodies(&fixture).len() <= 2 + 2 * 8,
        "the per-fragment allowance must bound every request"
    );
    // Truncated output never reaches the summary cache.
    let cached = fixture
        .store
        .db
        .with_conn(|conn| {
            Ok::<Option<String>, anyhow::Error>(conn
                .query_row(
                    "SELECT value FROM peripheral_settings WHERE key LIKE 'compaction.checkpoint.%'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok())
        })
        .unwrap()
        .unwrap();
    assert!(
        !cached.as_deref().unwrap_or_default().contains("TRUNCATED"),
        "{cached:?}"
    );
}

#[tokio::test]
async fn each_fragment_allowance_caps_its_own_recovery_requests() {
    // Nine planned fragments, eight of which need one split each. Every
    // fragment's recovery stays inside its own eight generations, so the
    // compaction finishes and the total number of requests stays bounded.
    let fixture = start(Config::new(
        "openai",
        64_000,
        4096,
        Behavior::TruncateAbove(30_000),
        History::Unique(130_000),
    ))
    .await;
    let before = fixture.store.history(&fixture.id).unwrap();
    compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap();
    let log = sent_log(&fixture);
    let planned = log
        .iter()
        .filter(|entry| entry.origin == "original" && entry.stage == "fragment")
        .count();
    assert_eq!(planned, 9, "{log:?}");
    // A fragment adds at most two halves and their merge to its own request.
    assert!(
        log.len() <= planned * 4 + 2,
        "{} requests for {planned} fragments",
        log.len()
    );
    assert_eq!(
        fixture.store.history(&fixture.id).unwrap().len(),
        before.len() + 1
    );
}

#[tokio::test]
async fn a_failing_fragment_keeps_the_others_and_spends_only_its_own_allowance() {
    for format in ["openai", "anthropic"] {
        // The first planned fragment carries a block that truncates at every
        // depth, so it exhausts its own allowance; the notes behind it finish.
        let fixture = start(Config::new(
            format,
            128_000,
            8192,
            Behavior::TruncateContaining("ALPHA"),
            History::Poisoned {
                poison: 30_000,
                rest: 40_000,
            },
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        let error = compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        // Only the poisoned fragment failed, and the report names it with its
        // own recovery usage instead of a compaction-wide allowance.
        assert!(
            text.contains("1 of 3 compaction fragments failed"),
            "{format}: {text}"
        );
        assert!(text.contains("fragment 1:"), "{format}: {text}");
        assert!(
            text.contains("of 8 recovery generations"),
            "{format}: {text}"
        );
        assert!(
            text.contains("original history retained"),
            "{format}: {text}"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap(),
            before,
            "{format}"
        );

        let first_run = sent_log(&fixture);
        let bodies_before = bodies(&fixture);
        let planned = first_run
            .iter()
            .filter(|entry| entry.origin == "original" && entry.stage == "fragment")
            .count();
        assert_eq!(planned, 3, "{format}: {first_run:?}");
        // Both remaining planned fragments completed and were cached.
        let completed: Vec<Value> = first_run
            .iter()
            .zip(bodies_before.iter())
            .filter(|(entry, _)| {
                entry.origin == "original" && entry.stage == "fragment" && !entry.truncated
            })
            .map(|(_, body)| body.clone())
            .collect();
        assert_eq!(completed.len(), 2, "{format}");
        let cache = fixture
            .store
            .db
            .with_conn(|conn| {
                Ok::<Option<String>, anyhow::Error>(conn
                    .query_row(
                        "SELECT value FROM peripheral_settings WHERE key LIKE 'compaction.checkpoint.%'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .ok())
            })
            .unwrap()
            .unwrap();
        assert!(
            !cache.as_deref().unwrap_or_default().contains("TRUNCATED"),
            "{format}: {cache:?}"
        );

        // The retry of the same compaction reuses those completed fragments.
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap_err();
        let replay = bodies(&fixture)[bodies_before.len()..]
            .iter()
            .filter(|body| completed.contains(body))
            .count();
        assert_eq!(
            replay, 0,
            "{format}: a completed fragment must not be requested again"
        );
    }
}

#[tokio::test]
async fn exhausted_recovery_reports_the_configured_budget() {
    let fixture = start(Config::new(
        "openai",
        128_000,
        16_384,
        Behavior::AlwaysTruncate,
        History::Short,
    ))
    .await;
    let before = fixture.store.history(&fixture.id).unwrap();
    let error = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap_err();
    let text = format!("{error:#}");
    // The request already started at the configured cap, so every recovery
    // attempt names the same budget.
    assert!(
        text.contains("still exceeded its 16384-token output limit"),
        "{text}"
    );
    assert!(text.contains("2 of 8 recovery generations"), "{text}");
    assert!(text.contains("depth limit"), "{text}");
    assert!(text.contains("original history retained"), "{text}");
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
}

#[tokio::test]
async fn a_cancelled_recovery_reuses_the_sub_summaries_it_completed() {
    let fixture = start(Config::new(
        "openai",
        128_000,
        4096,
        Behavior::TruncateAbove(60_000),
        History::Marked,
    ))
    .await;
    let before = fixture.store.history(&fixture.id).unwrap();
    fixture.hang_at.store(4, Ordering::SeqCst);
    let future = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    );
    tokio::pin!(future);
    tokio::select! {
        _ = &mut future => panic!("the merge must stay pending"),
        _ = async {
            while fixture.requests.lock().unwrap().len() < 4 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } => {}
    }
    // Dropping the future cancels the merge without touching the conversation.
    drop(future);
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
    let first_run = bodies(&fixture);
    assert_eq!(first_run.len(), 4);

    fixture.hang_at.store(0, Ordering::SeqCst);
    compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap();
    let replay = fixture.requests.lock().unwrap()[first_run.len()..]
        .iter()
        .filter(|body| **body == first_run[1] || **body == first_run[2])
        .count();
    assert_eq!(
        replay, 0,
        "sub-summaries completed before the cancel are reused"
    );
    assert_eq!(
        fixture.store.history(&fixture.id).unwrap().len(),
        before.len() + 1
    );
}

/// The checkpoint keeps completed fragments after a cancelled compaction is
/// followed by a real new message. Only requests whose exact input changed may
/// be sent again; the unchanged prefix fragments are reused.
async fn unchanged_fragments_after_a_grown_history(reopen: bool) {
    grown_history_case(reopen, Behavior::TruncateAbove(u64::MAX), 3, 0..2).await;
}

async fn grown_history_case(
    reopen: bool,
    behavior: Behavior,
    hang: usize,
    cached: std::ops::Range<usize>,
) {
    for format in ["openai", "anthropic"] {
        let mut fixture = start(Config::new(
            format,
            30_000,
            4096,
            behavior,
            History::Unique(30_000),
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        // Hold the third fragment, so the first two complete.
        fixture.hang_at.store(hang, Ordering::SeqCst);
        let first_run = {
            let future = compaction::compact(
                &fixture.engine(),
                &fixture.store.db,
                &fixture.id,
                "fixture",
                "gpt-6-astra",
                "",
            );
            tokio::pin!(future);
            tokio::select! {
                _ = &mut future => panic!("the third fragment must stay pending"),
                _ = async {
                    while fixture.requests.lock().unwrap().len() < hang {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                } => {}
            }
            drop(future);
            bodies(&fixture)
        };
        assert_eq!(first_run.len(), hang);
        match behavior {
            Behavior::EmptyAbove(limit) => {
                assert!(sent_log(&fixture)[0].tokens > limit);
                assert_eq!(
                    first_run[0], first_run[1],
                    "empty response must retry before splitting"
                );
                assert!(sent_log(&fixture)[2].tokens < sent_log(&fixture)[0].tokens);
            }
            Behavior::OversizedAbove(limit) => {
                assert!(sent_log(&fixture)[0].tokens > limit);
                assert!(sent_log(&fixture)[1].tokens < sent_log(&fixture)[0].tokens);
            }
            _ => {}
        }
        let completed = first_run[cached.clone()].to_vec();
        assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);

        // A real interrupted record now changes the transcript.
        fixture
            .store
            .append(&fixture.id, "user", "INTERRUPTED-NOTE-ZZZ keep this record")
            .unwrap();
        if reopen {
            let (store, engine) = fixture.reopen();
            fixture.store = store;
            fixture.engine = Some(engine);
        }
        fixture.hang_at.store(0, Ordering::SeqCst);
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();

        let retry = bodies(&fixture)[first_run.len()..].to_vec();
        for body in &completed {
            assert!(
                !retry.contains(body),
                "{format}: an unchanged fragment was sent again"
            );
        }
        assert!(
            retry
                .iter()
                .any(|body| body.to_string().contains("INTERRUPTED-NOTE-ZZZ")),
            "{format}: the changed tail must be re-planned"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 2
        );
    }
}

#[tokio::test]
async fn a_cancelled_recovery_reuses_unchanged_fragments_after_history_grows() {
    unchanged_fragments_after_a_grown_history(false).await;
}

#[tokio::test]
async fn a_cancelled_recovery_reuses_unchanged_fragments_after_history_grows_and_restart() {
    unchanged_fragments_after_a_grown_history(true).await;
}

#[tokio::test]
async fn a_non_truncating_stop_reports_stage_budget_and_does_not_split() {
    let mut config = Config::new(
        "openai",
        128_000,
        4096,
        Behavior::TruncateAbove(60_000),
        History::Marked,
    );
    config.stop_at = 2;
    let fixture = start(config).await;
    let before = fixture.store.history(&fixture.id).unwrap();
    let error = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("Generation did not finish normally"),
        "{text}"
    );
    assert!(
        text.contains("failed at its 4096-token output budget"),
        "{text}"
    );
    assert!(text.contains("of 8 recovery generations"), "{text}");
    assert!(text.contains("original history retained"), "{text}");
    // A non-truncation stop never launches another split.
    assert_eq!(bodies(&fixture).len(), 2, "{text}");
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
}

#[tokio::test]
async fn a_merge_above_the_safe_budget_fails_before_it_is_sent() {
    // The child summaries are 32k characters each, so merging two of them no
    // longer fits the safe line every other summary request obeys. The failure
    // names the merge stage and the request is never sent.
    for window in [29_000, 20_000] {
        let fixture = start(Config::new(
            "openai",
            window,
            4096,
            Behavior::LongSummaryAbove(11_000),
            History::Evidence(60_000),
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        let error = compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains(" merge "), "window {window}: {text}");
        assert!(
            text.contains("no request was sent for this input"),
            "window {window}: {text}"
        );
        assert!(
            text.contains("original history retained"),
            "window {window}: {text}"
        );
        assert!(
            sent_log(&fixture)
                .iter()
                .all(|entry| entry.stage != "merge"),
            "window {window}: an over-budget merge must not be sent"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap(),
            before,
            "window {window}"
        );
    }
}

#[tokio::test]
async fn summaries_that_cannot_be_combined_fail_without_a_loop() {
    // Each fragment answers with a summary that cannot fit the small window's
    // merge input budget, so no layer can converge. The failure says so
    // instead of looping.
    let fixture = start(Config::new(
        "openai",
        29_000,
        16_384,
        Behavior::LongSummaryAlways,
        History::Evidence(6_400),
    ))
    .await;
    let before = fixture.store.history(&fixture.id).unwrap();
    let error = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("merge input budget"), "{text}");
    assert!(text.contains("original history retained"), "{text}");
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
    assert!(sent_log(&fixture)
        .iter()
        .all(|entry| entry.stage != "merge"));
}

#[tokio::test]
async fn a_window_too_small_for_the_configured_budget_fails_without_a_request() {
    let fixture = start(Config::new(
        "openai",
        2_000,
        4096,
        Behavior::TruncateAbove(0),
        History::Short,
    ))
    .await;
    let error = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap_err();
    let text = format!("{error:#}");
    // A configured budget that leaves no safe input room is refused explicitly
    // before any request; no unlimited output is invented.
    assert!(
        text.contains("Compaction output budget 4096 leaves less than 1024 input tokens"),
        "{text}"
    );
    assert!(text.contains("2000-token context window"), "{text}");
    assert!(text.contains("no request was sent"), "{text}");
    assert!(text.contains("original history retained"), "{text}");
    assert!(bodies(&fixture).is_empty());
}

#[tokio::test]
async fn recovered_fragments_survive_a_restart_before_the_retry() {
    let mut config = Config::new(
        "openai",
        128_000,
        4096,
        Behavior::TruncateAbove(60_000),
        History::Marked,
    );
    config.stop_at = 4;
    let mut fixture = start(config).await;
    let before = fixture.store.history(&fixture.id).unwrap();
    let error = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("did not finish normally"),
        "{error:#}"
    );
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
    let first_run = bodies(&fixture);
    assert_eq!(first_run.len(), 4);

    fixture.stop_at.store(0, Ordering::SeqCst);
    let (store, engine) = fixture.reopen();
    compaction::compact(
        &engine,
        &store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap();
    let replay = fixture.requests.lock().unwrap()[first_run.len()..]
        .iter()
        .filter(|body| **body == first_run[1] || **body == first_run[2])
        .count();
    assert_eq!(
        replay, 0,
        "completed sub-summaries must be reused after a restart"
    );
    assert_eq!(store.history(&fixture.id).unwrap().len(), before.len() + 1);
}

#[tokio::test]
async fn recovered_fragments_are_reused_after_a_later_failure() {
    let mut config = Config::new(
        "openai",
        128_000,
        4096,
        Behavior::TruncateAbove(60_000),
        History::Marked,
    );
    config.stop_at = 4;
    let fixture = start(config).await;
    let before = fixture.store.history(&fixture.id).unwrap();
    let error = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("did not finish normally"),
        "{error:#}"
    );
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);

    fixture.stop_at.store(0, Ordering::SeqCst);
    compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap();
    let sent = fixture.requests.lock().unwrap();
    let replayed = sent[4..]
        .iter()
        .filter(|body| **body == sent[1] || **body == sent[2])
        .count();
    assert_eq!(
        replayed, 0,
        "the recovered fragment must come from the checkpoint"
    );
    assert_eq!(
        fixture.store.history(&fixture.id).unwrap().len(),
        before.len() + 1
    );
}

#[tokio::test]
async fn a_history_change_during_recovery_rejects_the_commit_and_keeps_both() {
    let mut config = Config::new(
        "openai",
        128_000,
        4096,
        Behavior::TruncateAbove(60_000),
        History::Marked,
    );
    config.delay = 40;
    let fixture = start(config).await;
    let before = fixture.store.history(&fixture.id).unwrap();
    let future = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    );
    tokio::pin!(future);
    tokio::select! {
        _ = &mut future => panic!("compaction must still be running"),
        _ = async {
            while fixture.requests.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } => {}
    }
    fixture
        .store
        .append(&fixture.id, "user", "Concurrent edit while compacting")
        .unwrap();
    let error = future.await.unwrap_err();
    assert!(
        format!("{error:#}").contains("Session changed during compaction"),
        "{error:#}"
    );
    let after = fixture.store.history(&fixture.id).unwrap();
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(&after[..before.len()], before.as_slice());
    assert_eq!(
        after.last().unwrap()["content"],
        "Concurrent edit while compacting"
    );
}

#[tokio::test]
async fn local_summary_recovery_handles_truncation() {
    let fixture = start(Config::new(
        "openai",
        128_000,
        16_384,
        Behavior::TruncateFirst,
        History::Short,
    ))
    .await;
    let before = fixture.store.history(&fixture.id).unwrap();
    let message_id = fixture
        .store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT id FROM messages WHERE conversation_id=?1 ORDER BY sort_order DESC LIMIT 1",
                [&fixture.id],
                |row| row.get::<_, String>(0),
            )
        })
        .unwrap()
        .unwrap();
    let (branch, count) = compaction::summarize_part(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        &message_id,
        compaction::Part::From,
        "fixture",
        "gpt-6-astra",
    )
    .await
    .unwrap();
    assert_eq!(count, 1);
    let sent = bodies(&fixture);
    // The truncated first request is split and merged at the same configured
    // budget; there is no 4096→8192 escalation.
    assert_eq!(sent.len(), 4);
    assert!(sent.iter().all(|body| body["max_tokens"] == 16_384));
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
    let branch_history = fixture.store.history(&branch).unwrap();
    let summary = branch_history.last().unwrap()["content"].as_str().unwrap();
    assert!(
        summary.contains("[Summary of the later conversation"),
        "{summary}"
    );
}

#[tokio::test]
async fn a_very_long_history_is_split_at_the_single_request_ceiling() {
    // About 850k tokens of history: the earlier planner sent this as one
    // request whose length target the model could not reach. Every request now
    // stays within the 64K single-request ceiling and the compaction finishes.
    let fixture = start(Config::new(
        "openai",
        1_000_000,
        16_384,
        Behavior::TruncateAbove(100_000),
        History::Unique(250_000),
    ))
    .await;
    let before = fixture.store.history(&fixture.id).unwrap();
    compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap();
    let log = sent_log(&fixture);
    let largest = log.iter().map(|entry| entry.tokens).max().unwrap_or(0);
    assert!(
        largest <= 64_000,
        "every request stays at or below the 64K ceiling, saw {largest}"
    );
    // Nothing is truncated: a request above 100k tokens is never sent.
    assert!(log.iter().all(|entry| !entry.truncated), "{log:?}");
    let fragments = log.iter().filter(|entry| entry.stage == "fragment").count();
    assert!(
        fragments >= 13,
        "the history is split into {fragments} fragments"
    );
    // Merging is layered, so the earlier summaries are combined before the
    // final summary.
    let merges = log.iter().filter(|entry| entry.stage == "merge").count();
    assert!(merges >= 3, "{merges} merge requests");
    assert_eq!(log.last().unwrap().stage, "final");
    assert_eq!(
        fixture.store.history(&fixture.id).unwrap().len(),
        before.len() + 1
    );
}

#[tokio::test]
async fn a_recovery_merge_above_the_single_request_ceiling_is_never_sent() {
    for format in ["openai", "anthropic"] {
        // The 1M window accepts a ~79k-token merge, but the single-request
        // ceiling does not: the two child summaries are the widest the result
        // limit accepts. The merge must be refused before it is sent.
        let fixture = start(Config::new(
            format,
            1_000_000,
            16_384,
            Behavior::WideSummaryAbove(40_000),
            History::Poisoned {
                poison: 20_000,
                rest: 10_000,
            },
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        let error = compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("1 of 2 compaction fragments failed"),
            "{format}: {text}"
        );
        assert!(text.contains("merge"), "{format}: {text}");
        assert!(
            text.contains("safe context budget planned for its 16384-token output"),
            "{format}: {text}"
        );
        assert!(
            text.contains("no request was sent for this input"),
            "{format}: {text}"
        );
        assert!(
            text.contains("original history retained"),
            "{format}: {text}"
        );

        let log = sent_log(&fixture);
        let largest = log.iter().map(|entry| entry.tokens).max().unwrap_or(0);
        assert!(
            largest <= 64_000,
            "{format}: every request stays at or below 64k, saw {largest}: {log:?}"
        );
        assert!(
            log.iter().all(|entry| entry.stage != "merge"),
            "{format}: the over-ceiling merge must not be sent: {log:?}"
        );
        // Both halves were summarized before their merge was refused, so a
        // retry can reuse them.
        assert_eq!(
            log.iter()
                .filter(|entry| entry.origin == "recovery-child")
                .count(),
            2,
            "{format}: {log:?}"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap(),
            before,
            "{format}"
        );
    }
}

#[tokio::test]
async fn a_large_history_isolates_one_failed_fragment_and_reuses_the_rest() {
    // About 800k tokens in thirteen fragments: the first carries a block that
    // truncates at every depth and spends only its own allowance, while the
    // other twelve complete and are cached.
    let fixture = start(Config::new(
        "openai",
        128_000,
        8192,
        Behavior::TruncateContaining("ALPHA"),
        History::Poisoned {
            poison: 20_000,
            rest: 250_000,
        },
    ))
    .await;
    let before = fixture.store.history(&fixture.id).unwrap();
    let error = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("1 of "), "{text}");
    assert!(text.contains("compaction fragments failed"), "{text}");
    assert!(text.contains("original history retained"), "{text}");
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);

    let first_run = sent_log(&fixture);
    let bodies_before = bodies(&fixture);
    let planned: Vec<usize> = first_run
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.origin == "original" && entry.stage == "fragment")
        .map(|(index, _)| index)
        .collect();
    assert!(planned.len() > 6, "expected a large plan: {planned:?}");
    assert!(
        first_run.iter().all(|entry| entry.tokens <= 64_000),
        "every request stays at or below 64k"
    );
    // Everything the failing branch sent carried the poisoned block; every
    // other planned fragment completed.
    let completed: Vec<Value> = planned
        .iter()
        .filter(|&&index| !first_run[index].truncated)
        .map(|&index| bodies_before[index].clone())
        .collect();
    assert_eq!(completed.len(), planned.len() - 1, "{first_run:?}");

    // The retry of the same compaction reuses all of them.
    compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap_err();
    let replay = bodies(&fixture)[bodies_before.len()..]
        .iter()
        .filter(|body| completed.contains(body))
        .count();
    assert_eq!(
        replay, 0,
        "completed fragments must come from the checkpoint"
    );
}

#[tokio::test]
async fn long_summaries_form_smaller_groups_and_pass_a_single_one_through() {
    // Six fragments answer with ~10.7k-token summaries, so only five fit one
    // merge request. The sixth is passed to the next layer instead of being
    // merged alone, which would waste a request.
    let fixture = start(Config::new(
        "openai",
        1_000_000,
        16_384,
        Behavior::LongSummaryAlways,
        History::Unique(110_000),
    ))
    .await;
    compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap();
    let log = sent_log(&fixture);
    let fragments = log.iter().filter(|entry| entry.stage == "fragment").count();
    assert_eq!(fragments, 6, "{log:?}");
    let merges: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.stage == "merge")
        .map(|(index, _)| index)
        .collect();
    // One merge of five, then one merge of the passed-through sixth summary and
    // the merged result: two requests, never a merge of a single summary.
    assert_eq!(merges.len(), 2, "{log:?}");
    let bodies = bodies(&fixture);
    let text = |index: usize| bodies[index].to_string();
    let first = text(merges[0]);
    for number in 1..=5 {
        assert!(
            first.contains(&format!("summary{number} s")),
            "merge 1 is missing summary {number}: {log:?}"
        );
    }
    assert!(
        !first.contains("summary6 s"),
        "the sixth summary must be passed on, not merged alone: {log:?}"
    );
    let second = text(merges[1]);
    assert!(
        second.contains("summary6 s") && second.contains("summary7 s"),
        "the last layer merges the passed-through summary with the earlier merge: {log:?}"
    );
    assert_eq!(log.last().unwrap().stage, "final");
}

/// The checkpoint row, when one was written at all.
fn cached_checkpoint(fixture: &Fixture) -> Option<String> {
    fixture
        .store
        .db
        .with_conn(|conn| {
            Ok::<Option<String>, anyhow::Error>(conn
                .query_row(
                    "SELECT value FROM peripheral_settings WHERE key LIKE 'compaction.checkpoint.%'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok())
        })
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn an_empty_summary_is_retried_once_and_then_installed() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::EmptyFirst,
            History::Short,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let sent = bodies(&fixture);
        let log = sent_log(&fixture);
        // The empty body is a finished response, so it is retried unchanged at
        // the same budget; only a valid summary may replace the history.
        assert_eq!(sent.len(), 2, "{format}: {log:?}");
        assert_eq!(log[0].stage, "final", "{format}");
        assert_eq!(log[1].stage, "final", "{format}");
        assert_eq!(log[0].budget, log[1].budget, "{format}");
        assert_eq!(sent[0], sent[1], "{format}");
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1,
            "{format}"
        );
    }
}

#[tokio::test]
async fn an_empty_summary_splits_only_after_its_single_retry() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::EmptyAbove(40_000),
            History::Short,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let sent = bodies(&fixture);
        let log = sent_log(&fixture);
        // Empty, one unchanged retry, then the two halves and their merge.
        assert_eq!(sent.len(), 5, "{format}: {log:?}");
        assert_eq!((log[0].stage, log[1].stage), ("final", "final"), "{format}");
        assert_eq!(
            log[0].budget, log[1].budget,
            "{format}: an empty body must not raise the output budget"
        );
        assert_eq!(sent[0], sent[1], "{format}: the retry is the same request");
        assert_eq!((log[2].stage, log[3].stage), ("final", "final"), "{format}");
        assert_eq!(log[4].stage, "merge", "{format}");
        assert!(
            log[2].tokens < log[0].tokens && log[3].tokens < log[0].tokens,
            "{format}: {log:?}"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1,
            "{format}"
        );
    }
}

#[tokio::test]
async fn an_oversized_summary_splits_without_changing_the_budget_or_retrying() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::OversizedAbove(40_000),
            History::Short,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let sent = bodies(&fixture);
        let log = sent_log(&fixture);
        // 32,001 characters is not an output limit: no staircase and no
        // unchanged retry, the input is split at the configured budget.
        assert_eq!(sent.len(), 4, "{format}: {log:?}");
        assert_eq!(log[0].stage, "final", "{format}");
        assert!(
            log.iter().all(|entry| entry.budget == 16_384),
            "{format}: an oversized body must not change the budget: {log:?}"
        );
        assert_ne!(
            sent[0], sent[1],
            "{format}: it must not be retried unchanged"
        );
        assert_eq!((log[1].stage, log[2].stage), ("final", "final"), "{format}");
        assert_eq!(log[3].stage, "merge", "{format}");
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1,
            "{format}"
        );
    }
}

#[tokio::test]
async fn an_empty_answer_at_the_configured_budget_is_retried_once_before_splitting() {
    // The first answer is empty at the model's configured budget. The body is
    // re-requested once at that same budget; only when that also fails does
    // the branch split, still at the configured budget.
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::EmptyAbove(40_000),
            History::Marked,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let requests = bodies(&fixture);
        let log = sent_log(&fixture);
        assert_eq!(log[0].stage, "fragment", "{format}: {log:?}");
        assert!(!log[0].truncated, "{format}: empty, not truncated: {log:?}");
        assert_eq!(log[0].budget, 16_384, "{format}");
        // An identical retry at the same budget.
        assert_eq!(log[1].budget, 16_384, "{format}");
        assert!(!log[1].truncated, "{format}: {log:?}");
        assert_eq!(
            (log[1].tokens, log[1].budget),
            (log[0].tokens, log[0].budget),
            "{format}: {log:?}"
        );
        assert_eq!(
            requests[0], requests[1],
            "{format}: the empty body must be retried unchanged"
        );
        // Only then does the branch split, at the same configured budget.
        assert_eq!(
            (log[2].stage, log[2].budget),
            ("fragment", 16_384),
            "{format}: {log:?}"
        );
        assert_eq!(
            (log[3].stage, log[3].budget),
            ("fragment", 16_384),
            "{format}: {log:?}"
        );
        assert!(!log[2].truncated && !log[3].truncated, "{format}: {log:?}");
        assert!(
            log[2].tokens < log[0].tokens && log[3].tokens < log[0].tokens,
            "{format}: {log:?}"
        );
        assert_eq!(log[4].stage, "merge", "{format}: {log:?}");
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1,
            "{format}"
        );
    }
}

#[tokio::test]
async fn an_empty_summary_is_recovered_by_the_same_budget_retry() {
    // The first answer is empty at the configured budget and the retry
    // succeeds: the branch never splits, and the summary is installed.
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::EmptyOnceAbove(40_000),
            History::Marked,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let requests = bodies(&fixture);
        let log = sent_log(&fixture);
        assert_eq!(
            (log[0].budget, log[0].truncated),
            (16_384, false),
            "{format}: {log:?}"
        );
        assert_eq!(
            (log[1].budget, log[1].truncated),
            (16_384, false),
            "{format}: {log:?}"
        );
        assert_eq!(requests[0], requests[1], "{format}");
        // The split never happened: every fragment request is a planned
        // fragment larger than the empty-answer threshold.
        let fragment_sizes: Vec<u64> = log
            .iter()
            .filter(|entry| entry.stage == "fragment")
            .map(|entry| entry.tokens)
            .collect();
        assert!(
            fragment_sizes.iter().all(|tokens| *tokens > 40_000),
            "{format}: no recovery children expected: {log:?}"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1,
            "{format}"
        );
    }
}

#[tokio::test]
async fn persistent_empty_summaries_stop_at_the_depth_limit_and_cache_nothing() {
    let fixture = start(Config::new(
        "openai",
        128_000,
        16_384,
        Behavior::EmptyAlways,
        History::Short,
    ))
    .await;
    let before = fixture.store.history(&fixture.id).unwrap();
    let error = compaction::compact(
        &fixture.engine(),
        &fixture.store.db,
        &fixture.id,
        "fixture",
        "gpt-6-astra",
        "",
    )
    .await
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("kept coming back empty"), "{text}");
    assert!(text.contains("cannot be split further"), "{text}");
    assert!(text.contains("original history retained"), "{text}");
    assert!(!text.contains("output limit"), "{text}");
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
    // The summary, its retry, then one split level with retries: the depth
    // limit ends the branch instead of repeating the same request.
    assert_eq!(bodies(&fixture).len(), 6, "{}", bodies(&fixture).len());
    assert_eq!(
        cached_checkpoint(&fixture),
        None,
        "an unusable result must never be cached"
    );
}

#[tokio::test]
async fn an_empty_fragment_failure_keeps_the_other_fragments_and_reuses_them() {
    for format in ["openai", "anthropic"] {
        // The first planned fragment carries a block that answers empty at
        // every depth, so it spends its own allowance; the notes behind it
        // finish and are cached for the retry.
        let fixture = start(Config::new(
            format,
            128_000,
            8192,
            Behavior::EmptyContaining("ALPHA"),
            History::Poisoned {
                poison: 30_000,
                rest: 40_000,
            },
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        let error = compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("compaction fragments failed"),
            "{format}: {text}"
        );
        assert!(
            text.contains("original history retained"),
            "{format}: {text}"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap(),
            before,
            "{format}"
        );

        let first_run = sent_log(&fixture);
        let bodies_before = bodies(&fixture);
        // Only the fragments that carry no poisoned block can complete; the
        // poisoned branch answers empty at every depth, so its requests are
        // retried rather than reused.
        let completed: Vec<Value> = first_run
            .iter()
            .zip(bodies_before.iter())
            .filter(|(entry, body)| {
                entry.origin == "original"
                    && entry.stage == "fragment"
                    && !entry.truncated
                    && !body.to_string().contains("ALPHA")
            })
            .map(|(_, body)| body.clone())
            .collect();
        assert!(!completed.is_empty(), "{format}: {first_run:?}");

        // The retry reuses the completed fragments and retries only the one
        // that answered empty; an empty body is never cached.
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap_err();
        let replay = bodies(&fixture)[bodies_before.len()..]
            .iter()
            .filter(|body| completed.contains(body))
            .count();
        assert_eq!(
            replay, 0,
            "{format}: a completed fragment must come from the checkpoint"
        );
        assert!(
            bodies(&fixture)[bodies_before.len()..]
                .iter()
                .any(|body| body.to_string().contains("ALPHA")),
            "{format}: the failed fragment must be requested again"
        );
    }
}

#[tokio::test]
async fn an_oversized_fragment_splits_at_the_configured_budget_without_retrying() {
    // The answer passes the character limit. No output budget can shorten it,
    // so the branch splits at the configured budget instead of retrying the
    // same request or escalating.
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::OversizedAbove(40_000),
            History::Marked,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap();
        let requests = bodies(&fixture);
        let log = sent_log(&fixture);
        assert_eq!(log[0].stage, "fragment", "{format}: {log:?}");
        assert!(!log[0].truncated, "{format}: {log:?}");
        assert_eq!(log[0].budget, 16_384, "{format}");
        // The oversized answer is not repeated.
        assert_eq!(
            requests.iter().filter(|body| **body == requests[0]).count(),
            1,
            "{format}: the oversized answer must not be retried"
        );
        assert!(
            log.iter().all(|entry| entry.budget == 16_384),
            "{format}: {log:?}"
        );
        assert_eq!(
            (log[1].stage, log[2].stage, log[1].budget),
            ("fragment", "fragment", 16_384),
            "{format}: {log:?}"
        );
        assert!(!log[1].truncated && !log[2].truncated, "{format}: {log:?}");
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1,
            "{format}"
        );
    }
}

#[tokio::test]
async fn persistent_empty_summaries_keep_the_finish_reason_and_sizes_in_the_final_error() {
    // The user-visible error must keep what the failed response reported, not
    // just the category: finish reason, character count, budget and usage.
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::EmptyAlways,
            History::Short,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        let error = compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("kept coming back empty"), "{format}: {text}");
        let finish = if format == "anthropic" {
            "end_turn"
        } else {
            "stop"
        };
        assert!(
            text.contains(&format!("finish reason `{finish}`, 0 characters")),
            "{format}: {text}"
        );
        assert!(
            text.contains("16384-token output budget"),
            "{format}: {text}"
        );
        // A provider that reports no usage says so instead of claiming zero.
        if format == "anthropic" {
            assert!(
                text.contains("1 output tokens reported"),
                "{format}: {text}"
            );
        } else {
            assert!(
                text.contains("output usage not reported"),
                "{format}: {text}"
            );
        }
        assert!(text.contains("cannot be split further"), "{format}: {text}");
        assert!(
            text.contains("original history retained"),
            "{format}: {text}"
        );
        assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
    }
}

#[tokio::test]
async fn persistent_oversized_summaries_keep_the_limit_and_sizes_in_the_final_error() {
    for format in ["openai", "anthropic"] {
        // Every answer is one character over the limit, whatever the budget.
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::OversizedAbove(0),
            History::Short,
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        let error = compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("kept returning an unusable body"),
            "{format}: {text}"
        );
        assert!(
            text.contains("above the 32,000-character limit"),
            "{format}: {text}"
        );
        assert!(text.contains("32001 characters"), "{format}: {text}");
        let finish = if format == "anthropic" {
            "end_turn"
        } else {
            "stop"
        };
        assert!(
            text.contains(&format!("finish reason `{finish}`")),
            "{format}: {text}"
        );
        assert!(text.contains("cannot be split further"), "{format}: {text}");
        assert!(
            text.contains("original history retained"),
            "{format}: {text}"
        );
        // An oversized body is never retried unchanged and never escalates.
        let log = sent_log(&fixture);
        assert!(
            log.iter().all(|entry| entry.budget == 16_384),
            "{format}: {log:?}"
        );
        assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
        assert_eq!(cached_checkpoint(&fixture), None, "{format}");
    }
}

/// The documented repro of a lost diagnostic: many fragments leave one recovery
/// generation each, so the split of an oversized answer is refused before it is
/// sent. The refusal must still name that answer.
#[tokio::test]
async fn an_oversized_response_that_runs_out_of_allowance_still_reports_its_details() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            8_000,
            4096,
            Behavior::OversizedAbove(0),
            History::Unique(80_000),
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        let error = compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        // Every fragment that ran out of allowance must keep the response that
        // needed the unsent request, not just the allowance wording.
        let refusals: Vec<&str> = text
            .split(" | ")
            .filter(|line| line.contains("allowance is spent"))
            .collect();
        assert!(
            !refusals.is_empty(),
            "{format}: the fixture must exhaust an allowance: {text}"
        );
        assert!(
            refusals.len() > 1,
            "{format}: many fragments share the allowance: {}",
            refusals.len()
        );
        for line in &refusals {
            assert!(
                line.contains("the response that needed that recovery ended at"),
                "{format}: the allowance refusal lost its cause: {line}"
            );
            assert!(line.contains("32001 characters"), "{format}: {line}");
            assert!(
                line.contains("32,000-character limit") || line.contains("32,000-character"),
                "{format}: {line}"
            );
            assert!(
                line.contains("finish reason `stop`") || line.contains("finish reason `end_turn`"),
                "{format}: {line}"
            );
            if format == "anthropic" {
                assert!(
                    line.contains("1 output tokens reported"),
                    "{format}: {line}"
                );
            } else {
                assert!(
                    line.contains("output usage not reported"),
                    "{format}: {line}"
                );
            }
            assert!(
                line.contains("original history retained"),
                "{format}: {line}"
            );
        }
        // The retry policy, cache and boundary rules are unchanged.
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap(),
            before,
            "{format}"
        );
        assert_eq!(cached_checkpoint(&fixture), None, "{format}");
        // Other independent fragments kept running.
        assert!(bodies(&fixture).len() > 128, "{format}");
    }
}

/// The same refusal after an empty body was retried once: the reported cause is
/// the empty response, not an output limit.
#[tokio::test]
async fn a_retried_empty_response_that_runs_out_of_allowance_still_reports_its_details() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            8_000,
            4096,
            Behavior::EmptyAlways,
            History::Unique(80_000),
        ))
        .await;
        let before = fixture.store.history(&fixture.id).unwrap();
        let error = compaction::compact(
            &fixture.engine(),
            &fixture.store.db,
            &fixture.id,
            "fixture",
            "gpt-6-astra",
            "",
        )
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        let refusals: Vec<&str> = text
            .split(" | ")
            .filter(|line| line.contains("allowance is spent"))
            .collect();
        assert!(
            !refusals.is_empty(),
            "{format}: the fixture must exhaust an allowance: {text}"
        );
        for line in &refusals {
            assert!(
                line.contains("the response that needed that recovery ended at"),
                "{format}: {line}"
            );
            assert!(line.contains("0 characters"), "{format}: {line}");
            assert!(
                line.contains("finish reason `stop`") || line.contains("finish reason `end_turn`"),
                "{format}: {line}"
            );
            assert!(
                line.contains("original history retained"),
                "{format}: {line}"
            );
        }
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap(),
            before,
            "{format}"
        );
        assert_eq!(cached_checkpoint(&fixture), None, "{format}");
    }
}

#[tokio::test]
async fn invalid_recovery_cancel_grow_restart_reuses_only_completed_requests() {
    // The first fragment recovers by splitting; cancel its merge after both
    // children are cached, then append real history and reopen the engine.
    grown_history_case(true, Behavior::EmptyAbove(12_000), 5, 2..4).await;
    grown_history_case(true, Behavior::OversizedAbove(12_000), 4, 1..3).await;
}
