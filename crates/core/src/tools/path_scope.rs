//! Explicit per-turn trusted directories; never shared across sessions or threads.
use std::{
    future::Future,
    path::{Path, PathBuf},
};
tokio::task_local! { static ALLOWED: Vec<PathBuf>; }
tokio::task_local! { static EXACT_WRITE: Option<PathBuf>; }
tokio::task_local! { static EXACT_COMMAND: Option<String>; }
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

/// Already approved built-ins receive one-call capabilities, never a parent
/// directory grant or an exception to operating-system sandbox restrictions.
pub(crate) async fn approved_auto_call<F: Future>(
    tool: &str,
    input: &serde_json::Value,
    cwd: &str,
    future: F,
) -> F::Output {
    let key = match tool {
        "Write" | "Edit" | "MultiEdit" | "ImageGenerate" => Some("file_path"),
        "NotebookEdit" => Some("notebook_path"),
        _ => None,
    };
    let target = key.and_then(|key| input[key].as_str()).and_then(|path| {
        let path = Path::new(path);
        canonical_target(&if path.is_absolute() {
            path.to_path_buf()
        } else {
            Path::new(cwd).join(path)
        })
    });
    let command = (tool == "Bash")
        .then(|| input["command"].as_str().map(str::to_owned))
        .flatten();
    EXACT_COMMAND
        .scope(command, EXACT_WRITE.scope(target, future))
        .await
}

fn canonical_target(path: &Path) -> Option<PathBuf> {
    if let Ok(path) = path.canonicalize() {
        return Some(path);
    }
    Some(canonical_target(path.parent()?)?.join(path.file_name()?))
}

pub(super) fn approved_auto_command(command: &str) -> bool {
    auto_mode()
        && EXACT_COMMAND
            .try_with(|approved| approved.as_deref() == Some(command))
            .unwrap_or(false)
}

pub(super) fn external_reads() -> bool {
    plan_mode() || auto_mode()
}

fn auto_mode() -> bool {
    PERMISSIONS
        .try_with(|manager| {
            manager.as_ref().is_some_and(|manager| {
                manager.get_mode() == crate::permissions::PermissionMode::Auto
            })
        })
        .unwrap_or(false)
}

/// Runs blocking work on another thread with the calling task's roots.
pub(crate) fn carry<R>(
    work: impl FnOnce() -> R + Send + 'static,
) -> impl FnOnce() -> R + Send + 'static {
    let roots = ALLOWED.try_with(Clone::clone).unwrap_or_default();
    let manager = PERMISSIONS.try_with(Clone::clone).unwrap_or_default();
    let exact = EXACT_WRITE.try_with(Clone::clone).unwrap_or_default();
    move || {
        PERMISSIONS.sync_scope(manager, || {
            ALLOWED.sync_scope(roots, || EXACT_WRITE.sync_scope(exact, work))
        })
    }
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

pub(super) fn plan_mode() -> bool {
    PERMISSIONS
        .try_with(|manager| {
            manager.as_ref().is_some_and(|manager| {
                manager.get_mode() == crate::permissions::PermissionMode::PlanMode
            })
        })
        .unwrap_or(false)
}

pub(super) fn allows(canonical: &Path) -> bool {
    // The permission gate still enforces explicit rules before tool execution.
    if bypass_permissions() {
        return true;
    }
    if auto_mode()
        && EXACT_WRITE
            .try_with(|target| target.as_deref() == Some(canonical))
            .unwrap_or(false)
    {
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
    async fn auto_external_paths_are_readable_but_write_grants_are_exact_and_temporary() {
        use crate::permissions::PermissionMode;
        use serde_json::json;
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("new/sub/file.txt");
        let other = outside.path().join("other.txt");
        let cwd = project.path().to_str().unwrap();
        let input = json!({"file_path":file,"content":"approved"});
        let manager = crate::runtime::default_permissions();
        manager.set_mode(PermissionMode::Auto);
        with_permissions(Some(manager.clone()), async {
            assert!(super::super::resolve_read_path(file.to_str().unwrap(), cwd).is_ok());
            assert!(super::super::resolve_path(file.to_str().unwrap(), cwd).is_err());
            approved_auto_call("Write", &input, cwd, async {
                assert!(super::super::resolve_path(file.to_str().unwrap(), cwd).is_ok());
                assert!(super::super::resolve_path(other.to_str().unwrap(), cwd).is_err());
                let copied = file.clone();
                assert!(!tokio::spawn(async move { allows(&copied) }).await.unwrap());
                manager.set_mode(PermissionMode::PlanMode);
                assert!(super::super::resolve_path(file.to_str().unwrap(), cwd).is_err());
                manager.set_mode(PermissionMode::Auto);
            })
            .await;
            assert!(super::super::resolve_path(file.to_str().unwrap(), cwd).is_err());
            manager.set_mode(PermissionMode::AskPermissions);
            assert!(super::super::resolve_read_path(file.to_str().unwrap(), cwd).is_err());
        })
        .await;
        assert!(super::super::resolve_read_path(file.to_str().unwrap(), cwd).is_err());
    }

    #[tokio::test]
    async fn plan_reads_external_files_without_granting_write_or_leaking_scope() {
        use crate::permissions::PermissionMode;
        use serde_json::json;
        crate::config::OrchestrationConfig::init_global().unwrap();
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("reference.txt");
        std::fs::write(&file, "outside UTF-8 reference: 中文").unwrap();
        let cwd = project.path().to_string_lossy().into_owned();
        let manager = crate::runtime::default_permissions();
        manager.set_mode(PermissionMode::PlanMode);
        with_permissions(Some(manager.clone()), async {
            for (tool, input) in [
                ("Read", json!({"file_path":file})),
                ("Glob", json!({"path":outside.path(),"pattern":"*.txt"})),
                (
                    "Grep",
                    json!({"path":outside.path(),"pattern":"中文","output_mode":"content"}),
                ),
            ] {
                let working = cwd.clone();
                let output = tokio::task::spawn_blocking(carry(move || {
                    crate::tools::execute_tool(tool, input, &working)
                }))
                .await
                .unwrap()
                .unwrap();
                assert!(
                    output.to_string().contains(if tool == "Glob" {
                        "reference.txt"
                    } else {
                        "中文"
                    }),
                    "{output}"
                );
            }
            assert!(crate::tools::execute_tool(
                "Write",
                json!({"file_path":file,"content":"changed"}),
                &cwd
            )
            .is_err());
            assert!(crate::tools::execute_tool(
                "Edit",
                json!({"file_path":file,"old_string":"outside","new_string":"changed"}),
                &cwd
            )
            .is_err());
            manager.set_mode(PermissionMode::AskPermissions);
            assert!(crate::tools::execute_tool("Read", json!({"file_path":file}), &cwd).is_err());
        })
        .await;
        assert!(crate::tools::execute_tool("Read", json!({"file_path":file}), &cwd).is_err());
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "outside UTF-8 reference: 中文"
        );
    }

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
