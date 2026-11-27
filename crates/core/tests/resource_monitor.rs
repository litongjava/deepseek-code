//! End-to-end tests for the built-in resource monitor inside the native engine.
//!
//! The monitor is driven by an injectable, deterministic sampler so these tests
//! never depend on real host load. A local mock provider records each model
//! request, which lets the tests prove that a warning is delivered to the model
//! mid-turn rather than to an idle session queue.
use axum::{http::HeaderMap, response::IntoResponse, routing::post, Json, Router};
use deepseek_code_core::{
    cache::FileCache,
    native_engine::{
        engine_core::ChatRequest,
        provider_manager::{ApiFormat, Provider, ProviderManager},
        tool_loop::EngineEvent,
    },
    permissions::PermissionMode,
    runtime::{self, RuntimePaths, SessionStore},
    services::resource_monitor::{
        ResourceKind, ResourceMonitor, ResourceMonitorConfig, ResourceSample, SystemSampler,
    },
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Protocol {
    OpenAiChat,
    Anthropic,
}

fn openai_tool_sse(file_path: &str) -> String {
    let delta = json!({
        "tool_calls": [{
            "index": 0,
            "id": "call-read-1",
            "type": "function",
            "function": {"name": "Read", "arguments": json!({"file_path": file_path}).to_string()}
        }]
    });
    let first = json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let second = json!({
        "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":300,"completion_tokens":10,"total_tokens":310}
    });
    format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n")
}

fn openai_text_sse(text: &str) -> String {
    let first = json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]});
    let second = json!({
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":400,"completion_tokens":10,"total_tokens":410}
    });
    format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n")
}

