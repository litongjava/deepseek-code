//! Per-skill states stored in `<data dir>/config/skill-states.json`.
//!
//! A skill without an entry gets its default state: [`SkillState::User`], except
//! the bundled translation skills, which default to [`SkillState::UserOnly`],
//! and installed built-in skills, which default to [`SkillState::Off`] except
//! `skill-creator`, the repository tooling skills, and `codegraph` when CodeGraph
//! is installed. Existing installations therefore keep a stored user choice
//! until the user changes it, while a built-in the user never touched follows
//! the current defaults.
use crate::skills::discovery::SkillCommand;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

const FILE_NAME: &str = "skill-states.json";
const TEMP_NAME: &str = "skill-states.json.tmp";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillState {
    Off,
    #[default]
    User,
    NameOnly,
    UserOnly,
    Always,
}

impl SkillState {
    pub fn label(self) -> &'static str {
        match self {
            SkillState::Off => "off",
            SkillState::User => "user",
            SkillState::NameOnly => "name-only",
            SkillState::UserOnly => "user-only",
            SkillState::Always => "always",
        }
    }
}

impl std::str::FromStr for SkillState {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "off" => Ok(SkillState::Off),
            "user" => Ok(SkillState::User),
            "name-only" => Ok(SkillState::NameOnly),
            "user-only" => Ok(SkillState::UserOnly),
            "always" => Ok(SkillState::Always),
            other => Err(anyhow::anyhow!(
                "Unknown skill state {other:?}; expected one of: off, user, name-only, user-only, always"
            )),
        }
    }
}

/// On-disk shape: `{"skills": {"<skill name>": "<label>"}}`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct SkillStatesFile {
    #[serde(default)]
    skills: BTreeMap<String, String>,
}

/// Read the state of every skill. Missing, unreadable or invalid files mean
/// that every skill stays at its default; an unknown label only skips its own
/// entry.
pub fn load(data_dir: &Path) -> BTreeMap<String, SkillState> {
    let path = crate::data_layout::DataLayout::from_root(data_dir).config_file(FILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "Ignoring unreadable skill states");
            return BTreeMap::new();
        }
    };
    let file: SkillStatesFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "Ignoring invalid skill states");
            return BTreeMap::new();
        }
    };
    let mut states = BTreeMap::new();
    for (name, label) in file.skills {
        match label.parse::<SkillState>() {
            Ok(state) => {
                states.insert(name, state);
            }
            Err(error) => tracing::warn!(skill = %name, %error, "Skipping unknown skill state"),
        }
    }
    states
}

/// Skills that ship hidden from the model: the user calls them by name.
const USER_ONLY_BY_DEFAULT: [&str; 2] = ["translate-zh-en", "translate-en-zh"];

/// The state a skill outside the built-in tree has when the states file does
/// not mention it.
pub fn default_state(name: &str) -> SkillState {
    if USER_ONLY_BY_DEFAULT.contains(&name) {
        SkillState::UserOnly
    } else {
        SkillState::User
    }
}

/// The default for one installed built-in skill.
///
/// Built-ins stay off unless they are part of the always-available toolkit or
/// depend on a tool the user actually installed. A stored state always wins
/// over this function, so a user choice survives upgrades and reinstallation.
pub fn bundled_default_state(name: &str) -> SkillState {
    match name {
        "skill-creator"
        | "dc-docs"
        | "llm-proxy-data-troubleshooting"
        | "project-docs" => SkillState::User,
        "codegraph" if codegraph_installed() => SkillState::Always,
        _ => SkillState::Off,
    }
}

/// Whether a `codegraph` executable is on `PATH`.
pub fn codegraph_installed() -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|directory| {
        codegraph_names()
            .iter()
            .any(|name| executable(&directory.join(name)))
    })
}

fn codegraph_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["codegraph", "codegraph.exe", "codegraph.cmd", "codegraph.bat"]
    }
    #[cfg(not(windows))]
    {
        &["codegraph"]
    }
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The stored state, or the default for a skill outside the built-in tree.
pub fn effective(states: &BTreeMap<String, SkillState>, name: &str) -> SkillState {
    effective_in(states, name, false)
}

