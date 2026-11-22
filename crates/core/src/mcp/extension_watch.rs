//! Watch only extension inputs. Callbacks never touch a running server or turn;
//! discovery and reconciliation read the authoritative files at the boundary.
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Pending {
    last: Option<Instant>,
    skills: bool,
}

pub struct ExtensionWatch {
    pub workspace: PathBuf,
    data: PathBuf,
    pending: Arc<Mutex<Pending>>,
    watcher: Option<RecommendedWatcher>,
}

impl ExtensionWatch {
    pub fn new(data: &Path, workspace: &Path) -> Self {
        let mut this = Self {
            data: data.into(),
            workspace: workspace.into(),
            pending: Arc::default(),
            watcher: None,
        };
        this.register();
        this
    }

    fn register(&mut self) {
        let claude = self.workspace.join(".claude");
        let roots = [
            self.data.join("skills"),
            claude.join("skills"),
            claude.join("commands"),
        ];
        let skill_paths = [roots.to_vec(), vec![self.data.join("skill-states.json")]].concat();
        let mcp = [
            self.data.join("mcp-servers.json"),
            self.data.join("mcp-states.json"),
        ];
        let parent = claude.clone();
        let pending = self.pending.clone();
        let callback = move |event: notify::Result<Event>| {
            let Ok(event) = event else { return };
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            let skills = event
                .paths
                .iter()
                .any(|p| p == &parent || skill_paths.iter().any(|s| p.starts_with(s)));
            if skills || event.paths.iter().any(|p| mcp.contains(p)) {
                let mut state = pending.lock().unwrap();
                state.last = Some(Instant::now());
                state.skills |= skills;
            }
        };
        // A missing directory or an exhausted OS watcher budget must not prevent
        // a turn. Its authoritative boundary read is also the polling fallback.
        if let Ok(mut watcher) = notify::recommended_watcher(callback) {
            for path in [&self.data, &self.workspace, &claude] {
                let _ = watcher.watch(path, RecursiveMode::NonRecursive);
            }
            for path in roots {
                let _ = watcher.watch(&path, RecursiveMode::Recursive);
            }
            self.watcher = Some(watcher);
        }
    }

    /// Coalesce bursts (including editor rename/save) without starving turns.
    pub async fn settle(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_millis(400);
        loop {
            let last = self.pending.lock().unwrap().last;
            let Some(last) = last else { return false };
            let wait = (last + Duration::from_millis(80))
                .min(deadline)
                .saturating_duration_since(Instant::now());
            if wait.is_zero() {
                break;
            }
            tokio::time::sleep(wait).await;
        }
        let dirty = std::mem::take(&mut *self.pending.lock().unwrap());
        self.register(); // Include directories created or atomically replaced since registration.
        dirty.skills
    }
}
