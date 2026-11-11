//! Explicit per-turn trusted directories; never shared across sessions or threads.
use std::{
    future::Future,
    path::{Path, PathBuf},
};
tokio::task_local! { static ALLOWED: Vec<PathBuf>; }
tokio::task_local! { static EXACT_WRITE: Option<PathBuf>; }
tokio::task_local! { static EXACT_COMMAND: Option<String>; }
tokio::task_local! { static EXACT_FETCH: Option<String>; }
tokio::task_local! { static EDIT_SESSION: Option<String>; }
tokio::task_local! { static PATCH_APPROVAL: bool; }
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

pub(crate) async fn with_edit_session<F: Future>(session: Option<String>, future: F) -> F::Output {
    EDIT_SESSION.scope(session, future).await
}

/// The session's additional trusted directories, for tools that resolve many
/// paths in one call. Empty outside an engine turn.
pub(crate) fn allowed_roots() -> Vec<PathBuf> {
    ALLOWED.try_with(Clone::clone).unwrap_or_default()
}

/// An explicit rule that covers this target as one of the shared file-edit
/// operations. Only deny and explicit ask rules are reported: mode grants,
/// saved approvals and allow rules belong to the tool-level decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExplicitPathRule {
    Denied(String),
    ConfirmationRequired(String),
}

/// Evaluate every canonical target of a multi-path edit against the session's
/// explicit file-operation rules. ApplyPatch performs the same create, modify,
/// remove and rename operations as `Edit`, `Write` and `MultiEdit`, so a rule
/// written for any of those tools covers its targets; a rule written for one
/// tool is never silently dropped under a universal `Edit` alias. Deny wins;
/// an explicit ask is reported as confirmation-required.
pub(crate) fn explicit_path_rule(raw_path: &str, workspace: &Path) -> Option<ExplicitPathRule> {
    let session = EDIT_SESSION.try_with(Clone::clone).ok().flatten()?;
    explicit_path_rule_for_session(raw_path, workspace, &session)
}

/// The same aggregation for a caller that already has the conversation id,
/// such as the engine's pre-dispatch permission check, where the edit-session
/// task local is not yet in scope.
pub(crate) fn explicit_path_rule_for_session(
    raw_path: &str,
    workspace: &Path,
    session: &str,
) -> Option<ExplicitPathRule> {
    let manager = PERMISSIONS.try_with(Clone::clone).ok().flatten()?;
    let mut confirmation = None;
    for tool_name in ["Edit", "Write", "MultiEdit"] {
        let decision = manager.evaluate_permission(&crate::permissions::PermissionContext {
            mcp_read_only: false,
            tool_name: tool_name.into(),
            tool_input: serde_json::json!({"file_path": raw_path}),
            conversation_id: session.to_owned(),
            user_id: None,
            workspace_path: Some(workspace.to_string_lossy().into()),
        });
        match decision.result {
            crate::permissions::PermissionResult::Denied(reason) => {
                return Some(ExplicitPathRule::Denied(format!("{tool_name}: {reason}")));
            }
            crate::permissions::PermissionResult::RequiresConfirmation(_)
                if decision.origin == crate::permissions::DecisionOrigin::ExplicitRule =>
            {
                confirmation.get_or_insert_with(|| {
                    format!("{tool_name} requires confirmation for this path")
                });
            }
            _ => {}
        }
    }
    confirmation.map(ExplicitPathRule::ConfirmationRequired)
}

/// Whether the engine asked the user and the user approved this exact
/// `ApplyPatch` call. A PreToolUse hook's automatic allow never sets it, so a
/// rewritten patch cannot inherit approval for unrelated paths.
pub(crate) fn patch_approval() -> bool {
    PATCH_APPROVAL
        .try_with(|approved| *approved)
        .unwrap_or(false)
}

