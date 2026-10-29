//! SDK permission patches for the session and persisted settings sources.
//! Validate a whole batch before publishing; settings writes are atomic.
use crate::permissions::{rules::PermissionAction, PermissionManager, PermissionMode};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Destination {
    Session,
    UserSettings,
    ProjectSettings,
    LocalSettings,
    CliArg,
}

impl Destination {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "session" => Self::Session,
            "userSettings" => Self::UserSettings,
            "projectSettings" => Self::ProjectSettings,
            "localSettings" => Self::LocalSettings,
            "cliArg" => Self::CliArg,
            other => bail!("Unsupported permission update destination: {other}"),
        })
    }
}

/// Settings files an SDK host has already trusted for this session.
#[derive(Clone)]
pub struct SettingsScope {
    pub data_dir: PathBuf,
    pub workspace: PathBuf,
    pub project_trusted: bool,
}

impl SettingsScope {
    fn file(&self, destination: Destination) -> Result<Option<PathBuf>> {
        Ok(match destination {
            Destination::Session => None,
            Destination::UserSettings => Some(self.data_dir.join("settings.json")),
            Destination::ProjectSettings => {
                ensure!(
                    self.project_trusted,
                    "Project settings cannot be updated for an untrusted project"
                );
                Some(self.workspace.join(".claude").join("settings.json"))
            }
            Destination::LocalSettings => {
                ensure!(
                    self.project_trusted,
                    "Local settings cannot be updated for an untrusted project"
                );
                Some(self.workspace.join(".claude").join("settings.local.json"))
            }
            Destination::CliArg => bail!("CLI argument rules cannot be updated at runtime"),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rule {
    tool_name: String,
    rule_content: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawUpdate {
    #[serde(rename = "type")]
    operation: String,
    destination: String,
    behavior: Option<String>,
    #[serde(default)]
    rules: Vec<Rule>,
    mode: Option<String>,
    #[serde(default)]
    directories: Vec<String>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

enum Prepared {
    Rules {
        destination: Destination,
        index: usize,
        operation: String,
        patterns: Vec<String>,
    },
    Mode {
        destination: Destination,
        mode: PermissionMode,
    },
    Directories {
        destination: Destination,
        add: bool,
        directories: Vec<PathBuf>,
    },
}

impl Prepared {
    fn destination(&self) -> Destination {
        match self {
            Self::Rules { destination, .. }
            | Self::Mode { destination, .. }
            | Self::Directories { destination, .. } => *destination,
        }
    }
}

fn pattern(rule: &Rule) -> Result<String> {
    ensure!(
        !rule.tool_name.contains(['(', ')']),
        "SDK toolName cannot contain a rule pattern"
    );
    let pattern = match &rule.rule_content {
        Some(content) => format!(
            "{}({})",
            rule.tool_name,
            content
                .replace('\\', "\\\\")
                .replace('(', "\\(")
                .replace(')', "\\)")
        ),
        None => rule.tool_name.clone(),
    };
    crate::permissions::patterns::validate(&pattern)?;
    Ok(pattern)
}

fn sdk_mode_name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::AskPermissions => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::PlanMode => "plan",
        PermissionMode::BypassPermissions => "bypassPermissions",
        PermissionMode::DontAsk => "dontAsk",
        PermissionMode::Auto => "auto",
    }
}

fn behavior(value: Option<&str>) -> Result<(usize, PermissionAction)> {
    Ok(match value {
        Some("allow") => (0, PermissionAction::Allow),
        Some("ask") => (1, PermissionAction::Confirm),
        Some("deny") => (2, PermissionAction::Deny),
        _ => bail!("Unsupported SDK permission behavior"),
    })
}

fn prepare(raw: RawUpdate) -> Result<Prepared> {
    ensure!(
        raw.extra.is_empty(),
        "Unsupported SDK permission update field"
    );
    let destination = Destination::parse(&raw.destination)?;
    match raw.operation.as_str() {
        "addRules" | "replaceRules" | "removeRules" => {
            ensure!(raw.rules.len() <= 1024, "Too many SDK permission rules");
            ensure!(raw.mode.is_none(), "Rules updates cannot set a mode");
            ensure!(
                raw.directories.is_empty(),
                "Rules updates cannot change directories"
            );
            let (index, _) = behavior(raw.behavior.as_deref())?;
            let patterns = raw.rules.iter().map(pattern).collect::<Result<Vec<_>>>()?;
            Ok(Prepared::Rules {
                destination,
                index,
                operation: raw.operation,
                patterns,
            })
        }
        "setMode" => {
            ensure!(raw.rules.is_empty(), "Mode updates cannot add rules");
            ensure!(raw.behavior.is_none(), "Mode updates cannot set behavior");
            ensure!(
                raw.directories.is_empty(),
                "Mode updates cannot change directories"
            );
            let mode = PermissionMode::parse(raw.mode.as_deref().context("mode is required")?)?;
            ensure!(
                mode != PermissionMode::BypassPermissions,
                "SDK permission updates cannot enable bypassPermissions"
            );
            ensure!(
                mode != PermissionMode::Auto,
                "SDK permission updates cannot enable auto mode"
            );
            Ok(Prepared::Mode { destination, mode })
        }
        "addDirectories" | "removeDirectories" => {
            ensure!(
                raw.rules.is_empty() && raw.mode.is_none() && raw.behavior.is_none(),
                "Directory updates cannot change rules, behavior or modes"
            );
            ensure!(
                raw.directories.len() <= 256,
                "Too many SDK permission directories"
            );
            for directory in &raw.directories {
                ensure!(
                    !directory.trim().is_empty() && directory.len() <= 4096,
                    "Invalid SDK permission directory"
                );
            }
            Ok(Prepared::Directories {
                destination,
                add: raw.operation == "addDirectories",
                directories: raw.directories.into_iter().map(PathBuf::from).collect(),
            })
        }
        other => bail!("Unsupported SDK permission update type: {other}"),
    }
}

fn lock_settings(path: &Path) -> Result<std::fs::File> {
    let directory = path.parent().context("Settings parent missing")?;
    std::fs::create_dir_all(directory)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings.json");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!(".{name}.lock")))?;
    file.lock()?;
    Ok(file)
}

struct FileEdit {
    path: PathBuf,
    original: Vec<u8>,
    bom: bool,
    value: Value,
    changed: bool,
}

fn read_settings(path: &Path) -> Result<FileEdit> {
    let original = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let bom = original.starts_with(&[0xEF, 0xBB, 0xBF]);
    let body = if bom { &original[3..] } else { &original[..] };
    let value = if body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        serde_json::from_slice(body).with_context(|| format!("Invalid {}", path.display()))?
    };
    ensure!(
        value.is_object(),
        "{} must contain a JSON object",
        path.display()
    );
    Ok(FileEdit {
        path: path.into(),
        original,
        bom,
        value,
        changed: false,
    })
}

fn string_rules(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn same_rule(a: &str, b: &str) -> bool {
    crate::permissions::patterns::canonical_rule(a)
        .eq_ignore_ascii_case(&crate::permissions::patterns::canonical_rule(b))
}

fn apply_rules_edit(edit: &mut FileEdit, index: usize, operation: &str, patterns: &[String]) {
    let key = ["allow", "ask", "deny"][index];
    let permissions = edit
        .value
        .as_object_mut()
        .expect("object")
        .entry("permissions")
        .or_insert_with(|| json!({}));
    let permissions = permissions.as_object_mut().expect("permissions object");
    let mut existing = string_rules(
        permissions
            .get(key)
            .unwrap_or(&Value::Null),
    );
    let before = existing.clone();
    match operation {
        "replaceRules" => {
            existing.clear();
            for pattern in patterns {
                if !existing.iter().any(|rule| same_rule(rule, pattern)) {
                    existing.push(pattern.clone());
                }
            }
        }
        "addRules" => {
            for pattern in patterns {
                if !existing.iter().any(|rule| same_rule(rule, pattern)) {
                    existing.push(pattern.clone());
                }
            }
        }
        "removeRules" => {
            existing.retain(|rule| !patterns.iter().any(|pattern| same_rule(rule, pattern)));
        }
        _ => unreachable!(),
    }
    if existing != before {
        permissions.insert(key.into(), json!(existing));
        edit.changed = true;
    }
}

fn apply_directories_edit(edit: &mut FileEdit, add: bool, directories: &[PathBuf]) {
    let permissions = edit
        .value
        .as_object_mut()
        .expect("object")
        .entry("permissions")
        .or_insert_with(|| json!({}));
    let permissions = permissions.as_object_mut().expect("permissions object");
    let mut existing = string_rules(permissions.get("additionalDirectories").unwrap_or(&Value::Null));
    let before = existing.clone();
    if add {
        for directory in directories {
            let path = directory.to_string_lossy().into_owned();
            if !existing.iter().any(|entry| Path::new(entry) == *directory) {
                existing.push(path);
            }
        }
    } else {
        existing.retain(|entry| !directories.iter().any(|directory| Path::new(entry) == *directory));
    }
    if existing != before || (!add && existing.is_empty()) {
        if existing.is_empty() {
            permissions.remove("additionalDirectories");
        } else {
            permissions.insert("additionalDirectories".into(), json!(existing));
        }
        edit.changed = true;
    }
}

fn apply_mode_edit(edit: &mut FileEdit, mode: PermissionMode) {
    let permissions = edit
        .value
        .as_object_mut()
        .expect("object")
        .entry("permissions")
        .or_insert_with(|| json!({}));
    let permissions = permissions.as_object_mut().expect("permissions object");
    if permissions.get("defaultMode").and_then(Value::as_str) != Some(sdk_mode_name(mode)) {
        permissions.insert("defaultMode".into(), json!(sdk_mode_name(mode)));
        edit.changed = true;
    }
}

fn write_settings(edit: &FileEdit) -> Result<()> {
    let directory = edit.path.parent().context("Settings parent missing")?;
    std::fs::create_dir_all(directory)?;
    let mut bytes = Vec::new();
    if edit.bom {
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    bytes.extend_from_slice(&serde_json::to_vec_pretty(&edit.value)?);
    bytes.push(b'\n');
    let temp = directory.join(format!(
        ".{}.{}.tmp",
        edit.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("settings"),
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&temp, &bytes)?;
    std::fs::rename(&temp, &edit.path)?;
    Ok(())
}

fn restore_settings(path: &Path, original: &[u8]) -> Result<()> {
    if original.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    } else {
        let directory = path.parent().context("Settings parent missing")?;
        let temp = directory.join(format!(".restore.{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temp, original)?;
        std::fs::rename(&temp, path)?;
        Ok(())
    }
}

fn apply_runtime_rules(
    manager: &PermissionManager,
    session: &str,
    rules: &[&Prepared],
    publish: bool,
) -> Result<()> {
    manager.update_runtime_rules(session, publish, |next| {
        for prepared in rules {
            let Prepared::Rules {
                index,
                operation,
                patterns,
                ..
            } = prepared
            else {
                continue;
            };
            let action = match index {
                0 => PermissionAction::Allow,
                1 => PermissionAction::Confirm,
                _ => PermissionAction::Deny,
            };
            if operation == "replaceRules" {
                next[*index].clear();
            }
            for pattern in patterns {
                match operation.as_str() {
                    "addRules" | "replaceRules" => {
                        next[*index].insert(pattern.clone(), action.clone());
                    }
                    "removeRules" => {
                        next[*index].remove(pattern);
                    }
                    _ => unreachable!(),
                }
            }
        }
        Ok(())
    })
}

fn apply_session_directories(
    manager: &PermissionManager,
    session: &str,
    directories: &[&Prepared],
) {
    let mut current = manager.session_directories(session);
    for prepared in directories {
        let Prepared::Directories {
            add, directories, ..
        } = prepared
        else {
            continue;
        };
        if *add {
            for directory in directories {
                if !current.contains(directory) {
                    current.push(directory.clone());
                }
            }
        } else {
            current.retain(|entry| !directories.contains(entry));
        }
    }
    manager.set_session_directories(session, current);
}

pub struct Updates {
    manager: Arc<PermissionManager>,
    session: String,
    settings: Option<SettingsScope>,
}

impl Updates {
    pub fn new(manager: Arc<PermissionManager>, session: String) -> Self {
        Self {
            manager,
            session,
            settings: None,
        }
    }

    pub fn with_settings(mut self, settings: SettingsScope) -> Self {
        self.settings = Some(settings);
        self
    }

    pub fn apply(&self, value: &Value) -> Result<()> {
        self.apply_inner(value, true)
    }

    pub fn prepare(
        self: &Arc<Self>,
        value: &Value,
    ) -> Result<crate::native_engine::permission_response::Commit> {
        self.apply_inner(value, false)?;
        let updates = self.clone();
        let value = value.clone();
        Ok(Box::new(move || updates.apply(&value)))
    }

    fn apply_inner(&self, value: &Value, publish: bool) -> Result<()> {
        let updates: Vec<RawUpdate> =
            serde_json::from_value(value.clone()).context("Invalid SDK updatedPermissions")?;
        ensure!(updates.len() <= 256, "Too many SDK permission updates");
        let mut prepared = Vec::with_capacity(updates.len());
        for update in updates {
            prepared.push(prepare(update)?);
        }
        // Validate destinations before touching any file or runtime state.
        for update in &prepared {
            if update.destination() != Destination::Session
                && self.settings.is_none()
            {
                bail!("Persisted permission updates require a settings scope");
            }
            if let Some(scope) = &self.settings {
                scope.file(update.destination())?;
            }
        }
        // Serialize whole-file read-modify-write across processes.
        let mut lock_paths: Vec<PathBuf> = Vec::new();
        if publish {
            if let Some(scope) = &self.settings {
                for update in &prepared {
                    if let Some(path) = scope.file(update.destination())? {
                        if !lock_paths.contains(&path) {
                            lock_paths.push(path);
                        }
                    }
                }
            }
        }
        lock_paths.sort();
        let _locks = lock_paths
            .iter()
            .map(|path| lock_settings(path))
            .collect::<Result<Vec<_>>>()?;
        let mut edits: Vec<FileEdit> = Vec::new();
        for update in &prepared {
            let Some(scope) = &self.settings else { continue };
            let Some(path) = scope.file(update.destination())? else {
                continue;
            };
            if !edits.iter().any(|edit| edit.path == path) {
                edits.push(read_settings(&path)?);
            }
            let edit = edits
                .iter_mut()
                .find(|edit| edit.path == path)
                .expect("edit");
            match update {
                Prepared::Rules {
                    index,
                    operation,
                    patterns,
                    ..
                } => apply_rules_edit(edit, *index, operation, patterns),
                Prepared::Mode { mode, .. } => apply_mode_edit(edit, *mode),
                Prepared::Directories {
                    add, directories, ..
                } => apply_directories_edit(edit, *add, directories),
            }
        }
        let rules: Vec<&Prepared> = prepared.iter().collect();
        apply_runtime_rules(&self.manager, &self.session, &rules, false)?;
        if !publish {
            return Ok(());
        }
        let mut written: Vec<(PathBuf, Vec<u8>)> = Vec::new();
        for edit in edits.iter().filter(|edit| edit.changed) {
            if let Err(error) = write_settings(edit) {
                for (path, original) in written.iter().rev() {
                    let _ = restore_settings(path, original);
                }
                return Err(error);
            }
            written.push((edit.path.clone(), edit.original.clone()));
        }
        let result = (|| {
            apply_runtime_rules(&self.manager, &self.session, &rules, true)?;
            for update in &prepared {
                if let Prepared::Mode { mode, .. } = update {
                    self.manager.set_mode(*mode);
                }
            }
            let directories: Vec<&Prepared> = prepared
                .iter()
                .filter(|update| matches!(update, Prepared::Directories { .. }))
                .collect();
            if !directories.is_empty() {
                apply_session_directories(&self.manager, &self.session, &directories);
            }
            Ok(())
        })();
        if let Err(error) = result {
            for (path, original) in written.iter().rev() {
                let _ = restore_settings(path, original);
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn clear(&self) {
        self.manager.clear_runtime_rules(&self.session);
    }
}
pub struct Cleanup(pub Arc<Updates>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.0.clear();
    }
}

/// Saves an allow rule to `.claude/settings.local.json`, where the reference
/// CLI keeps "Yes, and don't ask again" answers. Returns the file written.
pub fn add_local_allow_rule(workspace: &Path, rule: &str) -> Result<PathBuf> {
    crate::permissions::patterns::validate(rule)?;
    let path = workspace.join(".claude").join("settings.local.json");
    let _lock = lock_settings(&path)?;
    let mut edit = read_settings(&path)?;
    apply_rules_edit(&mut edit, 0, "addRules", &[rule.to_owned()]);
    if edit.changed {
        write_settings(&edit)?;
    }
    Ok(path)
}

#[derive(Default)]
pub struct LoadedPermissions {
    pub rules: HashMap<String, PermissionAction>,
    pub directories: Vec<PathBuf>,
    pub default_mode: Option<String>,
}

/// Reads persisted permission rules with local > project > user precedence.
/// A deny in any source cannot be turned into an allow by a later source.
pub fn load_settings(
    data_dir: &Path,
    workspace: &Path,
    project_trusted: bool,
) -> Result<LoadedPermissions> {
    let mut loaded = LoadedPermissions::default();
    let sources = [
        (data_dir.join("settings.json"), true),
        (workspace.join(".claude").join("settings.json"), project_trusted),
        (
            workspace.join(".claude").join("settings.local.json"),
            project_trusted,
        ),
    ];
    for (path, allowed) in sources {
        if !allowed {
            continue;
        }
        let edit = match read_settings(&path) {
            Ok(edit) => edit,
            Err(error) => {
                tracing::warn!("Ignoring invalid permission settings {}: {error:#}", path.display());
                continue;
            }
        };
        let Some(permissions) = edit.value.get("permissions") else {
            continue;
        };
        for (key, action) in [
            ("allow", PermissionAction::Allow),
            ("ask", PermissionAction::Confirm),
            ("deny", PermissionAction::Deny),
        ] {
            for rule in string_rules(permissions.get(key).unwrap_or(&Value::Null)) {
                let rule = crate::permissions::patterns::canonical_rule(&rule);
                if crate::permissions::patterns::validate(&rule).is_err() {
                    tracing::warn!("Ignoring invalid permission rule {rule} in {}", path.display());
                    continue;
                }
                match loaded.rules.get(&rule) {
                    // A deny in any source cannot be relaxed by a later source.
                    Some(PermissionAction::Deny) => {}
                    _ => {
                        loaded.rules.insert(rule, action.clone());
                    }
                }
            }
        }
        for directory in string_rules(permissions.get("additionalDirectories").unwrap_or(&Value::Null)) {
            let path = PathBuf::from(directory);
            if !loaded.directories.contains(&path) {
                loaded.directories.push(path);
            }
        }
        if let Some(mode) = permissions.get("defaultMode").and_then(Value::as_str) {
            loaded.default_mode = Some(mode.into());
        }
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::permissions::{AuditLogger, PermissionContext, PermissionMode, PermissionResult};
    use serde_json::json;
    fn patch(operation: &str, behavior: &str) -> Value {
        json!({"type":operation,"destination":"session","behavior":behavior,"rules":[{"toolName":"Write"}]})
    }
    #[test]
    fn independently_prepared_patches_preserve_later_rules() {
        let manager = Arc::new(PermissionManager::new(Arc::new(AuditLogger::new(16))));
        let updates = Arc::new(Updates::new(manager.clone(), "one".into()));
        let allow = updates
            .prepare(&json!([patch("addRules", "allow")]))
            .unwrap();
        let deny = updates
            .prepare(&json!([patch("addRules", "deny")]))
            .unwrap();
        allow().unwrap();
        deny().unwrap();
        let context = PermissionContext {
            tool_name: "Write".into(),
            tool_input: json!({}),
            conversation_id: "one".into(),
            user_id: None,
            workspace_path: None,
        };
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        updates
            .apply(&json!([patch("removeRules", "deny")]))
            .unwrap();
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
    }
    #[test]
    fn child_with_no_inherited_runtime_rules_can_prepare_a_patch() {
        let parent = PermissionManager::new(Arc::new(AuditLogger::new(16)));
        let child = Arc::new(parent.fork_session("parent", "child"));
        let updates = Arc::new(Updates::new(child.clone(), "child".into()));
        updates
            .prepare(&json!([patch("addRules", "allow")]))
            .unwrap()()
        .unwrap();
        assert_eq!(
            child.check_permission(&PermissionContext {
                tool_name: "Write".into(),
                tool_input: json!({}),
                conversation_id: "child".into(),
                user_id: None,
                workspace_path: None
            }),
            PermissionResult::Granted
        );
    }
    #[test]
    fn updates_are_atomic_scoped_and_behavior_specific() {
        let manager = Arc::new(PermissionManager::new(Arc::new(AuditLogger::new(16))));
        manager.set_mode(PermissionMode::AskPermissions);
        let updates = Arc::new(Updates::new(manager.clone(), "one".into()));
        let context = PermissionContext {
            tool_name: "Write".into(),
            tool_input: json!({"path":"test"}),
            conversation_id: "one".into(),
            user_id: None,
            workspace_path: None,
        };
        updates.apply(&json!([patch("addRules", "allow")])).unwrap();
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        for invalid in [
            json!([{"type":"setMode","destination":"session","mode":"bypassPermissions"}]),
            json!([{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write","ruleContent":"../outside"}]}]),
            json!([{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"LSP","ruleContent":"*"}]}]),
            json!([{"type":"addRules","destination":"session","behavior":"allow","rules":[],"unexpected":true}]),
        ] {
            assert!(updates.apply(&invalid).is_err());
            assert_eq!(
                manager.check_permission(&context),
                PermissionResult::Granted
            );
        }
        let other = PermissionContext {
            conversation_id: "two".into(),
            ..context.clone()
        };
        assert!(matches!(
            manager.check_permission(&other),
            PermissionResult::RequiresConfirmation(_)
        ));
        let mut invalid = patch("addRules", "deny");
        invalid["destination"] = json!("projectSettings");
        assert!(updates
            .apply(&json!([patch("removeRules", "allow"), invalid]))
            .is_err());
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        updates
            .apply(&json!([
                patch("addRules", "deny"),
                patch("addRules", "ask")
            ]))
            .unwrap();
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        updates
            .apply(&json!([patch("removeRules", "deny")]))
            .unwrap();
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
        updates.apply(&json!([{"type":"replaceRules","destination":"session","behavior":"ask","rules":[]}])).unwrap();
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        manager.set_session_rules(
            "one",
            HashMap::from([("Write".into(), PermissionAction::Deny)]),
        );
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        manager.set_session_rules(
            "one",
            HashMap::from([("Write".into(), PermissionAction::Confirm)]),
        );
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
        manager.set_session_rules("one", HashMap::new());
        drop(Cleanup(updates));
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
    }

    fn scope(temp: &tempfile::TempDir) -> SettingsScope {
        SettingsScope {
            data_dir: temp.path().join("data"),
            workspace: temp.path().join("project"),
            project_trusted: true,
        }
    }

    #[test]
    fn persisted_destinations_are_atomic_preserve_unknown_fields_and_bom() {
        let temp = tempfile::tempdir().unwrap();
        let scope = scope(&temp);
        std::fs::create_dir_all(&scope.workspace.join(".claude")).unwrap();
        std::fs::create_dir_all(&scope.data_dir).unwrap();
        let user = scope.data_dir.join("settings.json");
        std::fs::write(&user, b"\xEF\xBB\xBF{\n  \"theme\": \"dark\",\n  \"permissions\": {\"allow\": [\"Read\"]}\n}\n").unwrap();
        let local = scope.workspace.join(".claude").join("settings.local.json");
        std::fs::write(&local, json!({"permissions":{"deny":["Bash(rm *)"]}}).to_string()).unwrap();
        let manager = Arc::new(PermissionManager::new(Arc::new(AuditLogger::new(16))));
        manager.set_mode(PermissionMode::AskPermissions);
        let updates = Updates::new(manager.clone(), "one".into()).with_settings(scope.clone());
        updates
            .apply(&json!([
                {"type":"addRules","destination":"userSettings","behavior":"allow","rules":[{"toolName":"Write"},{"toolName":"Read"}]},
                {"type":"addRules","destination":"localSettings","behavior":"deny","rules":[{"toolName":"Edit"}]},
                {"type":"addDirectories","destination":"userSettings","directories":[scope.workspace.to_string_lossy()]},
                {"type":"addDirectories","destination":"localSettings","directories":[scope.data_dir.to_string_lossy()]},
                {"type":"setMode","destination":"localSettings","mode":"dontAsk"}
            ]))
            .unwrap();
        let bytes = std::fs::read(&user).unwrap();
        assert!(bytes.starts_with(&[0xEF, 0xBB, 0xBF]));
        let user_value: Value = serde_json::from_slice(&bytes[3..]).unwrap();
        assert_eq!(user_value["theme"], "dark");
        assert_eq!(user_value["permissions"]["allow"], json!(["Read", "Write"]));
        assert_eq!(user_value["permissions"]["defaultMode"], Value::Null);
        assert!(user_value["permissions"]["additionalDirectories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == &json!(scope.workspace.to_string_lossy())));
        let local_value: Value = serde_json::from_slice(&std::fs::read(&local).unwrap()).unwrap();
        assert_eq!(local_value["permissions"]["deny"], json!(["Bash(rm *)", "Edit"]));
        assert_eq!(local_value["permissions"]["defaultMode"], "dontAsk");
        assert_eq!(manager.get_mode(), PermissionMode::DontAsk);
        assert!(!manager.session_directories("one").is_empty());

        // A failed batch leaves every file untouched.
        let before_user = std::fs::read(&user).unwrap();
        let before_local = std::fs::read(&local).unwrap();
        assert!(updates
            .apply(&json!([
                {"type":"addRules","destination":"userSettings","behavior":"deny","rules":[{"toolName":"Write"}]},
                {"type":"addRules","destination":"localSettings","behavior":"allow","rules":[{"toolName":"LSP","ruleContent":"*"}]}
            ]))
            .is_err());
        assert_eq!(std::fs::read(&user).unwrap(), before_user);
        assert_eq!(std::fs::read(&local).unwrap(), before_local);
    }

    #[test]
    fn untrusted_projects_cannot_persist_and_rules_reload_with_deny_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let mut scope = scope(&temp);
        scope.project_trusted = false;
        std::fs::create_dir_all(scope.workspace.join(".claude")).unwrap();
        let manager = Arc::new(PermissionManager::new(Arc::new(AuditLogger::new(16))));
        let updates = Updates::new(manager, "one".into()).with_settings(scope.clone());
        assert!(updates
            .apply(&json!([
                {"type":"addRules","destination":"projectSettings","behavior":"allow","rules":[{"toolName":"Write"}]}
            ]))
            .is_err());
        assert!(!scope.workspace.join(".claude").join("settings.json").exists());

        std::fs::create_dir_all(scope.workspace.join(".claude")).unwrap();
        std::fs::create_dir_all(&scope.data_dir).unwrap();
        std::fs::write(
            scope.data_dir.join("settings.json"),
            json!({"permissions":{"allow":["Write"],"deny":["Bash(rm *)"]}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            scope.workspace.join(".claude").join("settings.json"),
            json!({"permissions":{"allow":["Edit"],"deny":["Write"],"additionalDirectories":[scope.workspace.to_string_lossy()]}}).to_string(),
        )
        .unwrap();
        let loaded = load_settings(&scope.data_dir, &scope.workspace, true).unwrap();
        assert_eq!(loaded.rules.get("Write"), Some(&PermissionAction::Deny));
        assert_eq!(loaded.rules.get("Edit"), Some(&PermissionAction::Allow));
        assert_eq!(loaded.rules.get("Bash(rm *)"), Some(&PermissionAction::Deny));
        assert_eq!(loaded.directories.len(), 1);
    }

    #[test]
    fn local_allow_rules_are_added_once_and_keep_other_settings() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path();
        std::fs::create_dir_all(workspace.join(".claude")).unwrap();
        let path = workspace.join(".claude").join("settings.local.json");
        std::fs::write(&path, json!({"env":{"A":"1"},"permissions":{"allow":["Read"]}}).to_string()).unwrap();
        assert_eq!(add_local_allow_rule(workspace, "WebFetch(domain:example.com)").unwrap(), path);
        add_local_allow_rule(workspace, "WebFetch(domain:example.com)").unwrap();
        assert!(add_local_allow_rule(workspace, "Bash(").is_err());
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["env"]["A"], "1");
        assert_eq!(saved["permissions"]["allow"], json!(["Read", "WebFetch(domain:example.com)"]));
        let loaded = load_settings(&temp.path().join("data"), workspace, true).unwrap();
        assert_eq!(loaded.rules.get("WebFetch(domain:example.com)"), Some(&PermissionAction::Allow));
    }
}

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    #[test]
    fn concurrent_process_writes_do_not_lose_rules() {
        let temp = tempfile::tempdir().unwrap();
        let scope = SettingsScope {
            data_dir: temp.path().join("data"),
            workspace: temp.path().join("project"),
            project_trusted: true,
        };
        std::fs::create_dir_all(&scope.data_dir).unwrap();
        let mut threads = Vec::new();
        for index in 0..8 {
            let scope = scope.clone();
            threads.push(std::thread::spawn(move || {
                let manager = Arc::new(PermissionManager::new(Arc::new(
                    crate::permissions::AuditLogger::new(16),
                )));
                Updates::new(manager, format!("s{index}"))
                    .with_settings(scope)
                    .apply(&json!([{
                        "type":"addRules",
                        "destination":"userSettings",
                        "behavior":"allow",
                        "rules":[{"toolName":format!("Tool{index}")}]
                    }]))
                    .unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let value: Value = serde_json::from_slice(
            &std::fs::read(scope.data_dir.join("settings.json")).unwrap(),
        )
        .unwrap();
        let rules = value["permissions"]["allow"].as_array().unwrap();
        for index in 0..8 {
            assert!(
                rules.iter().any(|rule| rule == &json!(format!("Tool{index}"))),
                "{value}"
            );
        }
    }
}
