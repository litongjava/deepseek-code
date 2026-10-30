//! Explicit per-turn trusted directories; never shared across sessions or threads.
use std::{
    future::Future,
    path::{Path, PathBuf},
};
tokio::task_local! { static ALLOWED: Vec<PathBuf>; }

pub(crate) async fn scope<F: Future>(directories: Vec<PathBuf>, future: F) -> F::Output {
    let roots = directories
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect();
    ALLOWED.scope(roots, future).await
}

/// Runs blocking work on another thread with the calling task's roots.
pub(crate) fn carry<R>(work: impl FnOnce() -> R + Send + 'static) -> impl FnOnce() -> R + Send + 'static {
    let roots = ALLOWED.try_with(Clone::clone).unwrap_or_default();
    move || ALLOWED.sync_scope(roots, work)
}

pub(super) fn allows(canonical: &Path) -> bool {
    ALLOWED
        .try_with(|roots| roots.iter().any(|root| canonical.starts_with(root)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn roots_are_scoped_and_do_not_escape_to_other_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let accepted = root.clone();
        scope(vec![root.clone()], async move {
            assert!(allows(&accepted));
            assert!(!tokio::spawn(async move { allows(&accepted) })
                .await
                .unwrap());
        })
        .await;
        assert!(!allows(&root));
    }

    #[tokio::test]
    async fn carried_roots_reach_blocking_threads() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (inside, outside) = (root.clone(), root.clone());
        assert!(scope(vec![root], async move { tokio::task::spawn_blocking(carry(move || allows(&inside))).await.unwrap() }).await);
        assert!(!tokio::task::spawn_blocking(carry(move || allows(&outside))).await.unwrap());
    }
}
