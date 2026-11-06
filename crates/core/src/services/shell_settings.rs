//! Shared shell selection for tool calls and interactive terminal entry points.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{io::Write, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ShellSettings {
    pub tool_default: String,
    pub terminal_default: String,
    pub bash_path: Option<String>,
    pub wsl: WslSettings,
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            tool_default: "bash".into(),
            terminal_default: "auto".into(),
            bash_path: None,
            wsl: WslSettings::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct WslSettings {
    pub distribution: Option<String>,
    pub bash_path: String,
}

impl Default for WslSettings {
    fn default() -> Self {
        Self {
            distribution: None,
            bash_path: "/bin/bash".into(),
        }
    }
}

fn document(root: &Path) -> Result<Value> {
    let path = root.join("settings.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(error) => return Err(error).with_context(|| format!("Cannot read {}", path.display())),
    };
    let value: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .with_context(|| format!("Invalid {}", path.display()))?;
    ensure!(value.is_object(), "settings.json must contain an object");
    Ok(value)
}

pub fn load(root: &Path) -> Result<ShellSettings> {
    let value = document(root)?;
    let settings: ShellSettings = match value.get("shell") {
        Some(value) => serde_json::from_value(value.clone()).context("Invalid shell settings")?,
        None => ShellSettings::default(),
    };
    settings.validate()?;
    Ok(settings)
}

pub fn save(root: &Path, settings: &ShellSettings) -> Result<()> {
    settings.validate()?;
    if let Some(path) = &settings.bash_path {
        validate_bash_path(path)?;
    }
    let mut value = document(root)?;
    value["shell"] = serde_json::to_value(settings)?;
    std::fs::create_dir_all(root)?;
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(&serde_json::to_vec_pretty(&value)?)?;
    file.as_file().sync_all()?;
    file.persist(root.join("settings.json"))?;
    Ok(())
}

pub fn set(root: &Path, field: &str, value: &str) -> Result<()> {
    let mut settings = load(root)?;
    match field {
        "toolDefault" => settings.tool_default = value.into(),
        "terminalDefault" => settings.terminal_default = value.into(),
        "bashPath" => settings.bash_path = (!value.is_empty()).then(|| value.into()),
        "wsl.distribution" => settings.wsl.distribution = (!value.is_empty()).then(|| value.into()),
        "wsl.bashPath" => settings.wsl.bash_path = value.into(),
        _ => bail!("Unsupported shell setting: {field}"),
    }
    save(root, &settings)
}

fn valid_shell(name: &str) -> bool {
    matches!(
        name,
        "bash" | "sh" | "zsh" | "fish" | "cmd" | "powershell" | "pwsh" | "wsl"
    )
}

fn validate_bash_path(path: &str) -> Result<()> {
    let file = Path::new(path);
    ensure!(
        file.is_absolute() && file.is_file(),
        "Bash path must be an existing absolute executable path: {path}"
    );
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    ensure!(
        !normalized.contains("/windowsapps/") && !normalized.ends_with("/system32/bash.exe"),
        "This is a WSL Bash alias; select shell wsl with an explicit distribution instead"
    );
    Ok(())
}

impl ShellSettings {
    fn validate(&self) -> Result<()> {
        ensure!(
            valid_shell(&self.tool_default),
            "Unsupported toolDefault shell: {}",
            self.tool_default
        );
        ensure!(
            self.terminal_default == "auto" || valid_shell(&self.terminal_default),
            "Unsupported terminalDefault shell: {}",
            self.terminal_default
        );
        ensure!(
            self.wsl.bash_path.starts_with('/') && !self.wsl.bash_path.contains(['\0', '\r', '\n']),
            "wsl.bashPath must be an absolute Linux path"
        );
        if let Some(distribution) = &self.wsl.distribution {
            ensure!(
                !distribution.trim().is_empty() && !distribution.contains(['\0', '\r', '\n']),
                "Invalid WSL distribution"
            );
        }
        Ok(())
    }

    fn bash(&self, environment: Option<&str>) -> Result<(String, &'static str)> {
        if let Some(path) = environment {
            validate_bash_path(path).context("Invalid DEEPSEEK_CODE_BASH_PATH")?;
            return Ok((path.into(), "environment"));
        }
        if let Some(path) = &self.bash_path {
            validate_bash_path(path).context("Invalid shell.bashPath")?;
            return Ok((path.clone(), "settings"));
        }
        Ok((
            crate::tools::find_shell("bash")
                .context("Bash not found; configure shell.bashPath or install Bash")?,
            "detected",
        ))
    }

    pub fn resolve(&self, requested: Option<&str>, terminal: bool) -> Result<ResolvedShell> {
        self.resolve_with_env(
            requested,
            terminal,
            std::env::var("DEEPSEEK_CODE_BASH_PATH").ok().as_deref(),
        )
    }

