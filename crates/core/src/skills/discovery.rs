//! Read-only discovery of user/project Claude skill and command files.
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize)]
pub struct SkillCommand {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub content: String,
    pub user_invocable: bool,
    pub disable_model_invocation: bool,
    pub context: Option<String>,
    pub shell: Option<String>,
    pub agent: Option<String>,
}

impl SkillCommand {
    /// Model invocation uses the Skill tool, including fork/shell execution.
    /// user-invocable only controls manual slash invocation.
    pub fn model_invocable(&self) -> bool {
        !self.disable_model_invocation
    }
    pub fn render(&self, arguments: &str, session_id: &str) -> Result<String> {
        ensure!(
            self.user_invocable,
            "Skill /{} is not user-invocable",
            self.name
        );
        ensure!(
            self.context.as_deref() != Some("fork"),
            "Skill /{} requires host-managed fork isolation; invoke it through the CLI skill handler",
            self.name
        );
        let template = self.render_template(arguments, session_id)?;
        ensure!(
            !super::invocation::parse(&template)?.iter().any(|part| matches!(part, super::invocation::Segment::Shell(_))),
            "Skill /{} requires host-managed shell approval; invoke it through the CLI skill handler",
            self.name
        );
        Ok(template)
    }

    /// Pure substitution only. Hosts must handle fork isolation and explicitly
    /// authorize any parsed shell commands before sending this text to a model.
    pub fn render_template(&self, arguments: &str, session_id: &str) -> Result<String> {
        ensure!(
            self.user_invocable,
            "Skill /{} is not user-invocable",
            self.name
        );
        let mut text = expand_arguments(&self.content, arguments)?;
        text = text.replace("${CLAUDE_SESSION_ID}", session_id).replace(
            "${CLAUDE_SKILL_DIR}",
            &self
                .path
                .parent()
                .unwrap_or(Path::new("."))
                .to_string_lossy()
                .replace('\\', "/"),
        );
        if self.path.parent() == Some(Path::new("__embedded_cli_skills__")) {
            return Ok(format!(
                "Bundled workflow /{} (embedded instructions; no source file to read)\n\n{}",
                self.name, text
            ));
        }
        Ok(format!("Skill /{} from {}\nResolve referenced files relative to this skill directory: {}\n\n{}", self.name, self.path.display(), self.path.parent().unwrap_or(Path::new(".")).display(), text))
    }
}

/// One resolver for the model-visible listing, the `Skill` tool and the CLI
/// slash list. Precedence, lowest to highest: embedded workflows,
/// `CLAUDE_CONFIG_DIR` (or `~/.claude`), `<data dir>/skills`,
/// `<workspace>/.claude`. The data directory is the native user scope, so it
/// overrides the Claude-compatible user root while the project still wins.
/// Data-directory names keep the pre-round-01 lowercase slugs (see
/// [`scan_data_dir`]); the other roots keep their existing spelling.
pub fn discover(data_dir: &Path, workspace: &Path) -> Result<BTreeMap<String, SkillCommand>> {
    let user = user_config_dir();
    Ok(discover_runtime_from(user.as_deref(), data_dir, workspace).0)
}

fn user_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
}

/// Explicit roots for tests; same precedence as [`discover`].
pub fn discover_runtime_from(
    user_config: Option<&Path>,
    data_dir: &Path,
    workspace: &Path,
) -> (BTreeMap<String, SkillCommand>, Vec<PathBuf>) {
    let mut commands = super::bundled::commands(workspace);
    let mut invalid = Vec::new();
    if let Some(user) = user_config {
        let _ = scan_config_impl(user, &mut commands, Some(&mut invalid));
    }
    let _ = scan_data_dir(&data_dir.join("skills"), &mut commands, Some(&mut invalid));
    let _ = scan_config_impl(
        &workspace.join(".claude"),
        &mut commands,
        Some(&mut invalid),
    );
    log_invalid(&invalid);
    (commands, invalid)
}

/// Tolerant runtime discovery without a data directory (diagnostics only).
/// Invalid files never override valid definitions.
/// Errors retain paths only: parser messages may contain credential values.
pub fn discover_available_from(
    user_config: Option<&Path>,
    workspace: &Path,
) -> (BTreeMap<String, SkillCommand>, Vec<PathBuf>) {
    let mut commands = super::bundled::commands(workspace);
    let mut invalid = Vec::new();
    for config in user_config
        .into_iter()
        .map(Path::to_path_buf)
        .chain(std::iter::once(workspace.join(".claude")))
    {
        let _ = scan_config_impl(&config, &mut commands, Some(&mut invalid));
    }
    log_invalid(&invalid);
    (commands, invalid)
}

