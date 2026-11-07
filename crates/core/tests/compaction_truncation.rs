//! Bounded recovery when a summary hits its output limit, against local fixtures.
use axum::{routing::post, Json, Router};
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
    /// Truncate anything below the configured 16k ceiling, then finish.
    RaiseToCeiling,
    /// Truncate fragment requests below the ceiling; the raised retry finishes.
    RaiseFragment,
    /// Truncate inputs above this many tokens; smaller inputs finish.
    TruncateAbove(u64),
    /// Truncate like `TruncateAbove`, answering with a 32k-character summary.
    LongSummaryAbove(u64),
    /// Truncate requests below this output budget whose input is larger than
    /// this many tokens, so the raised retry of a smaller input finishes.
    SmallInputBelowBudget { tokens: u64, budget: u64 },
    /// Always report the output limit.
    AlwaysTruncate,
    /// Split a large original fragment, then truncate the merge once at its
    /// initial budget; the raised merge finishes.
    SplitThenMergeRaise,
    /// Split a large original fragment and truncate every merge request until
    /// the flag is cleared, so a retry can reuse the completed sub-summaries.
    SplitThenMergeSwitch,
    /// Split every large original fragment and truncate only the fourth merge
    /// at its initial budget, so its raise meets an already spent allowance.
    SplitThenMergeLastAllowance,
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

struct Fixture {
    dir: tempfile::TempDir,
    store: SessionStore,
    id: String,
    engine: Option<NativeEngine>,
    requests: Arc<Mutex<Vec<Value>>>,
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
    }
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
    let stop_at = Arc::new(AtomicUsize::new(config.stop_at));
    let hang_at = Arc::new(AtomicUsize::new(0));
    let merge_truncate = Arc::new(AtomicUsize::new(1));
    // Which merge request this is, so one case can truncate a chosen merge.
    let merges = Arc::new(AtomicUsize::new(0));
    let captured = requests.clone();
    let captured_stop = stop_at.clone();
    let captured_hang = hang_at.clone();
    let captured_merge = merge_truncate.clone();
    let captured_merges = merges.clone();
    let anthropic = config.format == "anthropic";
    let window = u64::from(config.window);
    let (behavior, delay) = (config.behavior, config.delay);
    let handler = move |Json(body): Json<Value>| {
        let captured = captured.clone();
        let captured_stop = captured_stop.clone();
        let captured_hang = captured_hang.clone();
        let captured_merge = captured_merge.clone();
        let captured_merges = captured_merges.clone();
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
            let merge_flag = captured_merge.load(Ordering::SeqCst) == 1;
            let merge_index = if stage == "merge" {
                captured_merges.fetch_add(1, Ordering::SeqCst) + 1
            } else {
                0
            };
            let truncate = match behavior {
                Behavior::RaiseToCeiling => budget < 16_384,
                Behavior::RaiseFragment => stage == "fragment" && budget < 8_192,
                Behavior::TruncateAbove(limit) => tokens > limit,
                Behavior::LongSummaryAbove(limit) => tokens > limit,
                Behavior::SmallInputBelowBudget {
                    tokens: limit,
                    budget: ceiling,
                } => tokens > limit && budget < ceiling,
                Behavior::AlwaysTruncate => true,
                // The large original fragment is the one the planner fills; its
                // recovered children are far smaller and must finish.
                Behavior::SplitThenMergeRaise => {
                    (stage == "fragment" && tokens > 80_000) || (stage == "merge" && budget < 8_192)
                }
                Behavior::SplitThenMergeSwitch => {
                    (stage == "fragment" && tokens > 80_000) || (stage == "merge" && merge_flag)
                }
                // Three merges spend the allowance; the fourth is truncated at
                // its initial budget, where raising would need a thirteenth
                // recovery.
                Behavior::SplitThenMergeLastAllowance => {
                    (stage == "fragment" && tokens > 80_000)
                        || (stage == "merge" && merge_index == 4)
                }
            };
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
            let text = match (finish, behavior) {
                // Truncated text must never reach the cache or the history.
                (1, _) => format!("TRUNCATED #{number}"),
                (_, Behavior::LongSummaryAbove(_) | Behavior::SmallInputBelowBudget { .. }) => {
                    "s".repeat(32_000)
                }
                // Long enough that merging the two children is a real input
                // which can itself be split after another truncation.
                (_, Behavior::SplitThenMergeSwitch) if stage == "fragment" => {
                    format!("{label} #{number} {}", "s".repeat(1_400))
                }
                _ => format!("{label} #{number}"),
            };
            Json(if anthropic {
                json!({"id":"r","type":"message","role":"assistant","model":"gpt-6-astra","content":[{"type":"text","text":text}],"stop_reason":match finish { 1 => "max_tokens", 2 => "refusal", _ => "end_turn" },"usage":{"input_tokens":1,"output_tokens":1}})
            } else {
                json!({"id":"r","object":"chat.completion","created":0,"model":"gpt-6-astra","choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":match finish { 1 => "length", 2 => "content_filter", _ => "stop" }}]})
            })
        }
    };
    let app = Router::new().route(
        if anthropic {
            "/v1/messages"
        } else {
            "/v1/chat/completions"
        },
        post(handler),
    );
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
async fn truncated_final_summary_raises_its_budget_and_its_length_target() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::RaiseToCeiling,
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
        assert_eq!(sent.len(), 2, "{format}");
        assert_eq!(sent[0]["max_tokens"], 8192);
        assert!(prompt_of(&sent[0]).contains(FINAL_PROMPT));
        assert!(
            prompt_of(&sent[0]).contains("Aim for about 4096 tokens"),
            "{format}"
        );
        assert_eq!(sent[1]["max_tokens"], 16_384);
        assert!(prompt_of(&sent[1]).contains(FINAL_PROMPT));
        // The prompt follows the budget it is actually sent with.
        assert!(
            prompt_of(&sent[1]).contains("Aim for about 8192 tokens"),
            "{format}"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1
        );
    }
}

