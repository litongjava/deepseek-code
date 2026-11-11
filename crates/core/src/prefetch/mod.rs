use crate::cache::FileCache;
use anyhow::Result;
use regex::Regex;
use std::path::Path;
use std::sync::Arc;

pub struct PrefetchEngine {
    cache: Arc<FileCache>,
    workspace_cwd: String,
}

impl PrefetchEngine {
    pub fn new(cache: Arc<FileCache>, workspace_cwd: String) -> Self {
        Self {
            cache,
            workspace_cwd,
        }
    }

    pub async fn prefetch_for_message(&self, user_message: &str) {
        let intents = self.detect_intents(user_message);
        for intent in intents {
            let cache = self.cache.clone();
            let cwd = self.workspace_cwd.clone();
            tokio::spawn(async move {
                let _ = Self::execute_prefetch(intent, &cache, &cwd).await;
            });
        }
    }

    fn detect_intents(&self, message: &str) -> Vec<PrefetchIntent> {
        let mut intents = Vec::new();

        if let Ok(re) = Regex::new(r#"(?:["'`])([\w./\\]+\.\w+)(?:["'`])|([\w./\\]+\.\w{1,10})"#) {
            for cap in re.captures_iter(message) {
                let path = cap
                    .get(1)
                    .or_else(|| cap.get(2))
                    .map(|m| m.as_str().to_string());
                if let Some(path) = path {
                    if !path.starts_with("http") && path.len() > 3 {
                        intents.push(PrefetchIntent::ReadFile(path));
                    }
                }
            }
        }

        let project_signals = [
            "项目",
            "project",
            "代码库",
            "codebase",
            "仓库",
            "repo",
            "这个项目",
        ];
        let msg_lower = message.to_lowercase();
        for signal in &project_signals {
            if msg_lower.contains(signal) {
                intents.push(PrefetchIntent::ProjectScan);
                break;
            }
        }

        let modify_signals = [
            "修改", "编辑", "更改", "变更", "modify", "edit", "change", "update", "fix", "修复",
            "重构", "refactor",
        ];
        for signal in &modify_signals {
            if msg_lower.contains(signal) {
                intents.push(PrefetchIntent::GitStatus);
                break;
            }
        }

        intents
    }

    async fn execute_prefetch(
        intent: PrefetchIntent,
        cache: &Arc<FileCache>,
        cwd: &str,
    ) -> Result<()> {
        match intent {
            PrefetchIntent::ReadFile(path) => {
                let resolved = if Path::new(&path).is_absolute() {
                    path
                } else {
                    format!("{}/{}", cwd, path)
                };
                if Path::new(&resolved).exists() {
                    if let Ok(content) = std::fs::read_to_string(&resolved) {
                        if let Ok(metadata) = std::fs::metadata(&resolved) {
                            let mtime = metadata
                                .modified()
                                .map(|t| {
                                    t.duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                })
                                .unwrap_or(0);
                            cache.set_file(&resolved, content, mtime);
                        }
                    }
                }
                Ok(())
            }
            PrefetchIntent::ProjectScan => {
                let config_files = [
                    "package.json",
                    "Cargo.toml",
                    "pyproject.toml",
                    "go.mod",
                    "pom.xml",
                    "build.gradle",
                ];
                for config in &config_files {
                    let path = format!("{}/{}", cwd, config);
                    if Path::new(&path).exists() {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if let Ok(metadata) = std::fs::metadata(&path) {
                                let mtime = metadata
                                    .modified()
                                    .map(|t| {
                                        t.duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs()
                                    })
                                    .unwrap_or(0);
                                cache.set_file(&path, content, mtime);
                            }
                        }
                    }
                }
                Ok(())
            }
            PrefetchIntent::GitStatus => {
                // This observer runs in a spawned task and may overlap a tool
                // that is committing in the same worktree. Plain `git status`
                // refreshes and rewrites the index, briefly owning
                // `index.lock`, so a concurrent commit would fail with
                // "index.lock: File exists". Observation must never take an
                // optional lock; the host's real safety checks are unchanged.
                let output = std::process::Command::new("git")
                    .args(["status", "--porcelain"])
                    .current_dir(cwd)
                    .env("GIT_OPTIONAL_LOCKS", "0")
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .output();
                if let Ok(out) = output {
                    if out.status.success() {
                        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                        cache.set_command("git status --porcelain", cwd, stdout);
                    }
                }
                Ok(())
            }
        }
    }
}

enum PrefetchIntent {
    ReadFile(String),
    ProjectScan,
    GitStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn index_bytes(dir: &Path) -> Vec<u8> {
        std::fs::read(dir.join(".git/index")).unwrap()
    }

    /// A stat-only change (identical content, new mtime) makes Git's stat
    /// cache stale, so a plain `git status` rewrites the index. The prefetch
    /// observer must report the same status without touching the index, which
    /// is what keeps it from ever owning `index.lock` while a tool commits.
    #[tokio::test]
    async fn the_git_status_prefetch_never_rewrites_the_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.email", "test@example.invalid"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("tracked.txt"), "same\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "base"]);
        // Same bytes, new mtime: only the stat cache is stale.
        std::fs::write(root.join("tracked.txt"), "same\n").unwrap();
        let path = root.join("tracked.txt");
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2))
            .unwrap();
        drop(file);

        let before = index_bytes(root);
        let cache = Arc::new(FileCache::new());
        let cwd = root.to_string_lossy().into_owned();
        PrefetchEngine::execute_prefetch(PrefetchIntent::GitStatus, &cache, &cwd)
            .await
            .unwrap();
        let after = index_bytes(root);

        assert_eq!(
            before, after,
            "the observer rewrote the index and would have owned index.lock"
        );
        assert_eq!(
            cache.get_command("git status --porcelain", &cwd),
            Some(String::new()),
            "a clean worktree is cached as an empty status"
        );
    }

    /// Control: the same repository makes a plain status rewrite the index, so
    /// the test above is able to observe the difference rather than passing
    /// trivially.
    #[test]
    fn a_plain_git_status_would_rewrite_the_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.email", "test@example.invalid"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("tracked.txt"), "same\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "base"]);
        std::fs::write(root.join("tracked.txt"), "same\n").unwrap();
        let path = root.join("tracked.txt");
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2))
            .unwrap();
        drop(file);

        let before = index_bytes(root);
        let output = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_ne!(
            before,
            index_bytes(root),
            "the probe repository must be able to show an index refresh"
        );
    }
}