fn log_invalid(invalid: &[PathBuf]) {
    for path in invalid {
        let safe: String = path
            .to_string_lossy()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(240)
            .collect();
        tracing::warn!(path = %safe, category = "invalid_or_unreadable_skill", "Skipping invalid skill; run /doctor");
    }
}

/// Explicit roots allow fixture tests without reading or running personal skills.
pub fn discover_from(
    user_config: Option<&Path>,
    workspace: &Path,
) -> Result<BTreeMap<String, SkillCommand>> {
    let mut commands = super::bundled::commands(workspace);
    if let Some(user) = user_config {
        scan_config(user, &mut commands)?;
    }
    scan_config(&workspace.join(".claude"), &mut commands)?;
    Ok(commands)
}

/// Discover from a materialized, validated plugin bundle. Namespace prevents
/// collisions with user/project commands while references keep their real path.
pub fn discover_plugin(
    root: &Path,
    namespace: &str,
    skill_paths: &[String],
    command_paths: &[String],
) -> Result<BTreeMap<String, SkillCommand>> {
    let mut commands = BTreeMap::new();
    for (paths, is_skill) in [(command_paths, false), (skill_paths, true)] {
        for relative in paths {
            let selected = root.join(relative);
            ensure!(
                selected.exists(),
                "Plugin feature path does not exist: {relative}"
            );
            for entry in walkdir::WalkDir::new(&selected)
                .follow_links(false)
                .max_depth(16)
            {
                let entry = entry?;
                if !entry.file_type().is_file() {
                    continue;
                }
                if (is_skill && entry.file_name() != "SKILL.md")
                    || (!is_skill
                        && entry
                            .path()
                            .extension()
                            .is_none_or(|extension| extension != "md"))
                {
                    continue;
                }
                let path = entry.path();
                let text = std::fs::read_to_string(path)?;
                let (metadata, content) = frontmatter(&text)?;
                if !boolean(&metadata, "enabled", true)? {
                    continue;
                }
                let name_path = if selected.is_file() {
                    selected
                        .file_stem()
                        .context("Plugin command filename missing")?
                        .to_string_lossy()
                        .into_owned()
                } else if is_skill {
                    let parent = path
                        .parent()
                        .context("Plugin skill directory missing")?
                        .strip_prefix(&selected)?;
                    if parent.as_os_str().is_empty() {
                        selected
                            .file_name()
                            .context("Plugin skill name missing")?
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        parent.to_string_lossy().replace(['/', '\\'], ":")
                    }
                } else {
                    path.strip_prefix(&selected)?
                        .with_extension("")
                        .to_string_lossy()
                        .replace(['/', '\\'], ":")
                };
                let name = metadata
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&name_path);
                ensure!(
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')),
                    "Invalid plugin skill name"
                );
                let name = format!("{namespace}:{name}");
                commands.insert(
                    name.clone(),
                    SkillCommand {
                        name,
                        description: metadata
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("Run plugin skill instructions")
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" "),
                        path: path.to_path_buf(),
                        content: content.replace(
                            "${CLAUDE_PLUGIN_ROOT}",
                            &root.to_string_lossy().replace('\\', "/"),
                        ),
                        user_invocable: boolean(&metadata, "user-invocable", true)?,
                        disable_model_invocation: boolean(
                            &metadata,
                            "disable-model-invocation",
                            false,
                        )?,
                        context: metadata
                            .get("context")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        shell: metadata
                            .get("shell")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        agent: metadata
                            .get("agent")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    },
                );
            }
        }
    }
    Ok(commands)
}

fn scan_config(config: &Path, commands: &mut BTreeMap<String, SkillCommand>) -> Result<()> {
    scan_config_impl(config, commands, None)
}

