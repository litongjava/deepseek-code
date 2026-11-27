//! The lease remains the ownership authority. Its interactive holder publishes
//! a private IPC endpoint for commands and yield requests while the lease is held.
use super::*;
use deepseek_code_core::fs::OwnedFileLock;
use std::{
    fs,
    io::{ErrorKind, Write},
    time::Duration,
};

pub(in crate::session) struct FrontendEndpoint {
    address: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FrontendEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(in crate::session) struct OwnerLease {
    publication: Option<PathBuf>,
    generation: Arc<()>,
    _lock: OwnedFileLock,
}

impl Drop for OwnerLease {
    fn drop(&mut self) {
        if let Some(path) = &self.publication {
            let _ = fs::remove_file(path);
        }
    }
}

impl Session {
    fn lease_file(&self) -> Result<PathBuf> {
        let id = uuid::Uuid::parse_str(&self.id)?;
        Ok(self
            .paths
            .layout()
            .lock_path("mcp-owner")
            .join(format!("{id}.lock")))
    }

    fn try_mcp_lease(path: &Path) -> Result<Option<OwnedFileLock>> {
        fs::create_dir_all(path.parent().context("Missing MCP lease directory")?)?;
        let file = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(OwnedFileLock::try_acquire(file)?)
    }

    fn start_frontend_endpoint(&self, generation: std::sync::Weak<()>) -> Result<FrontendEndpoint> {
        let address = worker::frontend_endpoint(uuid::Uuid::new_v4())?;
        let mut listener = worker::Listener::bind_endpoint(&address)?;
        let mut target = self.worker_mcp();
        target.generation = Some(generation);
        let turns = self.mcp_local_turn.clone();
        let receipts = worker::McpReceipts::default();
        let task = tokio::spawn(async move {
            while let Ok(stream) = listener.accept().await {
                let target = target.clone();
                let turns = turns.clone();
                let receipts = receipts.clone();
                tokio::spawn(async move {
                    let Ok(Ok((stream, request))) =
                        tokio::time::timeout(Duration::from_secs(5), worker::read_request(stream))
                            .await
                    else {
                        return;
                    };
                    let turn = turns.try_lock_owned().ok();
                    WorkerMcp::dispatch(
                        Some(target),
                        request,
                        stream,
                        &receipts,
                        turn.is_none(),
                        turn,
                    );
                });
            }
        });
        Ok(FrontendEndpoint { address, task })
    }

    pub(super) async fn frontend_call(&self, op: &McpOp) -> Result<Option<McpResult>> {
        let lock = self.lease_file()?;
        let publication = lock.with_extension("frontend");
        let address = match fs::read_to_string(&publication) {
            Ok(address) => address,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if let Some(_unused) = Self::try_mcp_lease(&lock)? {
            // A crashed frontend left only its address behind.
            match fs::remove_file(publication) {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            return Ok(None);
        }
        let reply = self
            .mcp_client
            .lock()
            .await
            .call_endpoint(&address, op)
            .await?;
        Ok(Some(reply.mcp.context("Missing frontend MCP reply")?))
    }

    pub(super) async fn yield_frontend(&self) -> Result<()> {
        match self.frontend_call(&McpOp::StopAll).await? {
            Some(McpResult::Busy) => bail!(BUSY),
            Some(McpResult::Stopped | McpResult::Unavailable) | None => Ok(()),
            _ => bail!("Unexpected MCP yield reply"),
        }
    }

    pub(super) async fn claim_mcp_lease(&self) -> Result<OwnerLease> {
        let path = self.lease_file()?;
        let lock = match Self::try_mcp_lease(&path)? {
            Some(lock) => lock,
            None => {
                self.yield_frontend().await?;
                Self::try_mcp_lease(&path)?.context("Another process owns MCP for this session")?
            }
        };
        let publication = path.with_extension("frontend");
        let mut lease = OwnerLease {
            publication: Some(publication.clone()),
            generation: Arc::new(()),
            _lock: lock,
        };
        if self.observe_mcp_worker {
            // Recheck under the lease: a competing worker may have acquired it
            // and died between the initial discovery and our successful lock.
            // Only publish ownership once that worker's supervisor has drained.
            self.running_mcp_worker().await?;
            let endpoint = self.start_frontend_endpoint(Arc::downgrade(&lease.generation))?;
            let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
            file.write_all(endpoint.address.as_bytes())?;
            file.persist(publication)?;
            *self.mcp_endpoint.lock().unwrap() = Some(endpoint);
        } else {
            match fs::remove_file(publication) {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            lease.publication = None;
        }
        Ok(lease)
    }
}
