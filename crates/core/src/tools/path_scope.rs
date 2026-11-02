//! Explicit per-turn trusted directories; never shared across sessions or threads.
use std::{
    future::Future,
    path::{Path, PathBuf},
};
tokio::task_local! { static ALLOWED: Vec<PathBuf>; }
tokio::task_local! { static PERMISSIONS: Option<std::sync::Arc<crate::permissions::PermissionManager>>; }

pub(crate) async fn with_permissions<F: Future>(
    manager: Option<std::sync::Arc<crate::permissions::PermissionManager>>,
    future: F,
) -> F::Output {
    PERMISSIONS.scope(manager, future).await
}

pub(crate) async fn scope<F: Future>(directories: Vec<PathBuf>, future: F) -> F::Output {
    let roots = directories
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect();
    ALLOWED.scope(roots, future).await
}

/// Runs blocking work on another thread with the calling task's roots.
pub(crate) fn carry<R>(
    work: impl FnOnce() -> R + Send + 'static,
) -> impl FnOnce() -> R + Send + 'static {
    let roots = ALLOWED.try_with(Clone::clone).unwrap_or_default();
    let manager = PERMISSIONS.try_with(Clone::clone).unwrap_or_default();
    move || PERMISSIONS.sync_scope(manager, || ALLOWED.sync_scope(roots, work))
}

/// Trusted execution context, never derived from model-supplied tool arguments.
/// Read the live mode so changing out of bypass takes effect immediately.
pub(super) fn bypass_permissions() -> bool {
    PERMISSIONS
        .try_with(|manager| {
            manager.as_ref().is_some_and(|manager| {
                manager.get_mode() == crate::permissions::PermissionMode::BypassPermissions
            })
        })
        .unwrap_or(false)
}

pub(super) fn allows(canonical: &Path) -> bool {
    // The permission gate still enforces explicit rules before tool execution.
    if bypass_permissions() {
        return true;
    }
    ALLOWED
        .try_with(|roots| roots.iter().any(|root| canonical.starts_with(root)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bypass_tracks_live_mode_and_carries_without_leaking() {
        use crate::permissions::{AuditLogger, PermissionManager, PermissionMode};
        let manager = std::sync::Arc::new(PermissionManager::new(std::sync::Arc::new(
            AuditLogger::new(16),
        )));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        with_permissions(Some(manager.clone()), async {
            manager.set_mode(PermissionMode::BypassPermissions);
            assert!(allows(&path));
            let carried = path.clone();
            assert!(tokio::task::spawn_blocking(carry(move || allows(&carried)))
                .await
                .unwrap());
            manager.set_mode(PermissionMode::AskPermissions);
            assert!(!allows(&path));
        })
        .await;
        assert!(!allows(&path));
    }
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
        assert!(
            scope(vec![root], async move {
                tokio::task::spawn_blocking(carry(move || allows(&inside)))
                    .await
                    .unwrap()
            })
            .await
        );
        assert!(
            !tokio::task::spawn_blocking(carry(move || allows(&outside)))
                .await
                .unwrap()
        );
    }
}