fn scan_config_impl(
    config: &Path,
    commands: &mut BTreeMap<String, SkillCommand>,
    mut invalid: Option<&mut Vec<PathBuf>>,
) -> Result<()> {
    // Skills win over legacy commands in the same scope.
    for (directory, skill_format) in [("commands", false), ("skills", true)] {
        let root = config.join(directory);
        if !root.exists() {
            continue;
        }
        for path in collect_paths(&root, skill_format, invalid.as_deref_mut())? {
            let invalid_path = path.clone();
            match parse_definition(&root, &path, skill_format, false) {
                Ok(Some(command)) => {
                    commands.insert(command.name.clone(), command);
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(invalid) = invalid.as_mut() {
                        invalid.push(invalid_path);
                    } else {
                        return Err(error);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Read-only scan of `<data dir>/skills`. Names keep the pre-round-01 slash
/// spelling: the lowercase slug of the skill's own name, or its path-stable id
/// when that slug is empty, unusable or shared with another data-dir skill.
/// Managed subdirectories (`bundled`, `mcp`) stay visible, as they already were
/// in the slash list, but never replace an embedded workflow or a Claude-user
/// definition of the same name.
fn scan_data_dir(
    root: &Path,
    commands: &mut BTreeMap<String, SkillCommand>,
    mut invalid: Option<&mut Vec<PathBuf>>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let mut entries = Vec::new();
    for path in collect_paths(root, true, invalid.as_deref_mut())? {
        let invalid_path = path.clone();
        let managed = path
            .strip_prefix(root)
            .ok()
            .and_then(|relative| relative.components().next())
            .is_some_and(|component| {
                matches!(component.as_os_str().to_str(), Some("bundled" | "mcp"))
            });
        match parse_definition(root, &path, true, true) {
            Ok(Some(command)) => entries.push((command, managed)),
            Ok(None) => {}
            Err(error) => {
                if let Some(invalid) = invalid.as_mut() {
                    invalid.push(invalid_path);
                } else {
                    return Err(error);
                }
            }
        }
    }
    let mut counts = BTreeMap::new();
    for (command, _) in &entries {
        *counts.entry(command.name.clone()).or_insert(0usize) += 1;
    }
    let mut used = BTreeSet::new();
    for (mut command, managed) in entries {
        let shared = counts.get(&command.name).is_some_and(|count| *count > 1);
        let usable = !command.name.is_empty()
            && !command.name.contains('/')
            && !command.name.chars().any(char::is_control);
        if shared || !usable {
            command.name = legacy_skill_id(&command.path, &used);
        }
        if managed && commands.contains_key(&command.name) {
            continue;
        }
        used.insert(command.name.clone());
        commands.insert(command.name.clone(), command);
    }
    Ok(())
}

/// Pre-round-01 slash names in the data directory: the skill name lowercased,
/// with whitespace collapsed to `-`. Other punctuation is kept so commands that
/// earlier releases listed, such as `/r&d-(team)'s-check`, still resolve.
fn legacy_data_dir_name(raw: &str) -> String {
    raw.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

/// The id the data-directory manager used before round 01. Entries are visited
/// in sorted path order, so the result never depends on filesystem enumeration
/// order; only an actual hash collision appends a counter.
fn legacy_skill_id(path: &Path, used: &BTreeSet<String>) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    let base = format!("skill_{:x}", hasher.finish());
    if !used.contains(&base) {
        return base;
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !used.contains(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn collect_paths(
    root: &Path,
    skill_format: bool,
    invalid: Option<&mut Vec<PathBuf>>,
) -> Result<Vec<PathBuf>> {
    let mut invalid = invalid;
    let mut paths = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(16)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if let Some(invalid) = invalid.as_mut() {
                    invalid.push(error.path().unwrap_or(root).to_path_buf());
                    continue;
                }
                return Err(error).with_context(|| format!("Scanning {}", root.display()));
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let filename = entry.file_name().to_string_lossy();
        if (skill_format && filename == "SKILL.md")
            || (!skill_format
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "md"))
        {
            paths.push(entry.into_path());
        }
    }
    paths.sort();
    Ok(paths)
}

/// `root` is the naming root for config-scope relative names and local plugin
/// metadata. Data-directory skills use the pre-round-01 slash spelling instead:
/// the lowercase slug of the skill's own name (see [`legacy_data_dir_name`]).
fn parse_definition(
    root: &Path,
    path: &Path,
    skill_format: bool,
    data_dir: bool,
) -> Result<Option<SkillCommand>> {
    ensure!(
        std::fs::metadata(path)?.len() <= 1024 * 1024,
        "Skill file exceeds 1 MiB: {}",
        path.display()
    );
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Reading UTF-8 skill {}", path.display()))?;
    let (metadata, content) =
        frontmatter(&text).with_context(|| format!("Parsing skill {}", path.display()))?;
    if boolean(&metadata, "enabled", true)? == false {
        return Ok(None);
    }
    let name = if data_dir {
        let raw = metadata
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                path.parent()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        legacy_data_dir_name(&raw)
    } else {
        let relative = path.strip_prefix(root)?;
        let stem = if skill_format {
            relative
                .parent()
                .context("Skill must be in a named directory")?
                .to_path_buf()
        } else {
            relative.with_extension("")
        };
        let mut parts = stem
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        // A plugin checkout inside a user skill directory has its own
        // namespace and source root; structural `skills/` is not a command
        // namespace. Ordinary nested skill collections retain their paths.
        if skill_format {
            if let Some(plugin_parts) = local_plugin_name(root, path)? {
                parts = plugin_parts;
            }
        }
        ensure!(
            !parts.is_empty(),
            "Skill must be in a named directory: {}",
            path.display()
        );
        if let Some(name) = metadata.get("name").and_then(Value::as_str) {
            *parts.last_mut().unwrap() = name.into();
        }
        let name = parts.join(":");
        ensure!(
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')),
            "Invalid skill command name: {name}"
        );
        name
    };
    Ok(Some(SkillCommand {
        name: name.clone(),
        description: metadata
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("Run local skill instructions")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        path: path.to_path_buf(),
        content,
        user_invocable: boolean(
            &metadata,
            "user-invocable",
            boolean(&metadata, "userInvocable", true)?,
        )?,
        disable_model_invocation: boolean(&metadata, "disable-model-invocation", false)?,
        context: metadata
            .get("context")
            .and_then(Value::as_str)
            .map(str::to_owned),
        shell: metadata
            .get("shell")
            .and_then(Value::as_str)
            .map(str::to_owned),
        agent: metadata
            .get("agent")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }))
}

fn local_plugin_name(scan_root: &Path, skill_file: &Path) -> Result<Option<Vec<String>>> {
    for ancestor in skill_file.parent().into_iter().flat_map(Path::ancestors) {
        if ancestor == scan_root || !ancestor.starts_with(scan_root) {
            break;
        }
        let manifest_path = ancestor.join(".claude-plugin/plugin.json");
        if !manifest_path.is_file() {
            continue;
        }
        ensure!(
            std::fs::metadata(&manifest_path)?.len() <= 1024 * 1024,
            "Plugin manifest exceeds 1 MiB"
        );
        let manifest: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)
            .with_context(|| format!("Parsing plugin manifest {}", manifest_path.display()))?;
        let namespace = manifest
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| ancestor.file_name().and_then(|name| name.to_str()))
            .context("Plugin namespace missing")?;
        let mut sources = vec!["skills".to_owned()];
        if let Some(paths) = manifest.get("skills") {
            if let Some(path) = paths.as_str() {
                sources.push(path.into());
            } else if let Some(paths) = paths.as_array() {
                sources.extend(paths.iter().filter_map(Value::as_str).map(str::to_owned));
            }
        }
        for source in sources {
            let source = Path::new(&source);
            if source.is_absolute()
                || source.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir | std::path::Component::Prefix(_)
                    )
                })
            {
                continue;
            }
            let source_root = ancestor.join(source);
            if let Ok(relative) = skill_file.strip_prefix(&source_root) {
                let parent = relative.parent().context("Plugin skill has no directory")?;
                let mut parts = vec![namespace.to_owned()];
                parts.extend(
                    parent
                        .iter()
                        .map(|part| part.to_string_lossy().into_owned()),
                );
                if parts.len() == 1 {
                    parts.push(
                        source_root
                            .file_name()
                            .and_then(|part| part.to_str())
                            .unwrap_or("skill")
                            .into(),
                    );
                }
                return Ok(Some(parts));
            }
        }
    }
    Ok(None)
}

fn boolean(metadata: &Value, key: &str, default: bool) -> Result<bool> {
    metadata
        .get(key)
        .map(|v| {
            v.as_bool()
                .with_context(|| format!("{key} must be boolean"))
        })
        .unwrap_or(Ok(default))
}

pub(crate) fn frontmatter(text: &str) -> Result<(Value, String)> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    if !text.starts_with("---\n") {
        return Ok((serde_json::json!({}), text));
    }
    let end = text[4..]
        .find("\n---")
        .context("Unterminated YAML frontmatter")?
        + 4;
    let after = end + 4;
    ensure!(
        text[after..].starts_with('\n') || after == text.len(),
        "Invalid YAML frontmatter delimiter"
    );
    let yaml: serde_yaml::Value = serde_yaml::from_str(&text[4..end])?;
    let metadata = serde_json::to_value(yaml)?;
    ensure!(
        metadata.is_object() || metadata.is_null(),
        "Skill frontmatter must be a mapping"
    );
    Ok((metadata, text[after..].trim_start_matches('\n').to_owned()))
}

fn split_arguments(arguments: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut active = false;
    for ch in arguments.chars() {
        if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') {
            quote = Some(ch);
            active = true;
        } else if quote.is_none() && ch.is_whitespace() {
            if active {
                words.push(std::mem::take(&mut word));
                active = false;
            }
        } else {
            word.push(ch);
            active = true;
        }
    }
    ensure!(quote.is_none(), "Unterminated quote in skill arguments");
    if active {
        words.push(word);
    }
    Ok(words)
}

