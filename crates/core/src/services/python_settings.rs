//! The interpreter the Python tool uses when the model passes neither
//! `interpreter` nor `conda`, shared by the tool and the environment prompt.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct PythonSettings {
    /// An absolute path or a program name on PATH.
    pub path: Option<String>,
}

impl PythonSettings {
    fn validate(&self) -> Result<()> {
        if let Some(path) = &self.path {
            ensure!(!path.trim().is_empty(), "python.path must not be empty");
            ensure!(
                !path.contains(['\0', '\r', '\n']),
                "python.path contains control characters"
            );
        }
        Ok(())
    }
}

pub fn load(root: &Path) -> Result<PythonSettings> {
    let value = super::shell_settings::document(root)?;
    let settings: PythonSettings = match value.get("python") {
        Some(value) if !value.is_null() => {
            serde_json::from_value(value.clone()).context("Invalid python settings")?
        }
        _ => PythonSettings::default(),
    };
    settings.validate()?;
    Ok(settings)
}

pub fn save(root: &Path, settings: &PythonSettings) -> Result<()> {
    settings.validate()?;
    let mut value = super::shell_settings::document(root)?;
    value["python"] = serde_json::to_value(settings)?;
    super::shell_settings::save_document(root, &value)
}

pub fn set(root: &Path, field: &str, value: &str) -> Result<()> {
    let mut settings = load(root)?;
    match field {
        "path" => settings.path = (!value.trim().is_empty()).then(|| value.to_owned()),
        _ => anyhow::bail!("Unsupported python setting: {field}"),
    }
    save(root, &settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_path_is_optional_and_rejects_empty_values() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(load(root.path()).unwrap(), PythonSettings::default());

        let mut settings = PythonSettings {
            path: Some(" /usr/bin/python3 ".into()),
        };
        save(root.path(), &settings).unwrap();
        assert_eq!(load(root.path()).unwrap(), settings);

        settings.path = Some("  ".into());
        assert!(save(root.path(), &settings).is_err());
        settings.path = Some("bad\npath".into());
        assert!(save(root.path(), &settings).is_err());
    }

    #[test]
    fn invalid_settings_are_not_silently_ignored() {
        let root = tempfile::tempdir().unwrap();
        crate::data_layout::write_config_file(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"),
            r#"{"python":{"path":3}}"#,
        )
        .unwrap();
        assert!(load(root.path()).is_err());
    }
}
