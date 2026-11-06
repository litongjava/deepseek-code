use std::path::{Path, PathBuf};

/// Prefer the Git launcher over Windows' unrelated WSL bash aliases.
pub(super) fn git_shell(name: &str, paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    for path in paths {
        for root in path.ancestors().take(4) {
            if !root.join("git-bash.exe").is_file() {
                continue;
            }
            for relative in [format!("bin/{name}.exe"), format!("usr/bin/{name}.exe")] {
                let executable = root.join(relative);
                if executable.is_file() {
                    return Some(executable);
                }
            }
        }
    }
    None
}

pub(super) fn windows_git_shell(name: &str) -> Option<PathBuf> {
    let mut paths = Vec::new();
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(variable) {
            let base = PathBuf::from(base);
            paths.push(base.join("Git"));
            paths.push(base.join("Programs/Git"));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    paths.extend([
        Path::new(r"C:\Program Files\Git").into(),
        Path::new(r"C:\Git").into(),
    ]);
    git_shell(name, paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_nonstandard_git_install_and_skips_wsl_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("other drive/Program Files/Git");
        let system = temp.path().join("Windows/System32");
        let apps = temp.path().join("WindowsApps");
        for dir in [
            &system,
            &apps,
            &root.join("cmd"),
            &root.join("bin"),
            &root.join("usr/bin"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("bash.exe"), "").unwrap();
        }
        std::fs::write(root.join("git-bash.exe"), "").unwrap();
        assert_eq!(
            git_shell("bash", [system.clone(), apps.clone(), root.join("cmd")]),
            Some(root.join("bin/bash.exe"))
        );
        assert_eq!(git_shell("bash", [system, apps]), None);
        assert_eq!(
            git_shell("bash", [root.join("usr/bin")]),
            Some(root.join("bin/bash.exe"))
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_shells_preserve_failures_and_directory_state() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().to_str().unwrap();
        std::fs::create_dir(temp.path().join("sub space")).unwrap();
        for (shell, change, inspect, fail) in [
            ("cmd", "cd /d \"sub space\"", "cd", "cmd /d /c exit 7"),
            (
                "bash",
                "cd 'sub space'",
                "pwd -W",
                "printf '%s\\n' 'warning' >&2; (exit 7)",
            ),
        ] {
            let state = temp.path().join(format!("{shell}-cwd"));
            let run = |command: &str| serde_json::json!({"shell":shell,"command":command,"__cwd_state":state});
            let changed = super::super::tool_bash_async(run(change), cwd)
                .await
                .unwrap();
            assert_eq!(changed["exit_code"], 0, "{changed}");
            let located = super::super::tool_bash_async(run(inspect), cwd)
                .await
                .unwrap();
            assert!(
                located["stdout"].as_str().unwrap().contains("sub space"),
                "{located}"
            );
            let failed = super::super::tool_bash_async(run(fail), cwd).await.unwrap();
            assert_eq!(failed["exit_code"], 7, "{failed}");
            assert_eq!(failed["success"], false, "{failed}");
            assert_eq!(failed["shell"], shell);
            assert!(failed["shell_path"].as_str().is_some());
        }
        let syntax = super::super::tool_bash_async(serde_json::json!({"shell":"bash","command":"if then","__cwd_state":temp.path().join("syntax-cwd")}), cwd).await.unwrap();
        assert_eq!(syntax["success"], false, "{syntax}");
        assert_ne!(syntax["exit_code"], 0, "{syntax}");
        let unknown = super::super::tool_bash_async(
            serde_json::json!({"shell":"not-a-shell","command":"echo sentinel"}),
            cwd,
        )
        .await;
        assert!(unknown
            .unwrap_err()
            .to_string()
            .contains("Unsupported shell"));
    }
}
