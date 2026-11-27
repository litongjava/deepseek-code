//! Opt-out shared by CLI, SDK, and Desktop model requests.
use std::path::Path;

pub const KEY: &str = "summarize_long_command_output";
const ENV: &str = "DEEPSEEK_CODE_SUMMARIZE_LONG_COMMAND_OUTPUT";
const ORCHESTRATION: [&str; 2] = [
    "config/orchestration.toml",
    "../config/orchestration.toml",
];

pub fn enabled(data_dir: Option<&Path>, workspace: &Path) -> bool {
    let cwd = std::env::current_dir().ok();
    resolve(
        data_dir,
        workspace,
        cwd.as_deref(),
        std::env::var(ENV).ok().as_deref(),
    ) && super::OrchestrationConfig::get()
        .tools
        .summarize_long_command_output
}

fn resolve(
    data_dir: Option<&Path>,
    workspace: &Path,
    cwd: Option<&Path>,
    env: Option<&str>,
) -> bool {
    if env.is_some_and(|value| value.trim().eq_ignore_ascii_case("false")) {
        return false;
    }
    let json = [
        workspace.join(".claude/settings.json"),
        workspace.join(".claude/settings.local.json"),
    ]
    .into_iter()
    .chain(data_dir.into_iter().flat_map(|dir| {
        let layout = crate::data_layout::DataLayout::from_root(&dir);
        ["settings.json", "config.json", "customization.json"].map(|file| layout.config_file(file))
    }));
    if json
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|text| {
            serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')).ok()
        })
        .any(|doc| doc[KEY] == false || doc["tools"][KEY] == false)
    {
        return false;
    }

    let toml = std::iter::once(workspace)
        .chain(cwd.filter(|cwd| *cwd != workspace))
        .flat_map(|dir| ORCHESTRATION.map(|file| dir.join(file)))
        .chain(data_dir.map(|dir| dir.join(ORCHESTRATION[0])));
    !toml
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|text| toml::from_str::<toml::Value>(text.trim_start_matches('\u{feff}')).ok())
        .any(|doc| {
            doc.get(KEY).and_then(toml::Value::as_bool) == Some(false)
                || doc
                    .get("tools")
                    .and_then(|tools| tools.get(KEY))
                    .and_then(toml::Value::as_bool)
                    == Some(false)
        })
}

/// Persist the user default in the selected application's settings directory.
pub fn set(data_dir: &Path, enabled: bool) -> anyhow::Result<()> {
    use crate::services::shell_settings::{document, save_document};
    let mut doc = document(data_dir)?;
    doc[KEY] = serde_json::Value::Bool(enabled);
    if let Some(tools) = doc
        .get_mut("tools")
        .and_then(serde_json::Value::as_object_mut)
    {
        tools.remove(KEY);
    }
    save_document(data_dir, &doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_output_opt_out_resolves_each_source_independently() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let workspace = temp.path().join("workspace");
        let cwd = temp.path().join("installation");
        let enabled = |env| resolve(Some(&data), &workspace, Some(&cwd), env);
        assert!(enabled(None));
        assert!(!enabled(Some("false")));
        for path in [
            crate::data_layout::DataLayout::from_root(&data).config_file("settings.json"),
            crate::data_layout::DataLayout::from_root(&data).config_file("config.json"),
            crate::data_layout::DataLayout::from_root(&data).config_file("customization.json"),
            workspace.join(".claude/settings.json"),
            workspace.join(".claude/settings.local.json"),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            for document in [
                serde_json::json!({"providers":[],"summarize_long_command_output":false}),
                serde_json::json!({"providers":[],"tools":{"summarize_long_command_output":false}}),
            ] {
                std::fs::write(&path, document.to_string()).unwrap();
                assert!(!enabled(Some("true")), "{}", path.display());
            }
            std::fs::remove_file(&path).unwrap();
            assert!(enabled(None));
        }

        set(&data, true).unwrap();
        for path in [&cwd, &workspace]
            .into_iter()
            .flat_map(|root| ORCHESTRATION.map(|file| root.join(file)))
            .chain([data.join("config/orchestration.toml")])
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "[tools]\nsummarize_long_command_output = false").unwrap();
            assert!(!enabled(None), "{}", path.display());
            std::fs::remove_file(&path).unwrap();
            assert!(enabled(None));
        }

        crate::data_layout::write_config_file(crate::data_layout::DataLayout::from_root(&data).config_file("settings.json"), r#"{"shell":{"toolDefault":"bash"},"tools":{"summarize_long_command_output":false,"other":true}}"#).unwrap();
        set(&data, false).unwrap();
        assert!(!enabled(None));
        set(&data, true).unwrap();
        assert!(enabled(None));
        assert!(!enabled(Some("false")));
        let doc = crate::services::shell_settings::document(&data).unwrap();
        assert_eq!(doc["shell"]["toolDefault"], "bash");
        assert_eq!(doc["tools"]["other"], true);
    }
}
