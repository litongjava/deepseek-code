//! Off-by-default local coordination settings.
//!
//! Only the `localCoordination` section of the user data directory's
//! `config/settings.json` is read. Loading never writes, so unrelated settings
//! are preserved by never being touched.

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use std::path::Path;

const SETTINGS_FILE: &str = "settings.json";
const SETTINGS_SECTION: &str = "localCoordination";

/// Coordination opt-in. Missing settings and a missing section both disable
/// coordination; `registry` only matters once `enabled` is explicitly on.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct CoordinationSettings {
    pub enabled: bool,
    pub registry: bool,
}

impl Default for CoordinationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            registry: true,
        }
    }
}

/// Read the coordination section from `data_dir/config/settings.json`.
///
/// The file may start with a UTF-8 BOM and must contain an object. Only the
/// `localCoordination` section is parsed; unknown or mistyped fields inside it
/// are errors. A missing file or section yields the disabled default.
pub fn load_settings(data_dir: &Path) -> Result<CoordinationSettings> {
    let path =
        crate::data_layout::DataLayout::from_root(data_dir).config_file(SETTINGS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CoordinationSettings::default())
        }
        Err(error) => return Err(error).with_context(|| format!("Cannot read {}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .with_context(|| format!("Invalid {}", path.display()))?;
    ensure!(
        value.is_object(),
        "{} must contain an object",
        path.display()
    );
    match value.get(SETTINGS_SECTION) {
        Some(section) => serde_json::from_value(section.clone())
            .with_context(|| format!("Invalid {SETTINGS_SECTION} in {}", path.display())),
        None => Ok(CoordinationSettings::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_settings(dir: &Path, text: &str) -> Result<PathBuf> {
        let layout = crate::data_layout::DataLayout::from_root(dir);
        std::fs::create_dir_all(layout.config_root())?;
        let path = layout.config_file(SETTINGS_FILE);
        std::fs::write(&path, text)?;
        Ok(path)
    }

    #[test]
    fn coordination_settings_validate_section() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("data");

        // A UTF-8 BOM and unrelated top-level keys are accepted, and loading
        // never rewrites the file.
        let path = write_settings(
            &dir,
            "\u{feff}{\"other\":{\"keep\":true},\"localCoordination\":{\"enabled\":true,\"registry\":false}}",
        )?;
        let before = std::fs::read(&path)?;
        assert_eq!(
            load_settings(&dir)?,
            CoordinationSettings {
                enabled: true,
                registry: false
            }
        );
        assert_eq!(std::fs::read(&path)?, before);

        // A partial section keeps the documented defaults.
        write_settings(&dir, r#"{"localCoordination":{"registry":false}}"#)?;
        assert_eq!(
            load_settings(&dir)?,
            CoordinationSettings {
                enabled: false,
                registry: false
            }
        );

        // Malformed input and a failed load leave the file byte-identical.
        let malformed = write_settings(&dir, r#"{"localCoordination":"#)?;
        let before = std::fs::read(&malformed)?;
        assert!(load_settings(&dir).is_err());
        assert_eq!(std::fs::read(&malformed)?, before);

        for invalid in [
            "[]",
            "null",
            r#""settings""#,
            r#"{"localCoordination":true}"#,
            r#"{"localCoordination":{"enabled":"yes"}}"#,
            r#"{"localCoordination":{"registry":1}}"#,
            r#"{"localCoordination":{"enabled":true,"unknown":false}}"#,
        ] {
            write_settings(&dir, invalid)?;
            assert!(load_settings(&dir).is_err(), "accepted {invalid}");
        }

        // An unrelated-only object and a partial section keep documented defaults.
        write_settings(&dir, r#"{"other":{"keep":true}}"#)?;
        assert_eq!(
            load_settings(&dir)?,
            CoordinationSettings {
                enabled: false,
                registry: true
            }
        );
        write_settings(&dir, r#"{"localCoordination":{"enabled":true}}"#)?;
        assert_eq!(
            load_settings(&dir)?,
            CoordinationSettings {
                enabled: true,
                registry: true
            }
        );

        // Missing files create nothing and are disabled.
        let missing_dir = temp.path().join("missing");
        assert_eq!(
            load_settings(&missing_dir)?,
            CoordinationSettings {
                enabled: false,
                registry: true
            }
        );
        assert!(!missing_dir.exists());
        Ok(())
    }
}
