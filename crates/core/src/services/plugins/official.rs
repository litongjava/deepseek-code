//! Import official directory plugins without executing source files.
use super::*;

fn read_json(root: &Path, relative: &str, files: &mut BTreeMap<String, String>) -> Result<Value> {
    // A leading ./ is conventional in official plugin manifests.
    let relative = relative.strip_prefix("./").unwrap_or(relative);
    ensure!(
        safe_path(relative),
        "Plugin config path must stay inside its directory"
    );
    let path = root.join(relative).canonicalize()?;
    ensure!(
        path.starts_with(root),
        "Plugin config resolves outside its directory"
    );
    ensure!(
        std::fs::metadata(&path)?.len() <= 1_000_000,
        "Plugin config exceeds 1 MB"
    );
    let text = std::fs::read_to_string(path).context("Plugin config must be UTF-8")?;
    let value = serde_json::from_str(&text).context("Invalid plugin JSON")?;
    files.insert(relative.into(), text);
    Ok(value)
}

pub(super) fn merge(
    root: &Path,
    declaration: &Value,
    files: &mut BTreeMap<String, String>,
    servers: &mut BTreeMap<String, Value>,
    depth: usize,
) -> Result<()> {
    ensure!(depth <= 8, "LSP declaration nesting exceeds limit");
    match declaration {
        Value::String(path) => {
            let value = read_json(root, path, files)?;
            ensure!(
                value.is_object(),
                "Referenced LSP config must be a server map"
            );
            merge(root, &value, files, servers, depth + 1)?;
        }
        Value::Array(declarations) => {
            for declaration in declarations {
                merge(root, declaration, files, servers, depth + 1)?;
            }
        }
        Value::Object(map) => servers.extend(
            map.iter()
                .map(|(name, config)| (name.clone(), config.clone())),
        ),
        _ => anyhow::bail!("lspServers must be a server map, relative filename, or array"),
    }
    ensure!(servers.len() <= 32, "Too many plugin language servers");
    Ok(())
}

pub(super) fn replace_plugin_root(value: &mut Value, root: &str) {
    match value {
        Value::String(text) => *text = text.replace("${CLAUDE_PLUGIN_ROOT}", root),
        Value::Array(values) => {
            for value in values {
                replace_plugin_root(value, root);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                replace_plugin_root(value, root);
            }
        }
        _ => {}
    }
}

pub(super) fn load(source: &Path) -> Result<Bundle> {
    let root = source.canonicalize()?;
    ensure!(root.is_dir(), "Expected an official plugin directory");
    let mut files = BTreeMap::new();
    let manifest = read_json(&root, ".claude-plugin/plugin.json", &mut files)?;
    ensure!(
        manifest.get("agents").is_none() && !root.join("agents").exists(),
        "Official plugin agent definitions are not implemented by this importer"
    );
    let skill_paths = feature_paths(&root, &manifest, "skills")?;
    let command_paths = feature_paths(&root, &manifest, "commands")?;
    let mut servers = BTreeMap::new();
    if root.join(".lsp.json").exists() {
        let declaration = read_json(&root, ".lsp.json", &mut files)?;
        merge(&root, &declaration, &mut files, &mut servers, 0)?;
    }
    if let Some(declaration) = manifest.get("lspServers") {
        // Manifest declarations override the conventional .lsp.json, as upstream does.
        merge(&root, declaration, &mut files, &mut servers, 0)?;
    }
    let mut lifecycle_hooks = BTreeMap::new();
    if root.join("hooks/hooks.json").exists() {
        let hooks = read_json(&root, "hooks/hooks.json", &mut files)?;
        merge_hooks(&root, &hooks, &mut files, &mut lifecycle_hooks, 0)?;
    }
    if let Some(hooks) = manifest.get("hooks") {
        merge_hooks(&root, hooks, &mut files, &mut lifecycle_hooks, 0)?;
    }
    let mut mcp_servers = BTreeMap::new();
    if root.join(".mcp.json").exists() {
        let config = read_json(&root, ".mcp.json", &mut files)?;
        merge(
            &root,
            config.get("mcpServers").unwrap_or(&config),
            &mut files,
            &mut mcp_servers,
            0,
        )?;
    }
    if let Some(config) = manifest.get("mcpServers") {
        merge_mcp(&root, config, &mut files, &mut mcp_servers, 0)?;
    }
    let mut binary_files = BTreeMap::new();
    let mut executable_files = Vec::new();
    collect(&root, &mut files, &mut binary_files, &mut executable_files)?;
    let bundle = Bundle {
        id: manifest["name"]
            .as_str()
            .context("Plugin name is required")?
            .into(),
        version: manifest["version"].as_str().unwrap_or("0.0.0").into(),
        description: manifest["description"].as_str().unwrap_or_default().into(),
        system_prompt: String::new(),
        lsp_servers: servers,
        commands: BTreeMap::new(),
        files,
        binary_files,
        executable_files,
        skill_paths,
        command_paths,
        lifecycle_hooks,
        mcp_servers,
    };
    bundle.validate()?;
    Ok(bundle)
}

