//! Run-local SDK hooks; permissions and MCP use independent control brokers.
pub use crate::sdk_control::Broker;
use anyhow::Result;
use deepseek_code_core::services::hook_callbacks::Host;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
impl Host for Broker {
    fn invoke<'a>(
        &'a self,
        callback: &'a str,
        input: Value,
        tool_use_id: Option<String>,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let mut request =
                json!({"subtype":"hook_callback","callback_id":callback,"input":input});
            if let Some(tool_use_id) = tool_use_id {
                request["tool_use_id"] = json!(tool_use_id);
            }
            self.request(request).await
        })
    }
}
pub struct Runtime {
    callback_count: usize,
    pub broker: Arc<Broker>,
    registration: Arc<Mutex<Option<deepseek_code_core::services::hook_callbacks::Guard>>>,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            callback_count: 0,
            broker: Arc::default(),
            registration: Arc::default(),
        }
    }
}
impl Runtime {
    pub fn install(
        &mut self,
        session: &str,
        callbacks: Vec<deepseek_code_core::services::hook_callbacks::Callback>,
    ) {
        self.callback_count = callbacks.len();
        *self
            .registration
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(
            deepseek_code_core::services::hook_callbacks::install(session, callbacks, self.broker.clone()),
        );
    }
}
impl Runtime {
    pub fn has_callbacks(&self) -> bool {
        self.callback_count > 0
    }
    pub fn cleanup(&self) -> Cleanup {
        Cleanup {
            broker: self.broker.clone(),
            registration: self.registration.clone(),
        }
    }
}
pub struct Cleanup {
    broker: Arc<Broker>,
    registration: Arc<Mutex<Option<deepseek_code_core::services::hook_callbacks::Guard>>>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.broker.close();
        self.registration
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        drop(self.cleanup());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn run_guard_revokes_registration_even_while_session_retains_runtime() {
        let mut runtime = Runtime::default();
        let session = uuid::Uuid::new_v4().to_string();
        runtime.install(
            &session,
            deepseek_code_core::services::hook_callbacks::validate(
                &json!({"PreToolUse":[{"hookCallbackIds":["pre"]}]}),
            )
            .unwrap(),
        );
        assert!(deepseek_code_core::services::hook_callbacks::lookup(&session).is_some());
        let guard = runtime.cleanup();
        drop(guard);
        assert!(deepseek_code_core::services::hook_callbacks::lookup(&session).is_none());
        assert!(runtime.broker.is_closed());
    }
}