#[tokio::test]
async fn truncated_fragment_raises_its_budget_and_its_length_target() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::RaiseFragment,
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
        let fragment = |budget: u64| {
            sent.iter()
                .find(|body| {
                    body["max_tokens"] == budget && prompt_of(body).contains(FRAGMENT_PROMPT)
                })
                .unwrap_or_else(|| panic!("{format}: no {budget}-token fragment request"))
        };
        assert!(
            prompt_of(fragment(4096)).contains("Aim for about 2048 tokens"),
            "{format}"
        );
        assert!(
            prompt_of(fragment(8192)).contains("Aim for about 4096 tokens"),
            "{format}"
        );
        assert!(
            sent.iter().all(|body| body["max_tokens"] != 16_384),
            "{format}: a fragment should raise once"
        );
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
        assert!(sent.len() > 3, "{format}");
        assert!(
            sent.iter().all(|body| body["max_tokens"] == 4096),
            "{format}: the model cap must not be raised"
        );
        // The truncated fragment is summarized as two smaller, ordered halves.
        let halves: Vec<&Value> = sent
            .iter()
            .filter(|body| {
                let tokens = ContextUsage::from_body(
                    if format == "anthropic" {
                        "anthropic-messages"
                    } else {
                        "openai-completions"
                    },
                    body,
                    Some(128_000),
                    4096,
                )
                .text_subtotal();
                (30_000..=60_000).contains(&tokens)
            })
            .collect();
        assert_eq!(halves.len(), 2, "{format}");
        let merge = sent
            .iter()
            .find(|body| prompt_of(body).contains(MERGE_PROMPT))
            .expect("a merge request follows the split halves");
        let merged = prompt_of(merge);
        assert!(merged.contains("summary-first"), "{format}");
        assert!(merged.contains("summary-second"), "{format}");
        assert!(
            merged.find("summary-first") < merged.find("summary-second"),
            "{format}"
        );
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
async fn a_truncated_merge_raises_again_and_reports_its_own_stage() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::SplitThenMergeRaise,
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
        assert_eq!(sent.len(), 7, "{format}");
        // The original fragment is truncated first, then its two children, then
        // the merge of those children, then the remaining fragment and the
        // final summary.
        assert_eq!(
            stages(&sent),
            ["fragment", "fragment", "fragment", "merge", "merge", "fragment", "final"],
            "{format}"
        );
        assert_eq!(sent[0]["max_tokens"], 4096, "{format}");
        // The merge is truncated at its own initial budget and finishes after
        // one raise, with the prompt rebuilt for the larger target.
        assert_eq!(sent[3]["max_tokens"], 4096, "{format}");
        assert!(
            prompt_of(&sent[3]).contains("Aim for about 2048 tokens"),
            "{format}"
        );
        assert_eq!(sent[4]["max_tokens"], 8192, "{format}");
        assert!(
            prompt_of(&sent[4]).contains("Aim for about 4096 tokens"),
            "{format}"
        );
        let merged = prompt_of(&sent[3]);
        let first = merged
            .find("summary-first #2")
            .expect("first child summary");
        let second = merged
            .find("summary-first #3")
            .expect("second child summary");
        assert!(first < second, "{format}: the merge keeps the child order");
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1
        );
    }
}