fn merge_mcp(
    root: &Path,
    declaration: &Value,
    files: &mut BTreeMap<String, String>,
    servers: &mut BTreeMap<String, Value>,
    depth: usize,
) -> Result<()> {
    ensure!(depth <= 8, "MCP declaration nesting exceeds limit");
    match declaration {
        Value::String(path) => {
            let value = read_json(root, path, files)?;
            merge_mcp(root, &value, files, servers, depth + 1)?;
        }
        Value::Array(values) => {
            for value in values {
                merge_mcp(root, value, files, servers, depth + 1)?;
            }
        }
        Value::Object(map) if map.contains_key("mcpServers") => {
            merge_mcp(root, &map["mcpServers"], files, servers, depth + 1)?
        }
        Value::Object(map) => servers.extend(
            map.iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        ),
        _ => anyhow::bail!("Invalid plugin MCP configuration"),
    }
    Ok(())
}

fn feature_paths(root: &Path, manifest: &Value, feature: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    if root.join(feature).is_dir() {
        paths.push(feature.into());
    }
    if let Some(value) = manifest.get(feature) {
        let values = match value {
            Value::String(_) => vec![value],
            Value::Array(values) => values.iter().collect(),
            _ => anyhow::bail!("Plugin {feature} must be a relative path or path array"),
        };
        for value in values {
            let path = value
                .as_str()
                .context("Plugin feature path must be a string")?
                .strip_prefix("./")
                .unwrap_or(value.as_str().unwrap());
            ensure!(
                safe_path(path),
                "Plugin feature path must stay inside its directory"
            );
            ensure!(
                root.join(path).canonicalize()?.starts_with(root),
                "Plugin feature resolves outside its directory"
            );
            if !paths.iter().any(|item| item == path) {
                paths.push(path.into());
            }
        }
    }
    Ok(paths)
}

fn merge_hooks(
    root: &Path,
    declaration: &Value,
    files: &mut BTreeMap<String, String>,
    hooks: &mut BTreeMap<String, Value>,
    depth: usize,
) -> Result<()> {
    ensure!(depth <= 8, "Hook declaration nesting exceeds limit");
    match declaration {
        Value::String(path) => {
            let value = read_json(root, path, files)?;
            merge_hooks(root, &value, files, hooks, depth + 1)?;
        }
        Value::Array(values) => {
            for value in values {
                merge_hooks(root, value, files, hooks, depth + 1)?;
            }
        }
        Value::Object(map) if map.contains_key("hooks") => {
            merge_hooks(root, &map["hooks"], files, hooks, depth + 1)?
        }
        Value::Object(map) => {
            for (event, rules) in map {
                ensure!(
                    ["PreToolUse", "PostToolUse", "UserPromptSubmit", "Stop"]
                        .contains(&event.as_str()),
                    "Plugin hook event {event} is not implemented"
                );
                let rules = rules
                    .as_array()
                    .context("Hook event must contain a matcher array")?;
                for rule in rules {
                    for hook in rule["hooks"]
                        .as_array()
                        .context("Hook matcher must contain hooks")?
                    {
                        ensure!(
                            hook["type"] == "command"
                                && hook.get("async").and_then(Value::as_bool) != Some(true),
                            "Only synchronous command plugin hooks are supported"
                        );
                    }
                }
                hooks
                    .entry(event.clone())
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .unwrap()
                    .extend(rules.clone());
            }
        }
        _ => anyhow::bail!("Invalid plugin hook declaration"),
    }
    Ok(())
}