    fn resolve_with_env(
        &self,
        requested: Option<&str>,
        terminal: bool,
        environment: Option<&str>,
    ) -> Result<ResolvedShell> {
        self.validate()?;
        let selected = requested.unwrap_or(if terminal {
            &self.terminal_default
        } else {
            &self.tool_default
        });
        let selected = if selected == "auto" && terminal {
            if cfg!(windows) {
                "pwsh"
            } else {
                "bash"
            }
        } else {
            selected
        };
        ensure!(valid_shell(selected), "Unsupported shell: {selected}");
        let (program, source) = match selected {
            "bash" => self.bash(environment)?,
            "cmd" if cfg!(windows) => (
                std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
                "detected",
            ),
            "cmd" => bail!("cmd shell only available on Windows"),
            "wsl" => bail!("WSL execution requires an explicitly configured distribution"),
            name => (
                crate::tools::find_shell(name)
                    .with_context(|| format!("{name} shell not found"))?,
                "detected",
            ),
        };
        let name = if matches!(selected, "powershell" | "pwsh") {
            Path::new(&program)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or(selected)
                .to_ascii_lowercase()
        } else {
            selected.into()
        };
        let flag = match name.as_str() {
            "cmd" => "/C",
            "powershell" | "pwsh" => "-Command",
            _ => "-c",
        };
        Ok(ResolvedShell {
            name,
            program,
            command_flag: flag,
            source,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedShell {
    pub name: String,
    pub program: String,
    pub command_flag: &'static str,
    pub source: &'static str,
}

impl ResolvedShell {
    pub fn command(&self, text: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        match self.name.as_str() {
            "powershell" | "pwsh" => {
                command
                    .args(["-NoProfile", "-NonInteractive", "-Command"])
                    .arg(crate::tools::utf8::powershell_command(text));
            }
            #[cfg(windows)]
            "cmd" => {
                command
                    .args(["/D", "/S", "/C"])
                    .raw_arg(format!("\"{}\"", crate::tools::utf8::cmd_command(text)));
            }
            _ => {
                command.arg(self.command_flag).arg(text);
            }
        }
        command.kill_on_drop(true);
        command
    }
}

pub fn inspect(root: &Path) -> Result<Value> {
    let settings = load(root)?;
    let describe = |terminal| match settings.resolve(None, terminal) {
        Ok(resolved) => json!(resolved),
        Err(error) => json!({"error":format!("{error:#}")}),
    };
    Ok(json!({"settings":settings,"tool":describe(false),"terminal":describe(true)}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_preserves_other_fields_and_rejects_invalid_files() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(load(root.path()).unwrap(), ShellSettings::default());
        let file = root.path().join("settings.json");
        std::fs::write(
            &file,
            r#"{"hooks":{"sentinel":true},"sandbox":{"enabled":true}}"#,
        )
        .unwrap();
        set(root.path(), "toolDefault", "pwsh").unwrap();
        assert_eq!(load(root.path()).unwrap().tool_default, "pwsh");
        assert_eq!(document(root.path()).unwrap()["hooks"]["sentinel"], true);
        let original = std::fs::read(&file).unwrap();
        assert!(set(root.path(), "bashPath", "missing.exe").is_err());
        assert!(set(root.path(), "terminalDefault", "typo").is_err());
        assert!(set(root.path(), "unknown", "x").is_err());
        assert_eq!(std::fs::read(&file).unwrap(), original);
        std::fs::write(&file, "{broken").unwrap();
        assert!(set(root.path(), "toolDefault", "bash").is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{broken");
    }

    #[test]
    fn explicit_shell_and_environment_take_precedence_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let configured = root.path().join("configured bash.exe");
        let environment = root.path().join("environment bash.exe");
        std::fs::write(&configured, "").unwrap();
        std::fs::write(&environment, "").unwrap();
        let settings = ShellSettings {
            bash_path: Some(configured.to_string_lossy().into()),
            ..Default::default()
        };
        assert_eq!(
            settings
                .resolve_with_env(None, false, None)
                .unwrap()
                .program,
            configured.to_str().unwrap()
        );
        let resolved = settings
            .resolve_with_env(None, false, environment.to_str())
            .unwrap();
        assert_eq!(resolved.program, environment.to_str().unwrap());
        assert_eq!(resolved.source, "environment");
        assert!(settings
            .resolve_with_env(None, false, Some("missing.exe"))
            .is_err());
        if cfg!(windows) {
            assert_eq!(
                settings
                    .resolve_with_env(Some("cmd"), false, Some("missing.exe"))
                    .unwrap()
                    .name,
                "cmd"
            );
        }
        std::fs::remove_file(configured).unwrap();
        assert!(settings.resolve_with_env(None, false, None).is_err());
    }
}
