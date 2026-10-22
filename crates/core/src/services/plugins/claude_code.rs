//! Language servers from plugins installed by Claude Code itself
//! (`~/.claude/plugins`), so official LSP plugins such as
//! `rust-analyzer-lsp@claude-plugins-official` work without reinstalling.
use super::*;
use std::path::PathBuf;

/// Server configs declared by the user's enabled Claude Code plugins. Nothing
/// is started here; unreadable or invalid plugins are skipped.
pub(super) fn language_servers(claude_home: &Path) -> Vec<Value> {
    let plugins = claude_home.join("plugins");
    let Some(installed) = read(&plugins.join("installed_plugins.json")) else {
        return Vec::new();
    };
    let disabled = read(&claude_home.join("settings.json"))
        .and_then(|settings| settings.get("enabledPlugins").cloned())
        .unwrap_or_default();
    let mut servers = Vec::new();
    for (id, installs) in installed["plugins"].as_object().into_iter().flatten() {
        if disabled[id] == Value::Bool(false) {
            continue;
        }
        let Some(root) = installs
            .as_array()
            .and_then(|installs| installs.last())
            .and_then(|install| install["installPath"].as_str())
            .map(PathBuf::from)
        else {
            continue;
        };
        let (name, marketplace) = id.split_once('@').unwrap_or((id.as_str(), ""));
        let declared = declared_servers(&root, &plugins.join("marketplaces").join(marketplace), name);
        for (server, mut config) in declared {
            official::replace_plugin_root(&mut config, &root.to_string_lossy().replace('\\', "/"));
            config["name"] = Value::String(format!("claude-code:{id}:{server}"));
            if crate::tools::lsp_config_is_valid(&config) {
                servers.push(config);
            }
        }
    }
    servers
}

fn read(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The plugin's own `.lsp.json` and manifest, else its marketplace entry.
fn declared_servers(root: &Path, marketplace: &Path, name: &str) -> BTreeMap<String, Value> {
    let mut servers = BTreeMap::new();
    let mut files = BTreeMap::new();
    if let Ok(root) = root.canonicalize() {
        if root.join(".lsp.json").is_file() {
            if let Some(declaration) = read(&root.join(".lsp.json")) {
                let _ = official::merge(&root, &declaration, &mut files, &mut servers, 0);
            }
        }
        if let Some(declaration) = read(&root.join(".claude-plugin/plugin.json")).and_then(|manifest| manifest.get("lspServers").cloned()) {
            let _ = official::merge(&root, &declaration, &mut files, &mut servers, 0);
        }
    }
    if servers.is_empty() {
        // Official LSP plugins declare their servers inline in the marketplace.
        let entry = read(&marketplace.join(".claude-plugin/marketplace.json")).and_then(|catalog| {
            catalog["plugins"].as_array()?.iter().find(|plugin| plugin["name"] == name).cloned()
        });
        if let Some(Value::Object(inline)) = entry.as_ref().and_then(|entry| entry.get("lspServers")) {
            servers.extend(inline.iter().map(|(name, config)| (name.clone(), config.clone())));
        }
    }
    servers
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn installed_official_lsp_plugins_supply_their_servers() {
        let home = tempfile::tempdir().unwrap();
        let plugins = home.path().join("plugins");
        let install = plugins.join("cache/claude-plugins-official/rust-analyzer-lsp/1.0.0");
        std::fs::create_dir_all(&install).unwrap();
        let catalog = plugins.join("marketplaces/claude-plugins-official/.claude-plugin");
        std::fs::create_dir_all(&catalog).unwrap();
        std::fs::write(catalog.join("marketplace.json"), json!({"plugins":[
            {"name":"rust-analyzer-lsp","lspServers":{"rust-analyzer":{"command":"rust-analyzer","extensionToLanguage":{".rs":"rust"}}}},
            {"name":"gopls-lsp","lspServers":{"gopls":{"command":"gopls","extensionToLanguage":{".go":"go"}}}}
        ]}).to_string()).unwrap();
        let disabled = plugins.join("cache/claude-plugins-official/gopls-lsp/1.0.0");
        std::fs::create_dir_all(&disabled).unwrap();
        std::fs::write(plugins.join("installed_plugins.json"), json!({"version":2,"plugins":{
            "rust-analyzer-lsp@claude-plugins-official":[{"installPath":install}],
            "gopls-lsp@claude-plugins-official":[{"installPath":disabled}]
        }}).to_string()).unwrap();
        std::fs::write(home.path().join("settings.json"), json!({"enabledPlugins":{"gopls-lsp@claude-plugins-official":false}}).to_string()).unwrap();
        let servers = language_servers(home.path());
        assert_eq!(servers.len(), 1, "{servers:?}");
        assert_eq!(servers[0]["command"], "rust-analyzer");
        assert_eq!(servers[0]["extensionToLanguage"][".rs"], "rust");
        assert_eq!(servers[0]["name"], "claude-code:rust-analyzer-lsp@claude-plugins-official:rust-analyzer");
        assert!(language_servers(&home.path().join("missing")).is_empty());
    }
}