#[tokio::test]
async fn a_persistently_truncated_merge_stops_at_the_depth_limit_and_reuses_children() {
    for format in ["openai", "anthropic"] {
        let fixture = start(Config::new(
            format,
            128_000,
            16_384,
            Behavior::SplitThenMergeSwitch,
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
        // The merge itself hits the depth limit; the report keeps the merge stage,
        // the planned budget and the last budget actually sent.
        assert!(text.contains(" merge "), "{text}");
        assert!(text.contains("depth limit"), "{text}");
        assert!(text.contains("6 of 12 recovery generations"), "{text}");
        assert!(
            text.contains("still exceeded its 4096-token output limit"),
            "{text}"
        );
        assert!(
            text.contains("last request actually sent used a 8192-token output budget"),
            "{text}"
        );
        assert!(text.contains("original history retained"), "{text}");
        assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
        let first_run = bodies(&fixture);
        assert_eq!(first_run.len(), 7, "{first_run:?}");
        assert_eq!(
            stages(&first_run),
            ["fragment", "fragment", "fragment", "merge", "merge", "merge", "merge"]
        );

        // Once merges finish again, the completed child summaries come from the
        // checkpoint instead of the model, and the original history is replaced by
        // one summary only after the whole compaction succeeds.
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
        let sent = fixture.requests.lock().unwrap();
        let replay = sent[first_run.len()..]
            .iter()
            .filter(|body| **body == first_run[1] || **body == first_run[2])
            .count();
        assert_eq!(
            replay, 0,
            "{format}: the completed child summaries must be reused"
        );
        assert_eq!(
            fixture.store.history(&fixture.id).unwrap().len(),
            before.len() + 1,
            "{format}"
        );
    }
}

#[tokio::test]
async fn persistent_truncation_stops_within_the_allowance_and_never_caches_truncated_text() {
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
    assert!(
        bodies(&fixture).len() <= 1 + 12,
        "the recovery allowance must bound every request"
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
async fn recovery_allowance_caps_a_compaction_at_twelve_extra_generations() {
    let fixture = start(Config::new(
        "openai",
        64_000,
        4096,
        Behavior::TruncateAbove(30_000),
        History::Unique(130_000),
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
    assert!(text.contains("12 of 12 recovery generations"), "{text}");
    assert!(text.contains("original history retained"), "{text}");
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
    // Four fragments recover with three generations each; the fifth is
    // rejected before its split, so no fifth recovery generation is sent.
    assert_eq!(bodies(&fixture).len(), 4 + 12 + 1);
}

#[tokio::test]
async fn a_spent_allowance_names_the_raised_budget_it_did_not_send() {
    // Three fragments spend the allowance on their children and merges. The
    // fourth fragment's merge is truncated at its initial budget, where the
    // recovery rule would raise to 8192 instead of splitting; that raise needs
    // a thirteenth generation, so it must never be sent.
    let fixture = start(Config::new(
        "openai",
        128_000,
        16_384,
        Behavior::SplitThenMergeLastAllowance,
        History::Unique(200_000),
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
    assert!(text.contains(" merge "), "{text}");
    assert!(
        text.contains("stopped after 12 of 12 recovery generations"),
        "{text}"
    );
    // The next request would have been the raised merge, not another 4096 one.
    assert!(
        text.contains("its next 8192-token request was not sent"),
        "{text}"
    );
    assert!(
        text.contains("last request actually sent used a 4096-token output budget"),
        "{text}"
    );
    assert!(text.contains("original history retained"), "{text}");
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
    let sent = bodies(&fixture);
    assert_eq!(
        stages(&sent)
            .iter()
            .filter(|stage| **stage == "merge")
            .count(),
        4,
        "{:?}",
        stages(&sent)
    );
    assert!(
        sent.iter().all(|body| body["max_tokens"] == 4096),
        "the raised merge must not be sent"
    );
}

#[tokio::test]
async fn exhausted_recovery_reports_the_last_actual_budget() {
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
    // The summary was planned at 8k and the last retry actually sent 16k, so
    // both numbers are reported, each under its own label.
    assert!(
        text.contains("still exceeded its 8192-token output limit"),
        "{text}"
    );
    assert!(
        text.contains("last request actually sent used a 16384-token output budget"),
        "{text}"
    );
    assert!(text.contains("depth limit"), "{text}");
    assert!(text.contains("of 12 recovery generations"), "{text}");
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
    assert!(text.contains("of 12 recovery generations"), "{text}");
    assert!(text.contains("original history retained"), "{text}");
    // A non-truncation stop never launches another split.
    assert_eq!(bodies(&fixture).len(), 2, "{text}");
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
}

#[tokio::test]
async fn a_merge_above_the_safe_budget_fails_before_it_is_sent() {
    // The summaries are long enough that merging them no longer fits the same
    // 90% line every other summary request obeys, at or below the window. The
    // third case raises a child budget first, so the merge report must not pass
    // that raised budget off as its own planned one.
    for (window, max_tokens, behavior, history, last_budget) in [
        (
            29_000,
            4096,
            Behavior::LongSummaryAbove(11_000),
            History::Evidence(60_000),
            None,
        ),
        (
            20_000,
            4096,
            Behavior::LongSummaryAbove(11_000),
            History::Evidence(60_000),
            None,
        ),
        (
            29_000,
            16_384,
            Behavior::SmallInputBelowBudget {
                tokens: 9_000,
                budget: 8_192,
            },
            // One fragment, so the merge is the first recovery request that
            // runs out of input budget instead of the allowance running out.
            History::Evidence(6_400),
            Some(8192),
        ),
    ] {
        let fixture = start(Config::new("openai", window, max_tokens, behavior, history)).await;
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
            text.contains("safe context budget"),
            "window {window}: {text}"
        );
        // The failing stage is the merge, not the fragment that needed it.
        assert!(text.contains(" merge "), "window {window}: {text}");
        assert!(
            text.contains("planned for its 4096-token output"),
            "window {window}: {text}"
        );
        assert!(
            text.contains("no request was sent for this input"),
            "window {window}: {text}"
        );
        match last_budget {
            Some(budget) => assert!(
                text.contains(&format!(
                    "last request actually sent used a {budget}-token output budget"
                )),
                "window {window}: {text}"
            ),
            None => assert!(
                !text.contains("last request actually sent"),
                "window {window}: {text}"
            ),
        }
        assert!(
            text.contains("original history retained"),
            "window {window}: {text}"
        );
        assert!(
            bodies(&fixture)
                .iter()
                .all(|body| !prompt_of(body).contains(MERGE_PROMPT)),
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
async fn a_window_too_small_for_the_prompt_fails_without_a_request() {
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
    // Planning failed before any request: the report still names the stage,
    // the budget, the recoveries used and the retained history.
    assert!(
        text.contains("too small for conversation summarization"),
        "{text}"
    );
    assert!(text.contains("for fragment planning"), "{text}");
    assert!(
        text.contains("safe context budget for its 4096-token output"),
        "{text}"
    );
    assert!(text.contains("0 of 12 recovery generations"), "{text}");
    assert!(
        text.contains("no request was sent for the remaining input"),
        "{text}"
    );
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
        Behavior::RaiseToCeiling,
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
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["max_tokens"], 8192);
    assert_eq!(sent[1]["max_tokens"], 16_384);
    assert_eq!(fixture.store.history(&fixture.id).unwrap(), before);
    let branch_history = fixture.store.history(&branch).unwrap();
    let summary = branch_history.last().unwrap()["content"].as_str().unwrap();
    assert!(
        summary.contains("[Summary of the later conversation"),
        "{summary}"
    );
}