fn expand_arguments(content: &str, arguments: &str) -> Result<String> {
    let words = split_arguments(arguments)?;
    let pattern = regex::Regex::new(r"\$ARGUMENTS\[(\d+)\]|\$(\d+)|\$ARGUMENTS\b")?;
    let had_placeholder = pattern.is_match(content);
    let mut rendered = pattern
        .replace_all(content, |captures: &regex::Captures<'_>| {
            if let Some(index) = captures.get(1).or_else(|| captures.get(2)) {
                return index
                    .as_str()
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| words.get(i))
                    .cloned()
                    .unwrap_or_default();
            }
            arguments.to_owned()
        })
        .into_owned();
    if !had_placeholder && !arguments.trim().is_empty() {
        rendered.push_str(&format!("\n\nUser arguments: {arguments}"));
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    #[test]
    fn tolerant_discovery_keeps_good_siblings_and_bundled_doctor() {
        let root = tempfile::tempdir().unwrap();
        let commands = root.path().join(".claude/commands");
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(commands.join("good.md"), "Good instructions").unwrap();
        std::fs::write(commands.join("doctor.md"), "---\nname: doctor\nenabled: NOT_A_BOOL_SECRET\n---\nInvalid replacement").unwrap();
        let (available, invalid) = super::discover_available_from(None, root.path());
        assert!(available["good"].content.contains("Good instructions"));
        assert_eq!(available["doctor"].path.parent().unwrap(), std::path::Path::new("__embedded_cli_skills__"));
        assert_eq!(invalid, vec![commands.join("doctor.md")]);
        assert!(super::discover_from(None, root.path()).is_err());
    }

    use super::*;
    #[test]
    fn fork_metadata_does_not_bypass_user_invocation_restrictions() {
        let temp = tempfile::tempdir().unwrap();
        let skill = temp.path().join(".claude/skills/private");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\ncontext: fork\nagent: Explore\nshell: powershell\nuser-invocable: false\n---\n!`Write-Output hidden`").unwrap();
        let commands = discover_from(None, temp.path()).unwrap();
        let skill = &commands["private"];
        assert_eq!(skill.context.as_deref(), Some("fork"));
        assert_eq!(skill.agent.as_deref(), Some("Explore"));
        assert_eq!(skill.shell.as_deref(), Some("powershell"));
        assert!(skill.model_invocable());
        let mut dynamic = skill.clone();
        dynamic.context = None;
        assert!(dynamic.model_invocable());
        dynamic.content = "Read and explain this project".into();
        assert!(dynamic.model_invocable());
        assert!(skill
            .render_template("", "session")
            .unwrap_err()
            .to_string()
            .contains("not user-invocable"));
    }

    #[test]
    fn project_overrides_user_and_nested_names_are_preserved() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let workspace = root.path().join("project");
        for config in [&user, &workspace.join(".claude")] {
            std::fs::create_dir_all(config.join("skills/team/review")).unwrap();
            std::fs::write(config.join("skills/team/review/SKILL.md"), format!("---\ndescription: Review files\ndisable-model-invocation: true\n---\n{} $ARGUMENTS", config.display())).unwrap();
        }
        std::fs::create_dir_all(workspace.join(".claude/commands")).unwrap();
        std::fs::write(
            workspace.join(".claude/commands/hidden.md"),
            "---\nuser-invocable: false\n---\nHidden",
        )
        .unwrap();
        let found = discover_from(Some(&user), &workspace).unwrap();
        assert!(found["team:review"].path.starts_with(&workspace));
        assert!(found["team:review"].disable_model_invocation);
        assert!(found["team:review"]
            .render("src/main.rs", "id")
            .unwrap()
            .contains("src/main.rs"));
        assert!(!found["hidden"].user_invocable);
        assert!(found["hidden"].render("", "id").is_err());
    }

    #[test]
    fn argument_expansion_preserves_quotes_and_does_not_recursively_expand_user_input() {
        assert_eq!(
            expand_arguments("$0|$1|$ARGUMENTS[2]", "'two words' second third").unwrap(),
            "two words|second|third"
        );
        assert_eq!(expand_arguments("$ARGUMENTS", "$0").unwrap(), "$0");
        assert_eq!(
            expand_arguments("Instructions", "foo").unwrap(),
            "Instructions\n\nUser arguments: foo"
        );
        assert!(expand_arguments("$0", "'unclosed").is_err());
    }

    #[test]
    fn utf8_bom_crlf_and_invalid_frontmatter_are_handled() {
        assert_eq!(
            frontmatter("\u{feff}---\r\ndescription: test\r\n---\r\nbody")
                .unwrap()
                .1,
            "body"
        );
        assert!(frontmatter("---\nname: test\nbody").is_err());
        assert!(boolean(&serde_json::json!({"enabled":"false"}), "enabled", true).is_err());
    }
    #[test]
    fn plugin_checkouts_use_manifest_namespace_and_skip_structural_skills_directory() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let plugin = user.join("skills/checkout-name");
        std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
        std::fs::write(
            plugin.join(".claude-plugin/plugin.json"),
            r#"{"name":"vendor-tools","skills":["./extra"]}"#,
        )
        .unwrap();
        for (directory, name) in [
            ("skills/source-name", "deploy"),
            ("extra/local-name", "inspect"),
        ] {
            std::fs::create_dir_all(plugin.join(directory)).unwrap();
            std::fs::write(
                plugin.join(directory).join("SKILL.md"),
                format!("---\nname: {name}\n---\nRun $ARGUMENTS"),
            )
            .unwrap();
        }
        let ordinary = user.join("skills/collection/skills/nested");
        std::fs::create_dir_all(&ordinary).unwrap();
        std::fs::write(ordinary.join("SKILL.md"), "Ordinary nested skill").unwrap();
        let found = discover_from(Some(&user), temp.path()).unwrap();
        assert!(found.contains_key("vendor-tools:deploy"));
        assert!(found.contains_key("vendor-tools:inspect"));
        assert!(found.contains_key("collection:skills:nested"));
        assert!(!found.keys().any(|name| name.starts_with("checkout-name:")));
        assert!(found["vendor-tools:deploy"]
            .render("production", "id")
            .unwrap()
            .contains("Run production"));
    }

    fn write_skill(config: &Path, relative: &str, body: &str) {
        let directory = config.join(relative);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn runtime_discovery_lists_data_dir_skills_with_the_real_directory() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        std::fs::create_dir_all(&user).unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let data = temp.path().join("data");
        write_skill(
            &data.join("skills"),
            "data-dir-check",
            "---\nname: data-dir-check\ndescription: Data directory fixture\n---\nRead references/setup.md, then inspect $ARGUMENTS",
        );
        let (found, invalid) = discover_runtime_from(Some(&user), &data, &workspace);
        assert!(invalid.is_empty(), "{invalid:?}");
        let skill = &found["data-dir-check"];
        assert_eq!(skill.path, data.join("skills/data-dir-check/SKILL.md"));
        let rendered = skill.render_template("the target", "session-id").unwrap();
        assert!(rendered.contains("the target"));
        assert!(rendered.contains(
            &data
                .join("skills/data-dir-check")
                .to_string_lossy()
                .replace('\\', "/")
        ));
        assert!(rendered.contains("references/setup.md"));
    }

    #[test]
    fn runtime_discovery_prefers_project_then_data_dir_then_claude_user_and_lists_one_entry() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let workspace = temp.path().join("project");
        let data = temp.path().join("data");
        for (root, label) in [
            (&user, "CLAUDE_USER"),
            (&data.join("skills"), "DATA_DIR"),
            (&workspace.join(".claude/skills"), "PROJECT"),
        ] {
            write_skill(
                root,
                "shared",
                &format!("---\nname: shared\ndescription: {label}\n---\n{label}"),
            );
        }
        let (found, _) = discover_runtime_from(Some(&user), &data, &workspace);
        assert_eq!(
            found
                .values()
                .filter(|skill| skill.name == "shared")
                .count(),
            1
        );
        assert_eq!(found["shared"].description, "PROJECT");
        assert!(found["shared"].path.starts_with(&workspace));

        std::fs::remove_dir_all(workspace.join(".claude/skills/shared")).unwrap();
        let (found, _) = discover_runtime_from(Some(&user), &data, &workspace);
        assert_eq!(found["shared"].description, "DATA_DIR");
        assert!(found["shared"].path.starts_with(&data));
    }

    #[test]
    fn runtime_discovery_honors_enabled_and_model_invocation_switches() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let data = temp.path().join("data");
        let skills = data.join("skills");
        write_skill(
            &skills,
            "disabled",
            "---\nname: disabled\nenabled: false\n---\nHidden",
        );
        write_skill(
            &skills,
            "model-hidden",
            "---\nname: model-hidden\ndisable-model-invocation: true\n---\nManual only",
        );
        write_skill(
            &skills,
            "manual-only",
            "---\nname: manual-only\nuser-invocable: false\n---\nModel only",
        );
        let (found, invalid) = discover_runtime_from(None, &data, &workspace);
        assert!(invalid.is_empty(), "{invalid:?}");
        assert!(!found.contains_key("disabled"));
        let hidden = &found["model-hidden"];
        assert!(!hidden.model_invocable());
        assert!(hidden.user_invocable);
        assert!(hidden.render_template("", "id").is_ok());
        let manual = &found["manual-only"];
        assert!(manual.model_invocable());
        assert!(!manual.user_invocable);
        assert!(manual.render_template("", "id").is_err());
    }

    #[test]
    fn runtime_discovery_skips_an_invalid_data_dir_skill_beside_a_valid_one() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let data = temp.path().join("data");
        let skills = data.join("skills");
        write_skill(
            &skills,
            "good",
            "---\nname: good\ndescription: Good fixture\n---\nUseful",
        );
        let invalid_path = skills.join("bad/SKILL.md");
        write_skill(
            &skills,
            "bad",
            "---\nname: bad\nenabled: NOT_A_BOOL\n---\nBroken",
        );
        let (found, invalid) = discover_runtime_from(None, &data, &workspace);
        assert!(found.contains_key("good"));
        assert!(!found.contains_key("bad"));
        assert_eq!(invalid, vec![invalid_path]);
    }

    #[test]
    fn runtime_discovery_lists_managed_data_dirs_without_shadowing_embedded_workflows() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let data = temp.path().join("data");
        let skills = data.join("skills");
        write_skill(
            &skills.join("bundled"),
            "doctor",
            "---\nname: doctor\n---\nMANAGED_DOCTOR",
        );
        write_skill(
            &skills.join("bundled"),
            "legacy-commit",
            "---\nname: commit\ndescription: Commit fixture\n---\nCommit instructions",
        );
        write_skill(
            &skills.join("mcp"),
            "mcp-fixture",
            "---\nname: mcp-fixture\ndescription: MCP fixture\n---\nMCP instructions",
        );
        write_skill(
            &skills,
            "code-review",
            "---\nname: code-review\ndescription: User review\n---\nUSER_OVERRIDE",
        );
        let (found, invalid) = discover_runtime_from(None, &data, &workspace);
        assert!(invalid.is_empty(), "{invalid:?}");
        // A managed copy must not replace the embedded workflow.
        assert_eq!(
            found["doctor"].path.parent(),
            Some(Path::new("__embedded_cli_skills__"))
        );
        assert!(!found["doctor"].content.contains("MANAGED_DOCTOR"));
        // Managed directories are still listed when their name is free.
        assert!(found.contains_key("commit"));
        assert!(found.contains_key("mcp-fixture"));
        assert!(found["commit"].model_invocable());
        assert!(found["commit"].user_invocable);
        // A direct user skill keeps the normal user-overrides-embedded rule.
        assert!(found["code-review"].content.contains("USER_OVERRIDE"));
    }

    #[test]
    fn installed_placeholder_and_legacy_builtins_are_visible_to_both_views() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let data = temp.path().join("data");
        crate::skills::SkillsManager::with_path(data.join("skills"))
            .ensure_installed()
            .unwrap();
        let (found, invalid) = discover_runtime_from(None, &data, &workspace);
        assert!(invalid.is_empty(), "{invalid:?}");
        // First-run placeholders are managed copies, but their names are free,
        // so the same name reaches the model listing and the slash list.
        for name in [
            "skill-creator",
            "commit",
            "review",
            "test",
            "document",
            "build",
        ] {
            let skill = &found[name];
            assert!(skill.model_invocable(), "{name} must stay model-invocable");
            assert!(skill.user_invocable, "{name} must stay user-invocable");
            assert!(skill.path.starts_with(data.join("skills")), "{name}");
        }
    }

    #[test]
    fn runtime_discovery_does_not_create_the_data_skills_directory() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let data = temp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let (found, _) = discover_runtime_from(None, &data, &workspace);
        assert!(!data.join("skills").exists());
        assert!(found.contains_key("doctor"));
    }

    fn legacy_id(path: &Path) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        path.to_string_lossy().hash(&mut hasher);
        format!("skill_{:x}", hasher.finish())
    }

    #[test]
    fn runtime_discovery_keeps_pre_round_01_slash_names_for_mixed_case_and_spaces() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let data = temp.path().join("data");
        write_skill(
            &data.join("skills"),
            "My Skill",
            "---\nname: My Skill\ndescription: Mixed case fixture\n---\nInspect $ARGUMENTS",
        );
        // Nested collections were also reachable by the skill's own name.
        write_skill(
            &data.join("skills/collection"),
            "Deep Nested",
            "---\nname: Deep Nested\ndescription: Nested fixture\n---\nNested",
        );
        let (found, invalid) = discover_runtime_from(None, &data, &workspace);
        assert!(invalid.is_empty(), "{invalid:?}");
        let keys = found.keys().collect::<Vec<_>>();
        assert!(found.contains_key("my-skill"), "{keys:?}");
        assert!(!found.contains_key("My-Skill"), "{keys:?}");
        assert_eq!(
            found["my-skill"].path,
            data.join("skills/My Skill/SKILL.md")
        );
        assert!(found.contains_key("deep-nested"), "{keys:?}");
        assert!(!found.contains_key("collection:deep-nested"), "{keys:?}");
    }

    #[test]
    fn runtime_discovery_keeps_punctuation_that_skill_creation_allows() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let data = temp.path().join("data");
        write_skill(
            &data.join("skills"),
            "Punctuation",
            "---\nname: R&D (team)'s check\ndescription: Punctuation fixture\n---\nCheck",
        );
        let (found, invalid) = discover_runtime_from(None, &data, &workspace);
        assert!(invalid.is_empty(), "{invalid:?}");
        let name = "r&d-(team)'s-check";
        let keys = found.keys().collect::<Vec<_>>();
        assert!(found.contains_key(name), "{keys:?}");
        assert_eq!(found[name].path, data.join("skills/Punctuation/SKILL.md"));
        assert_eq!(found.keys().filter(|key| key.starts_with("r&d")).count(), 1);
    }

    #[test]
    fn runtime_discovery_disambiguates_shared_data_dir_names_with_stable_ids() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let data = temp.path().join("data");
        let skills = data.join("skills");
        for directory in ["first-skill", "second-skill"] {
            write_skill(
                &skills,
                directory,
                "---\nname: Shared Skill\ndescription: Shared fixture\n---\nShared body",
            );
        }
        let first = discover_runtime_from(None, &data, &workspace).0;
        let second = discover_runtime_from(None, &data, &workspace).0;
        let first_path = skills.join("first-skill/SKILL.md");
        let second_path = skills.join("second-skill/SKILL.md");
        assert!(!first.contains_key("shared-skill"));
        for path in [&first_path, &second_path] {
            let id = legacy_id(path);
            let keys = first.keys().collect::<Vec<_>>();
            assert!(first.contains_key(&id), "{id} missing: {keys:?}");
            assert_eq!(first[&id].path, *path);
            assert_eq!(second[&id].path, *path);
        }
        assert_eq!(
            first
                .keys()
                .filter(|name| name.starts_with("skill_"))
                .count(),
            2
        );
        let ids = |found: &BTreeMap<String, SkillCommand>| {
            found
                .keys()
                .filter(|name| name.starts_with("skill_"))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&first), ids(&second));
    }
}
