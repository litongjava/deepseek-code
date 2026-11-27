//! Process-local, session-scoped hook callbacks. No callback registration is persisted.
use super::tool_hooks::HookEvent;
use anyhow::{ensure, Context, Result};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::Duration,
};

pub trait Host: Send + Sync {
    fn invoke<'a>(
        &'a self,
        callback: &'a str,
        input: Value,
        tool_use_id: Option<String>,
    ) -> BoxFuture<'a, Result<Value>>;
}

#[derive(Clone)]
pub struct Callback {
    pub event: HookEvent,
    pub matcher: String,
    pub id: String,
    pub timeout: Duration,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Matcher {
    #[serde(default)]
    matcher: String,
    hook_callback_ids: Vec<String>,
    #[serde(default, deserialize_with = "timeout_seconds")]
    timeout: Option<f64>,
}

fn timeout_seconds<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<f64>, D::Error> {
    f64::deserialize(deserializer).map(Some)
}

pub fn validate(value: &Value) -> Result<Vec<Callback>> {
    let groups: BTreeMap<String, Vec<Matcher>> = serde_json::from_value(value.clone())
        .context("initialize.hooks must map supported events to callback matcher arrays")?;
    let mut callbacks = Vec::new();
    for (name, groups) in groups {
        let event: HookEvent = serde_json::from_value(Value::String(name.clone()))
            .with_context(|| format!("Unsupported SDK hook event: {name}"))?;
        ensure!(!matches!(event,HookEvent::WorktreeCreate|HookEvent::WorktreeRemove),"SDK {name} callbacks require startup worktree/provenance support; use command hooks for these events");
        for group in groups {
            ensure!(
                group.matcher.len() <= 4096,
                "SDK hook matcher exceeds 4096 bytes"
            );
            if event != HookEvent::CwdChanged && !group.matcher.is_empty() && group.matcher != "*" {
                regex::Regex::new(&group.matcher).context("Invalid SDK hook matcher regex")?;
            }
            let seconds = group.timeout.unwrap_or(60.0);
            ensure!(
                seconds.is_finite() && seconds >= 0.0,
                "SDK hook timeout must be finite and nonnegative"
            );
            // The reference treats zero like an omitted timeout; fractional seconds are valid.
            let timeout = Duration::try_from_secs_f64(if seconds == 0.0 { 60.0 } else { seconds })
                .context("SDK hook timeout is out of range")?;
            for id in group.hook_callback_ids {
                ensure!(!id.trim().is_empty() && id.len()<=512 && !id.chars().any(char::is_control),"SDK hook callback ID must be nonempty and at most 512 bytes without control characters");
                callbacks.push(Callback {
                    event,
                    matcher: group.matcher.clone(),
                    id,
                    timeout,
                });
                ensure!(callbacks.len() <= 1024, "SDK hook callback limit is 1024");
            }
        }
    }
    Ok(callbacks)
}

pub struct Registration {
    callbacks: Vec<Callback>,
    host: Weak<dyn Host>,
    active: AtomicBool,
}
impl Registration {
    pub(crate) fn file_watch_matchers(&self) -> Vec<String> {
        if !self.active.load(Ordering::Acquire) {
            return vec![];
        }
        self.callbacks
            .iter()
            .filter(|c| c.event == HookEvent::FileChanged)
            .map(|c| c.matcher.clone())
            .collect()
    }
    pub fn has_callbacks(&self) -> bool {
        self.active.load(Ordering::Acquire) && !self.callbacks.is_empty()
    }
    pub fn matching(&self, event: HookEvent, name: &str) -> Vec<Callback> {
        if !self.active.load(Ordering::Acquire) {
            return vec![];
        }
        self.callbacks
            .iter()
            .filter(|callback| {
                callback.event == event
                    && (event == HookEvent::CwdChanged
                        || callback.matcher.is_empty()
                        || callback.matcher == "*"
                        || regex::Regex::new(&callback.matcher)
                            .is_ok_and(|matcher| matcher.is_match(name)))
            })
            .cloned()
            .collect()
    }
    pub async fn invoke(&self, callback: &Callback, input: Value) -> Result<Value> {
        ensure!(
            self.active.load(Ordering::Acquire),
            "SDK hook registration ended"
        );
        let host = self.host.upgrade().context("SDK hook host disconnected")?;
        let id = input["tool_use_id"].as_str().map(str::to_owned);
        let output = tokio::time::timeout(callback.timeout, host.invoke(&callback.id, input, id))
            .await
            .context("SDK hook callback timed out")??;
        ensure!(
            self.active.load(Ordering::Acquire),
            "SDK hook registration ended during callback"
        );
        ensure!(
            output.is_object(),
            "SDK hook callback output must be an object"
        );
        ensure!(
            output.get("async").is_none(),
            "Async SDK hook outputs are not implemented"
        );
        if let Some(specific) = output.get("hookSpecificOutput") {
            super::tool_hooks::watch_paths(callback.event, &output)?;
            ensure!(
                specific.get("initialUserMessage").is_none()
                    || callback.event == HookEvent::SessionStart,
                "initialUserMessage requires SessionStart"
            );
            ensure!(
                specific.get("updatedMCPToolOutput").is_none()
                    || callback.event == HookEvent::PostToolUse,
                "updatedMCPToolOutput requires PostToolUse"
            );
        }
        ensure!(
            serde_json::to_vec(&output)?.len() <= 1024 * 1024,
            "SDK hook output exceeds 1 MiB"
        );
        Ok(output)
    }
}
fn registry() -> &'static Mutex<HashMap<String, Weak<Registration>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Weak<Registration>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}
pub fn lookup(session: &str) -> Option<Weak<Registration>> {
    registry()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(session)
        .filter(|registration| registration.strong_count() > 0)
        .cloned()
}
pub struct Guard {
    session: String,
    registration: Arc<Registration>,
}
pub fn install(session: &str, callbacks: Vec<Callback>, host: Arc<dyn Host>) -> Guard {
    let registration = Arc::new(Registration {
        callbacks,
        host: Arc::downgrade(&host),
        active: AtomicBool::new(true),
    });
    let mut entries = registry().lock().unwrap_or_else(|error| error.into_inner());
    entries.retain(|_, registration| registration.strong_count() > 0);
    if let Some(previous) = entries
        .insert(session.into(), Arc::downgrade(&registration))
        .and_then(|entry| entry.upgrade())
    {
        previous.active.store(false, Ordering::Release);
    }
    Guard {
        session: session.into(),
        registration,
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.registration.active.store(false, Ordering::Release);
        let mut entries = registry().lock().unwrap_or_else(|error| error.into_inner());
        if entries
            .get(&self.session)
            .is_some_and(|entry| entry.ptr_eq(&Arc::downgrade(&self.registration)))
        {
            entries.remove(&self.session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    struct Echo;
    impl Host for Echo {
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            input: Value,
            _: Option<String>,
        ) -> BoxFuture<'a, Result<Value>> {
            Box::pin(async move { Ok(input) })
        }
    }
    #[test]
    fn callback_validation_uses_seconds_and_rejects_unimplemented_or_malformed_fields() {
        let callbacks=validate(&json!({"PreToolUse":[{"matcher":"^Write$","hookCallbackIds":["one"],"timeout":0.125},{"hookCallbackIds":["two"],"timeout":0},{"hookCallbackIds":["three"],"timeout":7200}]})).unwrap();
        assert_eq!(callbacks[0].timeout, Duration::from_millis(125));
        assert_eq!(callbacks[1].timeout, Duration::from_secs(60));
        assert_eq!(callbacks[2].timeout, Duration::from_secs(7200));
        for invalid in [
            json!({"WorktreeCreate":[]}),
            json!({"WorktreeRemove":[]}),
            json!({"Unknown":[]}),
            json!({"PreToolUse":[{"hookCallbackIds":[""]}]}),
            json!({"PreToolUse":[{"hookCallbackIds":["id"],"matcher":"["}]}),
            json!({"PreToolUse":[{"hookCallbackIds":["id"],"timeout":-1}]}),
            json!({"PreToolUse":[{"hookCallbackIds":["id"],"timeout":null}]}),
            json!({"PreToolUse":[{"hookCallbackIds":["id"],"timeout":1e100}]}),
            json!({"PreToolUse":[{"hookCallbackIds":["id"],"unknown":true}]}),
        ] {
            assert!(validate(&invalid).is_err(), "{invalid}");
        }
    }
    #[tokio::test]
    async fn inherited_weak_registration_is_revoked_without_retaining_host_or_parent_run() {
        let session = uuid::Uuid::new_v4().to_string();
        let host: Arc<dyn Host> = Arc::new(Echo);
        let callbacks =
            validate(&json!({"PreToolUse":[{"matcher":"^Write$","hookCallbackIds":["id"]}]}))
                .unwrap();
        let guard = install(&session, callbacks.clone(), host.clone());
        let registration = lookup(&session).unwrap().upgrade().unwrap();
        assert_eq!(
            registration.matching(HookEvent::PreToolUse, "Write").len(),
            1
        );
        assert!(registration
            .matching(HookEvent::PreToolUse, "Read")
            .is_empty());
        assert_eq!(
            registration.invoke(&callbacks[0], json!({})).await.unwrap(),
            json!({})
        );
        drop(guard);
        assert!(lookup(&session).is_none());
        assert!(registration.invoke(&callbacks[0], json!({})).await.is_err());
        assert!(registration
            .matching(HookEvent::PreToolUse, "Write")
            .is_empty());
        assert_eq!(Arc::strong_count(&host), 1);
        let replacement = install(&session, callbacks.clone(), host.clone());
        let registration = lookup(&session).unwrap().upgrade().unwrap();
        drop(host);
        assert!(registration.invoke(&callbacks[0], json!({})).await.is_err());
        drop(replacement);
    }
    #[tokio::test]
    async fn malformed_and_unsupported_callback_outputs_cannot_grant_permissions() {
        let host: Arc<dyn Host> = Arc::new(Echo);
        let callbacks = validate(&json!({"PreToolUse":[{"hookCallbackIds":["id"]}]})).unwrap();
        let guard = install(
            &uuid::Uuid::new_v4().to_string(),
            callbacks.clone(),
            host.clone(),
        );
        for invalid in [
            json!([]),
            json!({"async":true}),
            json!({"hookSpecificOutput":{"initialUserMessage":"injected"}}),
            json!({"hookSpecificOutput":{"updatedMCPToolOutput":{}}}),
        ] {
            assert!(guard
                .registration
                .invoke(&callbacks[0], invalid)
                .await
                .is_err());
        }
    }
    #[tokio::test]
    async fn parallel_callback_merge_keeps_deny_and_ask_stronger_than_allow() {
        struct Decisions;
        impl Host for Decisions {
            fn invoke<'a>(
                &'a self,
                callback: &'a str,
                _: Value,
                _: Option<String>,
            ) -> BoxFuture<'a, Result<Value>> {
                Box::pin(async move {
                    if callback == "allow" {
                        tokio::task::yield_now().await;
                    }
                    Ok(
                        json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":callback}}),
                    )
                })
            }
        }
        let session = uuid::Uuid::new_v4().to_string();
        let host: Arc<dyn Host> = Arc::new(Decisions);
        let callbacks = validate(
            &json!({"PreToolUse":[{"matcher":"Write","hookCallbackIds":["deny","ask","allow"]}]}),
        )
        .unwrap();
        let _guard = install(&session, callbacks, host.clone());
        let root = tempfile::tempdir().unwrap();
        let runner = super::super::tool_hooks::HookRunner::load(root.path(), root.path())
            .unwrap()
            .with_session_callbacks(&session);
        assert!(runner.has_hooks());
        let outcome = runner
            .run(
                HookEvent::PreToolUse,
                &session,
                json!({"tool_name":"Write","tool_input":{}}),
            )
            .await
            .unwrap();
        assert!(outcome.blocked);
        assert!(outcome.ask_permission);
        assert!(outcome.allow_permission);
    }
    #[tokio::test]
    async fn disable_all_hooks_suppresses_sdk_callbacks_too() {
        struct Counting(std::sync::atomic::AtomicUsize);
        impl Host for Counting {
            fn invoke<'a>(
                &'a self,
                _: &'a str,
                _: Value,
                _: Option<String>,
            ) -> BoxFuture<'a, Result<Value>> {
                Box::pin(async move {
                    self.0.fetch_add(1, Ordering::Relaxed);
                    Ok(json!({}))
                })
            }
        }
        let root = tempfile::tempdir().unwrap();
        crate::data_layout::write_config_file(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"),
            r#"{"disableAllHooks":true}"#,
        )
        .unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let host = Arc::new(Counting(std::sync::atomic::AtomicUsize::new(0)));
        let callbacks = validate(&json!({"PreToolUse":[{"hookCallbackIds":["pre"]}]})).unwrap();
        let _guard = install(&session, callbacks, host.clone());
        let runner = super::super::tool_hooks::HookRunner::load(root.path(), root.path())
            .unwrap()
            .with_session_callbacks(&session);
        assert!(!runner.has_hooks());
        runner
            .run(
                HookEvent::PreToolUse,
                &session,
                json!({"tool_name":"Write"}),
            )
            .await
            .unwrap();
        assert_eq!(host.0.load(Ordering::Relaxed), 0);
    }
}
