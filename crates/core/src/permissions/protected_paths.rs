//! Upstream filesystem.ts: these require edit approval, not a blanket read ban.
use std::path::{Path, PathBuf};

fn sensitive(path: &Path) -> bool {
    let parts: Vec<_> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    for (i, part) in parts.iter().enumerate() {
        if [".git", ".vscode", ".idea", ".claude", ".deepseek-code"].contains(&part.as_str()) {
            if part == ".claude" && parts.get(i + 1).is_some_and(|p| p == "worktrees") {
                continue;
            }
            // The data directory holds hook settings and credentials, but its
            // `workspaces` are ordinary session working directories.
            if part == ".deepseek-code" && parts.get(i + 1).is_some_and(|p| p == "workspaces") {
                continue;
            }
            return true;
        }
    }
    parts.last().is_some_and(|p| {
        [
            ".gitconfig",
            ".gitmodules",
            ".bashrc",
            ".bash_profile",
            ".zshrc",
            ".zprofile",
            ".profile",
            ".ripgreprc",
            ".mcp.json",
            ".claude.json",
        ]
        .contains(&p.as_str())
    })
}

pub(super) fn requires_edit_approval(path: &str, cwd: &Path, home: Option<&Path>) -> bool {
    // UNC and protocol-relative paths may reach network resources. Verbatim
    // (`\\?\`) and device (`\\.\`) prefixes are local paths produced by
    // canonicalization, not network shares.
    let verbatim = path.starts_with(r"\\?\") || path.starts_with(r"\\.\");
    if (path.starts_with(r"\\") && !verbatim) || path.starts_with("//") {
        return true;
    }
    let path = if path == "~" {
        home.unwrap_or(cwd).to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.unwrap_or(cwd).join(rest)
    } else {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            cwd.join(p)
        }
    };
    if sensitive(&path) {
        return true;
    }
    // Resolve the existing ancestor too, including a symlink parent of a new file.
    let mut ancestor = path.as_path();
    let mut suffix = PathBuf::new();
    loop {
        if let Ok(resolved) = ancestor.canonicalize() {
            return sensitive(&resolved.join(suffix));
        }
        let Some(name) = ancestor.file_name() else {
            return false;
        };
        suffix = PathBuf::from(name).join(suffix);
        let Some(parent) = ancestor.parent() else {
            return false;
        };
        ancestor = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_edits_require_approval_but_reads_and_explicit_grants_work() {
        use crate::permissions::{
            AuditLogger, PermissionContext, PermissionManager, PermissionMode, PermissionResult,
        };
        let dir = tempfile::tempdir().unwrap();
        let manager = PermissionManager::new(std::sync::Arc::new(AuditLogger::new(16)));
        manager.set_mode(PermissionMode::AcceptEdits);
        let mut context = PermissionContext {
            tool_name: "Write".into(),
            tool_input: serde_json::json!({"file_path":dir.path().join(".bashrc")}),
            conversation_id: "home-test".into(),
            user_id: None,
            workspace_path: Some(dir.path().to_string_lossy().into()),
        };
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
        context.tool_name = "Read".into();
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        context.tool_name = "Write".into();
        manager.set_session_rules(
            "home-test",
            std::collections::HashMap::from([(
                "Write".into(),
                crate::permissions::rules::PermissionAction::Allow,
            )]),
        );
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
    }
    #[test]
    fn injected_root_home_allows_workspace_and_protects_config_edits() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("root");
        let workspace = home.join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let path = workspace.join("probe.txt");
        crate::fs::FileOperations::write_file(path.to_str().unwrap(), "before").unwrap();
        crate::fs::FileOperations::edit_file(path.to_str().unwrap(), "before", "after", false)
            .unwrap();
        assert_eq!(
            crate::fs::FileOperations::read_file(path.to_str().unwrap(), None, None).unwrap(),
            "after"
        );
        assert!(!requires_edit_approval(
            path.to_str().unwrap(),
            &workspace,
            Some(&home)
        ));
        for name in [
            ".gitconfig",
            ".gitmodules",
            ".bashrc",
            ".bash_profile",
            ".zshrc",
            ".zprofile",
            ".profile",
            ".ripgreprc",
            ".mcp.json",
            ".claude.json",
            ".git/config",
            ".vscode/settings.json",
            ".idea/a",
            ".claude/settings.json",
            ".deepseek-code/settings.json",
            ".deepseek-code/config.json",
        ] {
            assert!(
                requires_edit_approval(&format!("~/{name}"), &workspace, Some(&home)),
                "{name}"
            );
        }
        assert!(!requires_edit_approval(
            "~/.claude/worktrees/project/src/main.rs",
            &workspace,
            Some(&home)
        ));
        assert!(requires_edit_approval(
            "~/.claude/worktrees/project/.claude/settings.json",
            &workspace,
            Some(&home)
        ));
        assert!(!requires_edit_approval(
            "~/.deepseek-code/workspaces/session/notes.md",
            &workspace,
            Some(&home)
        ));
        assert!(!requires_edit_approval(
            "~/.ssh/id_rsa",
            &workspace,
            Some(&home)
        ));
        assert!(requires_edit_approval(
            r"\\server\share\probe.txt",
            &workspace,
            Some(&home)
        ));
        assert!(requires_edit_approval(
            "//server/share/probe.txt",
            &workspace,
            Some(&home)
        ));
        // Canonicalized local paths use a verbatim prefix on Windows.
        assert!(!requires_edit_approval(
            r"\\?\C:\project\probe.txt",
            &workspace,
            Some(&home)
        ));
    }
}
