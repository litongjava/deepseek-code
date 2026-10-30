//! SDK MCP uses its own control mailbox; replies cannot authorize tools or hooks.
use anyhow::{ensure, Context, Result};
use deepseek_code_core::mcp::sdk::{Host, Runtime};
use serde_json::{json, Value};
use std::{future::Future, pin::Pin, sync::Arc};

#[derive(Default)]
pub struct Transport {
    pub broker: Arc<crate::sdk_control::Broker>,
}

impl Host for Transport {
    fn exchange<'a>(
        &'a self,
        server: &'a str,
        message: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let response = self
                .broker
                .request(json!({"subtype":"mcp_message","server_name":server,"message":message}))
                .await?;
            ensure!(
                response.is_object(),
                "SDK MCP control output must be an object"
            );
            // The official envelope does not repeat server_name. If supplied,
            // it must match this request rather than another server's stream.
            ensure!(
                response
                    .get("server_name")
                    .is_none_or(|name| name == server),
                "SDK MCP response targets another server"
            );
            response
                .get("mcp_response")
                .cloned()
                .context("SDK MCP control output requires mcp_response")
        })
    }

    fn close(&self) {
        self.broker.close();
    }
}

pub struct Cleanup {
    pub runtime: Arc<Runtime>,
    pub transport: Arc<Transport>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.runtime.clear();
        self.transport.broker.close();
    }
}
