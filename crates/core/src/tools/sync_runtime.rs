use anyhow::{anyhow, Result};
use std::future::Future;

/// Keep the runtime alive until completion, and never drive it on an async worker.
pub(super) fn run<T: Send + 'static>(
    future: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    let work = super::path_scope::carry(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(future)
    });
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::Builder::new()
            .name("sync-tool-runtime".into())
            .spawn(work)?
            .join()
            .map_err(|_| anyhow!("Synchronous tool runtime thread panicked"))?
    } else {
        work()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timer() -> Result<u32> {
        run(async {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            Ok(42)
        })
    }

    #[test]
    fn runtime_lives_until_completion() {
        assert_eq!(timer().unwrap(), 42);
    }

    #[tokio::test]
    async fn sync_call_inside_current_thread_runtime() {
        assert_eq!(timer().unwrap(), 42);
        assert!(run(async { Err::<(), _>(anyhow!("tool failed")) })
            .unwrap_err()
            .to_string()
            .contains("tool failed"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_call_inside_worker_and_blocking_pool() {
        assert_eq!(tokio::spawn(async { timer() }).await.unwrap().unwrap(), 42);
        assert_eq!(
            tokio::task::spawn_blocking(timer).await.unwrap().unwrap(),
            42
        );
    }
}