fn anthropic_tool_sse(file_path: &str) -> String {
    let input = json!({"file_path": file_path});
    [
        json!({"type":"message_start","message":{"id":"m","usage":{"input_tokens":300}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_read_1","name":"Read","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

fn anthropic_text_sse(text: &str) -> String {
    [
        json!({"type":"message_start","message":{"id":"m","usage":{"input_tokens":400}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

/// Returns a healthy CPU sample for the first `healthy_calls` invocations and a
/// breaching one afterwards. `healthy_calls = 0` models sustained pressure.
#[derive(Clone)]
struct ScriptedSampler {
    calls: Arc<AtomicUsize>,
    healthy_calls: usize,
}

impl ScriptedSampler {
    fn new(healthy_calls: usize) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                calls: calls.clone(),
                healthy_calls,
            },
            calls,
        )
    }
}

impl SystemSampler for ScriptedSampler {
    fn sample(&mut self, _disk_targets: &[(String, PathBuf)]) -> ResourceSample {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let cpu_percent = if call > self.healthy_calls { 99.0 } else { 5.0 };
        ResourceSample {
            cpu_percent: Some(cpu_percent),
            ..ResourceSample::default()
        }
    }
}

/// Breaches for the first `breach_calls` invocations, then reports healthy CPU.
/// Models a host spike that recovers while no turn is running.
#[derive(Clone)]
struct IdleSpikeSampler {
    calls: Arc<AtomicUsize>,
    breach_calls: usize,
}

impl IdleSpikeSampler {
    fn new(breach_calls: usize) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                calls: calls.clone(),
                breach_calls,
            },
            calls,
        )
    }
}

impl SystemSampler for IdleSpikeSampler {
    fn sample(&mut self, _disk_targets: &[(String, PathBuf)]) -> ResourceSample {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let cpu_percent = if call <= self.breach_calls { 99.0 } else { 5.0 };
        ResourceSample {
            cpu_percent: Some(cpu_percent),
            ..ResourceSample::default()
        }
    }
}

fn monitor_config(cooldown_secs: u64) -> ResourceMonitorConfig {
    ResourceMonitorConfig {
        enabled: true,
        interval_secs: 2,
        cpu_percent_threshold: 95.0,
        cpu_consecutive: 1,
        cooldown_secs,
        ..ResourceMonitorConfig::default()
    }
}

struct Scenario {
    protocol: Protocol,
    healthy_calls: usize,
    /// When set, breach for this many samples and then stay healthy instead of
    /// using `healthy_calls`.
    initial_spike_calls: Option<usize>,
    tool_steps: usize,
    /// Delay the first response so a monitor tick is guaranteed to land between
    /// the first request being assembled and the follow-up request.
    first_response_delay: Duration,
    cooldown_secs: u64,
}

struct Outcome {
    _dir: tempfile::TempDir,
    calls: Vec<Value>,
    events: Vec<EngineEvent>,
}

async fn wait_for_samples(calls: &Arc<AtomicUsize>, expected: usize) {
    for _ in 0..500 {
        if calls.load(Ordering::SeqCst) >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the resource monitor sampled {} times, expected at least {expected}",
        calls.load(Ordering::SeqCst)
    );
}

/// Waits until the healthy tick has fully processed its recovery. The monitor
/// publishes the latest sample after pruning, so observing it removes the race
/// between the sampler call and the pruner.
async fn wait_for_recovery(monitor: &ResourceMonitor) {
    for _ in 0..500 {
        if monitor
            .latest_sample()
            .and_then(|sample| sample.cpu_percent)
            .is_some_and(|percent| percent < 50.0)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the resource monitor never reported recovery");
}

async fn run(scenario: Scenario) -> Outcome {
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("fixture.txt");
    std::fs::write(&fixture, "fixture contents for the resource monitor test\n").unwrap();
    let fixture_path = fixture.to_string_lossy().into_owned();

    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store
        .create(dir.path(), "fixture-model", "fixture")
        .unwrap();
    store
        .append(&id, "user", "Read the fixture and answer")
        .unwrap();
    let mut turn = store.db.start_turn_timer(&id).unwrap();
    let assistant_id = turn.ensure_segment().unwrap();

    let recorded: Arc<Mutex<Vec<Value>>> = Arc::default();
    let recorded_clone = recorded.clone();
    let step = Arc::new(AtomicUsize::new(0));
    let step_clone = step.clone();
    let protocol = scenario.protocol;
    let tool_steps = scenario.tool_steps;
    let delay = scenario.first_response_delay;
    let app = Router::new().fallback(post(move |_headers: HeaderMap, Json(body): Json<Value>| {
        let recorded = recorded_clone.clone();
        let step = step_clone.clone();
        let fixture_path = fixture_path.clone();
        async move {
            let index = step.fetch_add(1, Ordering::SeqCst);
            recorded.lock().unwrap().push(body);
            if index == 0 && !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let tool_turn = index < tool_steps;
            match (protocol, tool_turn) {
                (Protocol::OpenAiChat, true) => (
                    [("content-type", "text/event-stream")],
                    openai_tool_sse(&fixture_path),
                )
                    .into_response(),
                (Protocol::OpenAiChat, false) => (
                    [("content-type", "text/event-stream")],
                    openai_text_sse("FINAL_ANSWER"),
                )
                    .into_response(),
                (Protocol::Anthropic, true) => (
                    [("content-type", "text/event-stream")],
                    anthropic_tool_sse(&fixture_path),
                )
                    .into_response(),
                (Protocol::Anthropic, false) => (
                    [("content-type", "text/event-stream")],
                    anthropic_text_sse("FINAL_ANSWER"),
                )
                    .into_response(),
            }
        }
    }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (api_format, base_url) = match scenario.protocol {
        Protocol::OpenAiChat => ("openai", format!("http://{addr}/v1")),
        Protocol::Anthropic => ("anthropic", format!("http://{addr}")),
    };
    let provider: Provider = serde_json::from_value(json!({
        "id": "fixture",
        "name": "Fixture",
        "api_format": api_format,
        "base_url": base_url,
        "api_key": "fixture-key",
        "enabled": true,
        "models": [{
            "id": "fixture-model",
            "name": "fixture-model",
            "enabled": true,
            "max_tokens": 1000,
            "context_window": 32000,
            "supports_web_search": false
        }]
    }))
    .unwrap();

    let (sampler, samples): (Box<dyn SystemSampler>, Arc<AtomicUsize>) =
        match scenario.initial_spike_calls {
            Some(breach_calls) => {
                let (sampler, calls) = IdleSpikeSampler::new(breach_calls);
                (Box::new(sampler), calls)
            }
            None => {
                let (sampler, calls) = ScriptedSampler::new(scenario.healthy_calls);
                (Box::new(sampler), calls)
            }
        };
    let monitor = ResourceMonitor::start_with_sampler(
        monitor_config(scenario.cooldown_secs),
        dir.path().to_path_buf(),
        paths.data_dir.clone(),
        sampler,
    );
    assert!(monitor.is_enabled());
    // The first tick is immediate; wait for it so the turn starts from a known
    // sampler phase instead of racing the background task. A recovering idle
    // spike also needs its recovery tick fully processed before the turn.
    let expected_samples = if scenario.initial_spike_calls.is_some() {
        2
    } else {
        1
    };
    wait_for_samples(&samples, expected_samples).await;
    if scenario.initial_spike_calls.is_some() {
        wait_for_recovery(&monitor).await;
    }

    let permissions = runtime::default_permissions();
    permissions.set_mode(PermissionMode::BypassPermissions);
    let engine = runtime::create_engine(
        &paths,
        store.db.clone(),
        Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
            vec![provider],
            paths.data_dir.join("config/providers.json"),
        ))),
        permissions,
        Arc::new(FileCache::new()),
    )
    .with_resource_monitor(monitor.clone());

    let format = match scenario.protocol {
        Protocol::Anthropic => ApiFormat::Anthropic,
        _ => ApiFormat::OpenAI,
    };
    let request: ChatRequest = serde_json::from_value(json!({
        "conversation_id": id,
        "messages": store.model_history(&id, &format).unwrap(),
        "assistant_message_id": assistant_id,
        "standalone_frontend": true,
        "model": "fixture-model",
        "provider_id": "fixture",
        "workspace_path": dir.path(),
        "max_tokens": 1000
    }))
    .unwrap();

    let mut rx = engine
        .send_message_with_history(request, false)
        .await
        .unwrap();
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = rx.recv().await {
            assert!(
                !matches!(event, EngineEvent::Error(_)),
                "unexpected engine error: {event:?}"
            );
            events.push(event);
        }
    })
    .await
    .expect("the turn must finish");

    server.abort();
    let _ = server.await;
    let calls = recorded.lock().unwrap().clone();
    monitor.stop();
    Outcome {
        _dir: dir,
        calls,
        events,
    }
}

fn resource_alerts(
    events: &[EngineEvent],
) -> Vec<deepseek_code_core::services::resource_monitor::ResourceAlert> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::ResourceAlert(alert) => Some(alert.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn openai_mid_turn_injects_resource_alert() {
    let outcome = run(Scenario {
        protocol: Protocol::OpenAiChat,
        healthy_calls: 1,
        initial_spike_calls: None,
        tool_steps: 1,
        first_response_delay: Duration::from_millis(2600),
        cooldown_secs: 300,
    })
    .await;

    assert_eq!(outcome.calls.len(), 2, "one tool round and one final round");
    let first_messages = outcome.calls[0]["messages"].to_string();
    assert!(
        !first_messages.contains("<kind>system_resource</kind>"),
        "the healthy first iteration must not carry a warning: {first_messages}"
    );
    let second_messages = outcome.calls[1]["messages"].to_string();
    assert!(second_messages.contains("<task-notification>"));
    assert!(second_messages.contains("<kind>system_resource</kind>"));

    let alerts = resource_alerts(&outcome.events);
    assert_eq!(alerts.len(), 1, "exactly one alert event");
    assert_eq!(alerts[0].kind, ResourceKind::Cpu);
    assert_eq!(alerts[0].summary(), "High CPU usage: 99.0% sustained");
    assert!(alerts[0].details.contains("-j"));
}

#[tokio::test]
async fn anthropic_mid_turn_injects_resource_alert() {
    let outcome = run(Scenario {
        protocol: Protocol::Anthropic,
        healthy_calls: 1,
        initial_spike_calls: None,
        tool_steps: 1,
        first_response_delay: Duration::from_millis(2600),
        cooldown_secs: 300,
    })
    .await;

    assert_eq!(outcome.calls.len(), 2, "one tool round and one final round");
    let first_messages = outcome.calls[0]["messages"].to_string();
    assert!(
        !first_messages.contains("<kind>system_resource</kind>"),
        "the healthy first iteration must not carry a warning: {first_messages}"
    );
    let second_messages = outcome.calls[1]["messages"].to_string();
    assert!(second_messages.contains("<task-notification>"));
    assert!(second_messages.contains("<kind>system_resource</kind>"));

    let alerts = resource_alerts(&outcome.events);
    assert_eq!(alerts.len(), 1, "exactly one alert event");
    assert_eq!(alerts[0].kind, ResourceKind::Cpu);
    assert_eq!(alerts[0].summary(), "High CPU usage: 99.0% sustained");
}

#[tokio::test]
async fn cooldown_suppresses_alerts_across_iterations() {
    let outcome = run(Scenario {
        protocol: Protocol::OpenAiChat,
        healthy_calls: 0,
        initial_spike_calls: None,
        tool_steps: 2,
        first_response_delay: Duration::ZERO,
        cooldown_secs: 300,
    })
    .await;

    assert_eq!(
        outcome.calls.len(),
        3,
        "two tool rounds and one final round"
    );
    // The injected message stays in the in-memory conversation for the rest of
    // the turn, so every later request repeats it exactly once. A second
    // warning would make the count grow.
    for body in &outcome.calls {
        let messages = body["messages"].to_string();
        assert_eq!(
            messages.matches("<kind>system_resource</kind>").count(),
            1,
            "exactly one warning may be present: {messages}"
        );
    }
    assert_eq!(
        resource_alerts(&outcome.events).len(),
        1,
        "the cooldown must throttle repeated warnings"
    );
}

#[tokio::test]
async fn idle_spike_that_recovers_before_turn_is_not_injected() {
    let outcome = run(Scenario {
        protocol: Protocol::OpenAiChat,
        healthy_calls: 0,
        initial_spike_calls: Some(1),
        tool_steps: 1,
        first_response_delay: Duration::ZERO,
        cooldown_secs: 300,
    })
    .await;

    assert_eq!(outcome.calls.len(), 2, "one tool round and one final round");
    for body in &outcome.calls {
        let messages = body["messages"].to_string();
        assert!(
            !messages.contains("<kind>system_resource</kind>"),
            "a recovered idle spike must not reach a later turn: {messages}"
        );
    }
    assert!(
        resource_alerts(&outcome.events).is_empty(),
        "a recovered idle spike must not emit a resource alert"
    );
}

#[tokio::test]
async fn idle_session_buffers_alerts_without_waking_or_queueing_input() {
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store
        .create(dir.path(), "fixture-model", "fixture")
        .unwrap();
    store.append(&id, "user", "hello").unwrap();

    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let app = Router::new().fallback(post(move |Json(_body): Json<Value>| {
        let seen = seen.clone();
        async move {
            seen.fetch_add(1, Ordering::SeqCst);
            (
                [("content-type", "text/event-stream")],
                openai_text_sse("x"),
            )
                .into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (sampler, samples) = ScriptedSampler::new(0);
    let monitor = ResourceMonitor::start_with_sampler(
        monitor_config(300),
        dir.path().to_path_buf(),
        paths.data_dir.clone(),
        Box::new(sampler),
    );
    assert!(monitor.is_enabled());

    // No turn is started: the monitor ticks and buffers a warning on its own.
    wait_for_samples(&samples, 1).await;
    assert!(monitor.latest_sample().is_some());
    assert!(
        !monitor.drain_alerts().is_empty(),
        "the idle monitor must buffer its warning in-process"
    );

    assert!(
        store.db.next_desktop_input(&id).unwrap().is_none(),
        "an idle warning must never create a queued turn"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "an idle warning must never reach a provider"
    );

    monitor.stop();
    assert!(!monitor.is_enabled());
    let before = samples.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(
        samples.load(Ordering::SeqCst),
        before,
        "stop must cancel sampling"
    );
    monitor.stop();

    server.abort();
    let _ = server.await;
}