fn collect(
    root: &Path,
    text: &mut BTreeMap<String, String>,
    binary: &mut BTreeMap<String, String>,
    executable: &mut Vec<String>,
) -> Result<()> {
    use base64::Engine;
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(17)
    {
        let entry = entry?;
        ensure!(
            !entry.file_type().is_symlink(),
            "Plugin symbolic links are not supported"
        );
        ensure!(
            entry.path().canonicalize()?.starts_with(root),
            "Plugin source escapes its directory"
        );
        if entry.file_type().is_dir() {
            ensure!(
                entry.depth() <= 16,
                "Plugin directory nesting exceeds limit"
            );
            continue;
        }
        ensure!(
            entry.file_type().is_file(),
            "Plugin contains a special file"
        );
        total += entry.metadata()?.len();
        ensure!(
            total <= 25_000_000 && text.len() + binary.len() < 1000,
            "Plugin exceeds 25 MB or 1000-file limit"
        );
        let relative = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .context("Plugin path is not UTF-8")?
            .replace('\\', "/");
        ensure!(safe_path(&relative), "Plugin source path is invalid");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if entry.metadata()?.permissions().mode() & 0o111 != 0 {
                executable.push(relative.clone());
            }
        }
        #[cfg(windows)]
        if entry.path().extension().is_some_and(|value| value == "sh") {
            executable.push(relative.clone());
        }
        match String::from_utf8(std::fs::read(entry.path())?) {
            Ok(value) => {
                text.insert(relative, value);
            }
            Err(error) => {
                binary.insert(
                    relative,
                    base64::engine::general_purpose::STANDARD.encode(error.into_bytes()),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(root: &Path, value: Value) {
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(root.join(".claude-plugin/plugin.json"), value.to_string()).unwrap();
    }

    #[test]
    fn imports_conventional_and_manifest_servers_with_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let old = json!({"command":"old","extensionToLanguage":{".ts":"typescript"}});
        let new = json!({"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/server.js"],"extensionToLanguage":{".ts":"typescript"}});
        std::fs::write(
            dir.path().join(".lsp.json"),
            json!({"typescript":old}).to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("extra.json"),
            json!({"typescript":new}).to_string(),
        )
        .unwrap();
        manifest(
            dir.path(),
            json!({"name":"test-lsp","version":"1.0.0","lspServers":["./extra.json"]}),
        );
        let bundle = load(dir.path()).unwrap();
        assert_eq!(bundle.lsp_servers["typescript"]["command"], "node");
        assert!(bundle.lsp_servers["typescript"]["args"][0]
            .as_str()
            .unwrap()
            .contains("${CLAUDE_PLUGIN_ROOT}"));
        assert!(bundle.files.contains_key(".claude-plugin/plugin.json"));
    }

    #[test]
    fn rejects_path_escape_and_unimplemented_plugin_features() {
        let dir = tempfile::tempdir().unwrap();
        manifest(
            dir.path(),
            json!({"name":"test-lsp","lspServers":"../outside.json"}),
        );
        assert!(load(dir.path()).unwrap_err().to_string().contains("inside"));
        manifest(dir.path(), json!({"name":"test-lsp","agents":"./agents"}));
        assert!(load(dir.path())
            .unwrap_err()
            .to_string()
            .contains("agent definitions"));
    }

    #[tokio::test]
    async fn installed_mixed_plugin_survives_source_removal_and_disable() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir_all(source.join("skills/check")).unwrap();
        std::fs::create_dir_all(source.join("commands")).unwrap();
        std::fs::create_dir_all(source.join("scripts")).unwrap();
        manifest(
            &source,
            json!({"name":"mixed","version":"1.0.0","hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"exit 2"}]}]},"lspServers":{"test":{"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/scripts/server.js"],"extensionToLanguage":{".ts":"typescript"}}}}),
        );
        std::fs::write(source.join("skills/check/SKILL.md"), "---\ndescription: Check files\n---\nUse ${CLAUDE_PLUGIN_ROOT}/scripts/server.js with $ARGUMENTS").unwrap();
        std::fs::write(source.join("commands/review.md"), "Review $ARGUMENTS").unwrap();
        std::fs::write(source.join("scripts/server.js"), "// fixture").unwrap();
        std::fs::write(source.join("asset.bin"), [0xff, 0, 0x80]).unwrap();
        let context = ServiceContext::open(
            crate::runtime::RuntimePaths::new(dir.path().join("data")).unwrap(),
            std::sync::Arc::new(crate::services::EnvironmentCredentials),
        )
        .unwrap();
        install(&context, source.to_str().unwrap(), None)
            .await
            .unwrap();
        std::fs::rename(&source, dir.path().join("moved-source")).unwrap();
        let commands = skill_commands(&context).unwrap();
        assert!(commands.contains_key("mixed:check") && commands.contains_key("mixed:review"));
        let skill = &commands["mixed:check"];
        assert!(skill
            .path
            .starts_with(context.paths.data_dir.join("plugins/cache")));
        assert!(!skill.content.contains("${CLAUDE_PLUGIN_ROOT}"));
        assert!(skill.render("target", "id").unwrap().contains("target"));
        let servers = language_servers(&context).unwrap();
        let script = servers[0]["args"][0].as_str().unwrap();
        assert!(Path::new(script).is_file());
        let cache =
            materialized::materialize(&context, &enabled_bundle(&context, "mixed").unwrap())
                .unwrap();
        assert_eq!(
            std::fs::read(cache.join("asset.bin")).unwrap(),
            [0xff, 0, 0x80]
        );
        let runner =
            crate::services::tool_hooks::HookRunner::load_with_plugins(&context, dir.path())
                .unwrap();
        assert!(
            runner
                .run(
                    crate::services::tool_hooks::HookEvent::PreToolUse,
                    "fixture",
                    json!({"tool_name":"Write"})
                )
                .await
                .unwrap()
                .blocked
        );
        enable(&context, "mixed", false).unwrap();
        assert!(skill_commands(&context).unwrap().is_empty());
        assert!(command_hooks(&context).unwrap().is_empty());
        assert!(language_servers(&context).unwrap().is_empty());
    }

    #[test]
    fn unsupported_mcp_transport_and_symbolic_links_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        manifest(
            dir.path(),
            json!({"name":"mixed","mcpServers":{"remote":{"type":"http","url":"https://example.com/mcp"}}}),
        );
        assert!(load(dir.path()).is_err());
        manifest(dir.path(), json!({"name":"mixed"}));
        std::fs::write(dir.path().join("target.txt"), "target").unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(
            dir.path().join("target.txt"),
            dir.path().join("linked.txt"),
        );
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(
            dir.path().join("target.txt"),
            dir.path().join("linked.txt"),
        );
        if linked.is_ok() {
            assert!(load(dir.path())
                .unwrap_err()
                .to_string()
                .contains("symbolic"));
        }
    }
}
