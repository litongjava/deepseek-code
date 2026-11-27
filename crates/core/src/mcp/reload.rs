use super::McpServerManager;
use anyhow::Result;
use std::{collections::BTreeSet, io::ErrorKind};

impl McpServerManager {
    /// Apply the latest complete config at a turn boundary. The persistence CAS
    /// may already have advanced (e.g. when a tool installed a server locally).
    pub async fn reconcile_config(&self) -> Result<Vec<(String, String)>> {
        let _guard = self.lifecycle_lock.lock().await;
        let mut live = self.servers.write().await;
        let bytes = match tokio::fs::read(&self.config_path).await {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let prior = self.applied.lock().unwrap().clone();
        if bytes == prior {
            return Ok(Vec::new());
        }
        let previous = match prior.as_deref() {
            Some(bytes) => Self::parse_config(bytes)?,
            None => Default::default(),
        };
        // A malformed edit keeps the last good configuration on a hot reload;
        // only a first load treats a non-object `mcpServers` as empty.
        let mut desired = match bytes.as_deref() {
            Some(bytes) if prior.is_some() => Self::parse_reload_config(bytes)?,
            Some(bytes) => Self::parse_config(bytes)?,
            None => Default::default(),
        };
        let ids: BTreeSet<_> = previous
            .keys()
            .chain(desired.keys())
            .chain(live.keys())
            .cloned()
            .collect();
        let mut notices = Vec::new();
        let mut starts = Vec::new();
        for id in ids {
            let next = desired.remove(&id);
            let target = next.as_ref().map(|s| &s.config);
            let same_live = live.get(&id).map(|s| &s.config) == target;
            // Plugin synchronization may have just applied and attempted this
            // exact definition in this turn; starting it again here would repeat
            // its notice and keep retrying an unchanged broken server.
            if same_live && live.get(&id).is_some_and(|server| server.plugin_start_failed) {
                continue;
            }
            if same_live && previous.get(&id).map(|s| &s.config) == target {
                continue;
            }
            if same_live && target.is_some_and(|c| c.enabled) && live[&id].status.running {
                // Already applied by the local manager; only the snapshot is stale.
                notices.push((id, "updated".into()));
                continue;
            }
            let running = live.get(&id).is_some_and(|s| s.status.running);
            if let Some(connector) = live.get(&id).and_then(|s| s.connector.as_ref()) {
                // A call owns this mutex until it receives its response.
                connector.lock().await.stop().await?;
            }
            if let Some(state) = next {
                if state.config.enabled {
                    starts.push((id.clone(), if running { "restarted" } else { "started" }));
                } else {
                    notices.push((id.clone(), "disabled".into()));
                }
                live.insert(id, state);
            } else {
                live.remove(&id);
                notices.push((id, "removed".into()));
            }
        }
        *self.revision.lock().unwrap() = bytes.clone();
        drop(live);
        for (id, action) in starts {
            let status = match self.start_server_inner(&id).await {
                Ok(()) => action.to_owned(),
                Err(error) => format!("failed to start: {error:#}"),
            };
            notices.push((id, status));
        }
        *self.applied.lock().unwrap() = bytes;
        Ok(notices)
    }
}
