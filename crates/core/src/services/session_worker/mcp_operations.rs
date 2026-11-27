//! Mutation receipts are local to one worker generation. Finishing an admitted
//! action never depends on the original client keeping its stream open.
use super::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

type Outcome = std::result::Result<McpResult, String>;

fn response(outcome: Outcome) -> Reply {
    match outcome {
        Ok(result) => {
            let mut reply = Reply::ok(None);
            reply.mcp = Some(result);
            reply
        }
        Err(error) => Reply::error(error),
    }
}

#[derive(Default, Clone)]
pub struct McpReceipts(Arc<Mutex<VecDeque<(Uuid, Option<Outcome>)>>>);

impl McpReceipts {
    /// Reserve before spawning the task, so a status request can never pass it.
    pub fn admit(&self, id: Uuid) -> Option<Reply> {
        let mut entries = self.0.lock().unwrap();
        if let Some((_, entry)) = entries.iter().find(|(key, _)| *key == id) {
            return Some(response(entry.clone().unwrap_or(Ok(McpResult::Pending))));
        }
        if entries.len() >= 128 {
            let Some(index) = entries.iter().position(|(_, value)| value.is_some()) else {
                return Some(response(Ok(McpResult::Busy)));
            };
            entries.remove(index);
        }
        entries.push_back((id, None));
        None
    }

    pub fn finish(&self, id: Uuid, outcome: Result<McpResult>) -> Reply {
        let outcome = outcome.map_err(|err| format!("{err:#}"));
        if let Some((_, entry)) = self
            .0
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(key, _)| *key == id)
        {
            *entry = Some(outcome.clone());
        }
        response(outcome)
    }

    pub fn query(&self, id: Uuid) -> Reply {
        let entries = self.0.lock().unwrap();
        response(match entries.iter().find(|(key, _)| *key == id) {
            Some((_, Some(outcome))) => outcome.clone(),
            Some(_) => Ok(McpResult::Pending),
            None => Ok(McpResult::MissingReceipt),
        })
    }
}

impl McpOp {
    pub fn needs_receipt(&self) -> bool {
        matches!(
            self,
            Self::StopAll
                | Self::State { .. }
                | Self::ReloadPlugins
                | Self::SetPluginEnabled { .. }
        )
    }
}

#[derive(Default)]
pub struct McpClient {
    pending: Option<(String, Uuid, Vec<u8>)>,
}

impl McpClient {
    pub async fn call(&mut self, host: &Worker, op: &McpOp) -> Result<Reply> {
        let key = serde_json::to_vec(op)?;
        if let Some((generation, id, prior)) = &self.pending {
            if generation == &host.id {
                let reply = resolve(host, *id).await?;
                if matches!(reply.mcp, Some(McpResult::MissingReceipt)) {
                    // A lost frame or an expired receipt. Fence a retry on an
                    // idle manager before applying the desired state again.
                    let probe = exchange(
                        host,
                        &Request::Mcp(McpOp::ProbeOwner),
                        Duration::from_secs(5),
                    )
                    .await?
                    .checked()?;
                    ensure!(
                        !matches!(probe.mcp, Some(McpResult::Busy)),
                        "MCP operation outcome unknown; owner is busy"
                    );
                } else if *prior == key {
                    self.pending = None;
                    return reply.checked();
                }
            }
            self.pending = None;
        }
        if !op.needs_receipt() {
            return exchange(host, &Request::Mcp(op.clone()), Duration::from_secs(5))
                .await?
                .checked();
        }
        let id = Uuid::new_v4();
        self.pending = Some((host.id.clone(), id, key));
        let request = Request::McpTracked {
            id,
            operation: op.clone(),
        };
        let reply = match exchange(host, &request, Duration::from_secs(30)).await {
            Ok(reply) => reply,
            Err(_) => resolve(host, id).await?,
        };
        ensure!(
            !matches!(
                reply.mcp,
                Some(McpResult::MissingReceipt | McpResult::Pending)
            ),
            "MCP operation outcome unknown for worker {} ({id})",
            host.id
        );
        self.pending = None;
        reply.checked()
    }
}

async fn resolve(host: &Worker, id: Uuid) -> Result<Reply> {
    let poll = async {
        loop {
            let reply = exchange(host, &Request::McpOutcome { id }, Duration::from_secs(5)).await?;
            if !matches!(reply.mcp, Some(McpResult::Pending)) {
                return Ok::<_, anyhow::Error>(reply);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), poll)
        .await
        .map_err(anyhow::Error::from)
        .and_then(|reply| reply)
        .with_context(|| {
            format!(
                "MCP operation outcome unknown for worker {} ({id})",
                host.id
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mcp_retry_queries_the_original_generation_after_a_lost_reply() {
        let temp = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4().to_string();
        let host = Worker {
            endpoint: worker_endpoint(&id).unwrap(),
            id,
            session_id: Uuid::new_v4().to_string(),
            root: temp.path().into(),
            workspace: temp.path().into(),
            executable: "fixture".into(),
            status: "running".into(),
            supervisor_pid: None,
            child_pid: None,
            heartbeat_ms: 0,
            active_job: None,
            error: None,
        };
        let mut listener = Listener::bind(&host).unwrap();
        let server = tokio::spawn(async move {
            let (stream, request) = read_request(listener.accept().await.unwrap())
                .await
                .unwrap();
            let Request::McpTracked {
                id,
                operation: McpOp::StopAll,
            } = request
            else {
                panic!("expected one mutation");
            };
            // Both the first reply and its immediate status check are lost.
            drop(stream);
            for deliver in [false, true] {
                let (stream, query) = read_request(listener.accept().await.unwrap())
                    .await
                    .unwrap();
                assert!(matches!(query, Request::McpOutcome { id: prior } if prior == id));
                if deliver {
                    reply(stream, &response(Ok(McpResult::Stopped)))
                        .await
                        .unwrap();
                }
            }
        });
        let mut client = McpClient::default();
        assert!(client
            .call(&host, &McpOp::StopAll)
            .await
            .unwrap_err()
            .to_string()
            .contains("outcome unknown"));
        assert!(matches!(
            client.call(&host, &McpOp::StopAll).await.unwrap().mcp,
            Some(McpResult::Stopped)
        ));
        server.await.unwrap();
    }
}
