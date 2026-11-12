//! Per-skill states stored in `<data dir>/skill-states.json`.
//!
//! A skill without an entry gets its default state: [`SkillState::User`], except
//! the bundled translation skills, which default to [`SkillState::UserOnly`].
//! Existing installations therefore keep their previous behavior until the user
//! changes a state.
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
}

impl SkillState {
    pub fn label(self) -> &'static str {
        match self {
            SkillState::Off => "off",
            SkillState::User => "user",
            SkillState::NameOnly => "name-only",
            SkillState::UserOnly => "user-only",
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
            other => Err(anyhow::anyhow!(
                "Unknown skill state {other:?}; expected one of: off, user, name-only, user-only"
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
    let path = data_dir.join(FILE_NAME);
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

/// The state a skill has when the states file does not mention it.
pub fn default_state(name: &str) -> SkillState {
    if USER_ONLY_BY_DEFAULT.contains(&name) {
        SkillState::UserOnly
    } else {
        SkillState::User
    }
}

/// The stored state, or the skill's default.
pub fn effective(states: &BTreeMap<String, SkillState>, name: &str) -> SkillState {
    states
        .get(name)
        .copied()
        .unwrap_or_else(|| default_state(name))
}

pub fn get(data_dir: &Path, name: &str) -> SkillState {
    effective(&load(data_dir), name)
}

/// Store one skill's state, keeping the other entries. The file is replaced
/// atomically so a concurrent reader never sees a partial document.
pub fn set(data_dir: &Path, name: &str, state: SkillState) -> Result<()> {
    let mut states = load(data_dir);
    states.insert(name.to_owned(), state);
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("Creating {}", data_dir.display()))?;
    let file = SkillStatesFile {
        skills: states
            .into_iter()
            .map(|(name, state)| (name, state.label().to_owned()))
            .collect(),
    };
    let path = data_dir.join(FILE_NAME);
    let temp = data_dir.join(TEMP_NAME);
    let mut text = serde_json::to_string_pretty(&file)?;
    text.push('\n');
    std::fs::write(&temp, text).with_context(|| format!("Writing {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("Replacing {}", path.display()))?;
    Ok(())
}

/// Apply stored states to the merged command map. Names the file does not
/// mention still get their default, so skills that ship user-only stay hidden
/// from the model until the user changes them.
pub fn apply(commands: &mut BTreeMap<String, SkillCommand>, states: &BTreeMap<String, SkillState>) {
    let names: Vec<String> = commands.keys().cloned().collect();
    for name in names {
        match effective(states, &name) {
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
        }
    }
}

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
            context: None,
            shell: None,
            agent: None,
        }
    }

    #[test]
    fn missing_or_invalid_file_means_every_skill_is_user() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(get(dir.path(), "anything"), SkillState::User);

        std::fs::write(dir.path().join(FILE_NAME), "not json").unwrap();
        assert_eq!(get(dir.path(), "anything"), SkillState::User);

        std::fs::write(
            dir.path().join(FILE_NAME),
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

        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("\"c\": \"user\""), "{text}");
        assert!(!dir.path().join(TEMP_NAME).exists());
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
    fn labels_and_parsing_cover_exactly_the_four_states() {
        let expected = [
            (SkillState::Off, "off"),
            (SkillState::User, "user"),
            (SkillState::NameOnly, "name-only"),
            (SkillState::UserOnly, "user-only"),
        ];
        for (state, label) in expected {
            assert_eq!(state.label(), label);
            assert_eq!(label.parse::<SkillState>().unwrap(), state);
        }
        assert_eq!(SkillState::default(), SkillState::User);
        let error = "marketplace".parse::<SkillState>().unwrap_err().to_string();
        for label in ["off", "user", "name-only", "user-only"] {
            assert!(error.contains(label), "{error}");
        }
    }
}