fn session_edits() -> bool {
    EDIT_SESSION
        .try_with(|session| {
            PERMISSIONS
                .try_with(|manager| {
                    manager.as_ref().is_some_and(|manager| {
                        manager.get_mode() != crate::permissions::PermissionMode::PlanMode
                            && session
                                .as_deref()
                                .is_some_and(|session| manager.edit_session_allowed(session))
                    })
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Already approved built-ins receive one-call capabilities, never a parent
/// directory grant or an exception to operating-system sandbox restrictions.
pub(crate) async fn approved_auto_call<F: Future>(
    tool: &str,
    input: &serde_json::Value,
    cwd: &str,
    approved_command: bool,
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
    let command = (tool == "Bash" && approved_command)
        .then(|| input["command"].as_str().map(str::to_owned))
        .flatten();
    let fetch = (tool == "WebFetch" && approved_command)
        .then(|| input["url"].as_str().map(str::to_owned))
        .flatten();
    // ApplyPatch has no single path key. The approval binds the exact patch
    // input, and the tool re-derives every affected path from it.
    let patch = tool == "ApplyPatch" && approved_command;
    PATCH_APPROVAL
        .scope(
            patch,
            EXACT_COMMAND.scope(
                command,
                EXACT_FETCH.scope(fetch, EXACT_WRITE.scope(target, future)),
            ),
        )
        .await
}

pub(super) fn approved_local_fetch(url: &str) -> bool {
    EXACT_FETCH
        .try_with(|approved| approved.as_deref() == Some(url))
        .unwrap_or(false)
}

fn canonical_target(path: &Path) -> Option<PathBuf> {
    if let Ok(path) = path.canonicalize() {
        return Some(path);
    }
    Some(canonical_target(path.parent()?)?.join(path.file_name()?))
}

pub(super) fn approved_auto_command(command: &str) -> bool {
    EXACT_COMMAND
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
    let fetch = EXACT_FETCH.try_with(Clone::clone).unwrap_or_default();
    let session = EDIT_SESSION.try_with(Clone::clone).unwrap_or_default();
    let patch = PATCH_APPROVAL
        .try_with(|approved| *approved)
        .unwrap_or_default();
    move || {
        PERMISSIONS.sync_scope(manager, || {
            EDIT_SESSION.sync_scope(session, || {
                PATCH_APPROVAL.sync_scope(patch, || {
                    ALLOWED.sync_scope(roots, || {
                        EXACT_FETCH.sync_scope(fetch, || EXACT_WRITE.sync_scope(exact, work))
                    })
                })
            })
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
    if (auto_mode() || session_edits())
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
    async fn explicit_path_rules_cover_every_file_edit_tool_and_deny_wins() {
        use crate::permissions::rules::PermissionAction;
        use crate::permissions::PermissionMode;
        use std::collections::HashMap;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.txt");
        std::fs::write(&target, "x").unwrap();
        let canonical = canonical_target(&target).unwrap();
        let manager = crate::runtime::default_permissions();
        manager.set_mode(PermissionMode::AcceptEdits);
        manager.set_edit_session_allowed("session", true);
        for rule in [
            "Edit(target.txt)",
            "Write(target.txt)",
            "MultiEdit(target.txt)",
        ] {
            manager.set_session_rules(
                "session",
                HashMap::from([(rule.to_string(), PermissionAction::Confirm)]),
            );
            let path = canonical.clone();
            let root = root.path().to_path_buf();
            with_permissions(
                Some(manager.clone()),
                with_edit_session(Some("session".into()), async move {
                    assert!(
                        matches!(
                            explicit_path_rule(path.to_str().unwrap(), &root),
                            Some(ExplicitPathRule::ConfirmationRequired(_))
                        ),
                        "{rule} must not be lost under an Edit alias"
                    );
                }),
            )
            .await;
        }
        manager.set_session_rules(
            "session",
            HashMap::from([
                ("Edit(target.txt)".to_string(), PermissionAction::Confirm),
                ("Write(target.txt)".to_string(), PermissionAction::Deny),
            ]),
        );
        let path = canonical.clone();
        let root = root.path().to_path_buf();
        with_permissions(
            Some(manager),
            with_edit_session(Some("session".into()), async move {
                assert!(matches!(
                    explicit_path_rule(path.to_str().unwrap(), &root),
                    Some(ExplicitPathRule::Denied(_))
                ));
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn patch_approval_is_exact_temporary_and_carried_to_blocking_work() {
        let input = serde_json::json!({"patch": "*** Begin Patch\n*** End Patch"});
        assert!(!patch_approval());
        approved_auto_call("ApplyPatch", &input, ".", true, async {
            assert!(patch_approval());
            assert!(tokio::task::spawn_blocking(carry(patch_approval))
                .await
                .unwrap());
            assert!(!tokio::spawn(async { patch_approval() }).await.unwrap());
        })
        .await;
        assert!(!patch_approval());
        approved_auto_call("ApplyPatch", &input, ".", false, async {
            assert!(!patch_approval());
        })
        .await;
        approved_auto_call("Write", &input, ".", true, async {
            assert!(
                !patch_approval(),
                "another tool cannot grant patch approval"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn local_fetch_approval_is_exact_temporary_and_carried_to_blocking_work() {
        let input = serde_json::json!({"url":"http://localhost:8080/page"});
        approved_auto_call("WebFetch", &input, ".", true, async {
            assert!(approved_local_fetch("http://localhost:8080/page"));
            assert!(!approved_local_fetch("http://localhost:8081/page"));
            assert!(!approved_local_fetch("http://localhost:8080/other"));
            assert!(tokio::task::spawn_blocking(carry(|| approved_local_fetch(
                "http://localhost:8080/page"
            )))
            .await
            .unwrap());
            assert!(
                !tokio::spawn(async { approved_local_fetch("http://localhost:8080/page") })
                    .await
                    .unwrap()
            );
        })
        .await;
        assert!(!approved_local_fetch("http://localhost:8080/page"));
        approved_auto_call("WebFetch", &input, ".", false, async {
            assert!(!approved_local_fetch("http://localhost:8080/page"));
        })
        .await;
    }

    #[tokio::test]
    async fn dangerous_command_approval_is_exact_in_ask_and_does_not_leak() {
        let input = serde_json::json!({"command":"shutdown"});
        assert!(!approved_auto_command("shutdown"));
        approved_auto_call("Bash", &input, ".", true, async {
            assert!(approved_auto_command("shutdown"));
            assert!(!approved_auto_command("shutdown /s"));
            assert!(!tokio::spawn(async { approved_auto_command("shutdown") })
                .await
                .unwrap());
        })
        .await;
        approved_auto_call("Bash", &input, ".", false, async {
            assert!(!approved_auto_command("shutdown"));
        })
        .await;
        assert!(!approved_auto_command("shutdown"));
    }
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
            approved_auto_call("Write", &input, cwd, false, async {
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
    async fn session_edits_have_exact_revocable_paths_and_carry_no_other_session() {
        use crate::permissions::PermissionMode;
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = canonical_target(&outside.path().join("file.txt")).unwrap();
        let other = canonical_target(&outside.path().join("other.txt")).unwrap();
        let manager = crate::runtime::default_permissions();
        manager.set_mode(PermissionMode::AskPermissions);
        manager.set_edit_session_allowed("session", true);
        with_permissions(
            Some(manager.clone()),
            with_edit_session(Some("session".into()), async {
                approved_auto_call(
                    "Write",
                    &serde_json::json!({"file_path":target}),
                    project.path().to_str().unwrap(),
                    false,
                    async {
                        assert!(allows(&target));
                        assert!(!allows(&other));
                        let copy = target.clone();
                        assert!(tokio::task::spawn_blocking(carry(move || allows(&copy)))
                            .await
                            .unwrap());
                        with_edit_session(Some("other".into()), async {
                            assert!(!allows(&target));
                        })
                        .await;
                        manager.set_mode(PermissionMode::PlanMode);
                        assert!(!allows(&target));
                        manager.set_mode(PermissionMode::AskPermissions);
                        manager.set_edit_session_allowed("session", false);
                        assert!(!allows(&target));
                    },
                )
                .await;
            }),
        )
        .await;
        assert!(!allows(&target));
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