/// The stored state, or the default for a built-in skill when `bundled`.
pub fn effective_in(
    states: &BTreeMap<String, SkillState>,
    name: &str,
    bundled: bool,
) -> SkillState {
    states.get(name).copied().unwrap_or_else(|| {
        if bundled {
            bundled_default_state(name)
        } else {
            default_state(name)
        }
    })
}

pub fn get(data_dir: &Path, name: &str) -> SkillState {
    effective(&load(data_dir), name)
}

/// Store one skill's state, keeping the other entries. The file is replaced
/// atomically so a concurrent reader never sees a partial document.
pub fn set(data_dir: &Path, name: &str, state: SkillState) -> Result<()> {
    let mut states = load(data_dir);
    states.insert(name.to_owned(), state);
    let layout = crate::data_layout::DataLayout::from_root(data_dir);
    std::fs::create_dir_all(layout.config_root())
        .with_context(|| format!("Creating {}", layout.config_root().display()))?;
    let file = SkillStatesFile {
        skills: states
            .into_iter()
            .map(|(name, state)| (name, state.label().to_owned()))
            .collect(),
    };
    let path = layout.config_file(FILE_NAME);
    let temp = layout.config_file(TEMP_NAME);
    let mut text = serde_json::to_string_pretty(&file)?;
    text.push('\n');
    std::fs::write(&temp, text).with_context(|| format!("Writing {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("Replacing {}", path.display()))?;
    Ok(())
}

/// Apply stored states to a command map that contains only non-built-in
/// skills. Names the file does not mention still get their default.
pub fn apply(commands: &mut BTreeMap<String, SkillCommand>, states: &BTreeMap<String, SkillState>) {
    apply_in(commands, states, None);
}

/// Apply stored states to the merged command map. A command under
/// `bundled_root` follows [`bundled_default_state`] until the user stores a
/// state for it; every other command keeps the non-built-in defaults.
pub fn apply_in(
    commands: &mut BTreeMap<String, SkillCommand>,
    states: &BTreeMap<String, SkillState>,
    bundled_root: Option<&Path>,
) {
    let names: Vec<String> = commands.keys().cloned().collect();
    for name in names {
        let bundled = bundled_root.is_some_and(|root| {
            commands
                .get(&name)
                .is_some_and(|command| command.path.starts_with(root))
        });
        match effective_in(states, &name, bundled) {
            SkillState::Off => {
                commands.remove(&name);
            }
            SkillState::User => {}
            SkillState::NameOnly => {
                if let Some(command) = commands.get_mut(&name) {
                    command.listing_name_only = true;
                }
            }
            SkillState::UserOnly => {
                if let Some(command) = commands.get_mut(&name) {
                    command.disable_model_invocation = true;
                    command.user_invocable = true;
                }
            }
            SkillState::Always => {
                if let Some(command) = commands.get_mut(&name) {
                    command.always_inline = true;
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) static PATH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn command(name: &str) -> SkillCommand {
        SkillCommand {
            name: name.to_owned(),
            description: "Fixture description".to_owned(),
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
            content: "Fixture body".to_owned(),
            user_invocable: true,
            disable_model_invocation: false,
            listing_name_only: false,
            always_inline: false,
            context: None,
            shell: None,
            agent: None,
        }
    }

    #[test]
    fn missing_or_invalid_file_means_every_skill_is_user() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(get(dir.path(), "anything"), SkillState::User);
        let config = crate::data_layout::DataLayout::from_root(dir.path()).config_root();
        std::fs::create_dir_all(&config).unwrap();

        std::fs::write(config.join(FILE_NAME), "not json").unwrap();
        assert_eq!(get(dir.path(), "anything"), SkillState::User);

        std::fs::write(
            config.join(FILE_NAME),
            r#"{"skills":{"known":"name-only","future":"brand-new"}}"#,
        )
        .unwrap();
        assert_eq!(get(dir.path(), "known"), SkillState::NameOnly);
        assert_eq!(get(dir.path(), "future"), SkillState::User);
    }

    #[test]
    fn set_round_trips_and_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        set(dir.path(), "a", SkillState::UserOnly).unwrap();
        set(dir.path(), "b", SkillState::Off).unwrap();
        set(dir.path(), "c", SkillState::User).unwrap();

        let states = load(dir.path());
        assert_eq!(states.len(), 3);
        assert_eq!(states.get("a"), Some(&SkillState::UserOnly));
        assert_eq!(states.get("b"), Some(&SkillState::Off));
        assert_eq!(states.get("c"), Some(&SkillState::User));

        let text = std::fs::read_to_string(
            crate::data_layout::DataLayout::from_root(dir.path()).config_file(FILE_NAME),
        )
        .unwrap();
        assert!(text.contains("\"c\": \"user\""), "{text}");
        assert!(!crate::data_layout::DataLayout::from_root(dir.path())
            .config_file(TEMP_NAME)
            .exists());
    }

    #[test]
    fn apply_maps_each_state() {
        let mut commands = ["a", "b", "c", "d"]
            .into_iter()
            .map(|name| (name.to_owned(), command(name)))
            .collect::<BTreeMap<_, _>>();
        let states = BTreeMap::from([
            ("a".to_owned(), SkillState::Off),
            ("b".to_owned(), SkillState::User),
            ("c".to_owned(), SkillState::NameOnly),
            ("d".to_owned(), SkillState::UserOnly),
        ]);

        apply(&mut commands, &states);

        assert!(!commands.contains_key("a"));
        assert_eq!(commands["b"].name, "b");
        assert!(!commands["b"].listing_name_only);
        assert!(commands["b"].model_invocable());
        assert!(commands["c"].listing_name_only);
        assert!(commands["c"].model_invocable());
        assert!(commands["c"].user_invocable);
        assert!(!commands["d"].model_invocable());
        assert!(commands["d"].user_invocable);
        assert!(!commands["d"].listing_name_only);
    }

    #[test]
    fn translation_skills_default_to_user_only() {
        assert_eq!(default_state("translate-zh-en"), SkillState::UserOnly);
        assert_eq!(default_state("translate-en-zh"), SkillState::UserOnly);
        assert_eq!(default_state("simplify"), SkillState::User);

        // No entry: the default hides the skill from the model but keeps the
        // user slash invocation.
        let mut commands = ["translate-en-zh"]
            .into_iter()
            .map(|name| (name.to_owned(), command(name)))
            .collect::<BTreeMap<_, _>>();
        apply(&mut commands, &BTreeMap::new());
        assert!(commands["translate-en-zh"].disable_model_invocation);
        assert!(commands["translate-en-zh"].user_invocable);

        // An explicit `user` state makes it listable and invocable by the model.
        let mut commands = ["translate-en-zh"]
            .into_iter()
            .map(|name| (name.to_owned(), command(name)))
            .collect::<BTreeMap<_, _>>();
        let states = BTreeMap::from([("translate-en-zh".to_owned(), SkillState::User)]);
        apply(&mut commands, &states);
        assert!(!commands["translate-en-zh"].disable_model_invocation);
        assert!(commands["translate-en-zh"].user_invocable);
    }

    #[test]
    fn labels_and_parsing_cover_all_five_states() {
        let expected = [
            (SkillState::Off, "off"),
            (SkillState::User, "user"),
            (SkillState::NameOnly, "name-only"),
            (SkillState::UserOnly, "user-only"),
            (SkillState::Always, "always"),
        ];
        for (state, label) in expected {
            assert_eq!(state.label(), label);
            assert_eq!(label.parse::<SkillState>().unwrap(), state);
        }
        assert_eq!(SkillState::default(), SkillState::User);
        let error = "marketplace".parse::<SkillState>().unwrap_err().to_string();
        for label in ["off", "user", "name-only", "user-only", "always"] {
            assert!(error.contains(label), "{error}");
        }
    }

    struct PathGuard(Option<std::ffi::OsString>);

    impl PathGuard {
        fn set(value: &Path) -> Self {
            let previous = std::env::var_os("PATH");
            std::env::set_var("PATH", value);
            Self(previous)
        }
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn bundled_defaults_enable_the_always_available_toolkit() {
        let _lock = PATH_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let empty = tempfile::tempdir().unwrap();
        let _path = PathGuard::set(empty.path());

        for name in [
            "skill-creator",
            "dc-docs",
            "llm-proxy-data-troubleshooting",
            "project-docs",
        ] {
            assert_eq!(bundled_default_state(name), SkillState::User, "{name}");
        }
        for name in ["codegraph", "docx", "task-template", "translate-zh-en"] {
            assert_eq!(bundled_default_state(name), SkillState::Off, "{name}");
        }
        // The same names outside the built-in tree keep the old defaults.
        assert_eq!(default_state("docx"), SkillState::User);
        assert_eq!(default_state("translate-zh-en"), SkillState::UserOnly);

        // A non-executable file on PATH is not an installed CodeGraph.
        let codegraph = empty
            .path()
            .join(if cfg!(windows) { "codegraph.cmd" } else { "codegraph" });
        std::fs::write(&codegraph, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&codegraph, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(bundled_default_state("codegraph"), SkillState::Off);
            std::fs::set_permissions(&codegraph, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(bundled_default_state("codegraph"), SkillState::Always);
    }

    #[test]
    fn always_state_round_trips_and_marks_command_for_inline_injection() {
        assert_eq!("always".parse::<SkillState>().unwrap(), SkillState::Always);
        assert_eq!(SkillState::Always.label(), "always");
        let err = "bogus".parse::<SkillState>().unwrap_err().to_string();
        assert!(err.contains("always"), "{err}");

        let dir = tempfile::tempdir().unwrap();
        set(dir.path(), "style-guide", SkillState::Always).unwrap();
        assert_eq!(load(dir.path()).get("style-guide"), Some(&SkillState::Always));

        let mut commands = BTreeMap::from([
            ("style-guide".to_owned(), command("style-guide")),
            ("normal".to_owned(), command("normal")),
        ]);
        apply(&mut commands, &load(dir.path()));
        assert!(commands["style-guide"].always_inline);
        assert!(commands["style-guide"].model_invocable());
        assert!(!commands["normal"].always_inline);
    }

    #[test]
    fn bundled_paths_use_builtin_defaults_and_stored_states_win() {
        let root = PathBuf::from("data").join("skills").join("bundled");
        let mut docx = command("docx");
        docx.path = root.join("docx/SKILL.md");
        let mut creator = command("skill-creator");
        creator.path = root.join("skill-creator/SKILL.md");
        let mut commands = BTreeMap::from([
            ("docx".to_owned(), docx),
            ("skill-creator".to_owned(), creator),
        ]);

        apply_in(&mut commands, &BTreeMap::new(), Some(&root));
        assert!(!commands.contains_key("docx"));
        assert!(commands.contains_key("skill-creator"));

        // A user skill with a built-in name is not forced off.
        let mut user_docx = command("docx");
        user_docx.path = PathBuf::from("data").join("skills").join("docx/SKILL.md");
        let mut commands = BTreeMap::from([("docx".to_owned(), user_docx)]);
        apply_in(&mut commands, &BTreeMap::new(), Some(&root));
        assert!(commands.contains_key("docx"));

        // A stored state always beats the built-in default.
        let mut docx = command("docx");
        docx.path = root.join("docx/SKILL.md");
        let mut creator = command("skill-creator");
        creator.path = root.join("skill-creator/SKILL.md");
        let mut commands = BTreeMap::from([
            ("docx".to_owned(), docx),
            ("skill-creator".to_owned(), creator),
        ]);
        let states = BTreeMap::from([
            ("docx".to_owned(), SkillState::User),
            ("skill-creator".to_owned(), SkillState::Off),
        ]);
        apply_in(&mut commands, &states, Some(&root));
        assert!(commands.contains_key("docx"));
        assert!(!commands.contains_key("skill-creator"));
    }
}
