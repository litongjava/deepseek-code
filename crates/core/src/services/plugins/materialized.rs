use super::*;
use base64::Engine;
use std::path::PathBuf;

fn decoded_files(bundle: &Bundle) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = bundle
        .files
        .iter()
        .map(|(path, content)| (path.clone(), content.as_bytes().to_vec()))
        .collect::<BTreeMap<_, _>>();
    for (path, content) in &bundle.binary_files {
        ensure!(
            !files.contains_key(path),
            "Duplicate text/binary plugin file"
        );
        files.insert(
            path.clone(),
            base64::engine::general_purpose::STANDARD
                .decode(content)
                .context("Invalid binary plugin file")?,
        );
    }
    ensure!(
        files.values().map(Vec::len).sum::<usize>() <= 25_000_000,
        "Plugin content exceeds 25 MB"
    );
    Ok(files)
}

fn directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Plugin cache directory is not a real directory"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(path)?,
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(super) fn materialize(context: &ServiceContext, bundle: &Bundle) -> Result<PathBuf> {
    bundle.validate()?;
    let files = decoded_files(bundle)?;
    let payload = serde_json::to_vec(bundle)?;
    let digest = ring::digest::digest(&ring::digest::SHA256, &payload)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let mut base = context.paths.data_dir.clone();
    std::fs::create_dir_all(&base)?;
    for segment in ["plugins", "cache", &bundle.id, &bundle.version] {
        base.push(segment);
        directory(&base)?;
    }
    let target = base.join(digest);
    if !target.exists() {
        let staged = tempfile::Builder::new()
            .prefix("install-")
            .tempdir_in(&base)?;
        for (relative, bytes) in &files {
            let path = staged.path().join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            std::io::Write::write_all(&mut output, bytes)?;
            #[cfg(unix)]
            if bundle.executable_files.contains(relative) {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        match std::fs::rename(staged.path(), &target) {
            Ok(()) => {}
            Err(_) if target.exists() => {}
            Err(error) => return Err(error).context("Installing plugin cache"),
        }
    }
    directory(&target)?;
    for (relative, bytes) in files {
        let path = target.join(&relative);
        let mut current = target.clone();
        for part in Path::new(&relative).components() {
            current.push(part);
            ensure!(
                !std::fs::symlink_metadata(&current)?
                    .file_type()
                    .is_symlink(),
                "Plugin cache contains a symbolic link"
            );
        }
        ensure!(
            std::fs::read(path)? == bytes,
            "Installed plugin cache differs from its saved bundle; reinstall the plugin"
        );
    }
    Ok(target)
}

pub fn skill_commands(
    context: &ServiceContext,
) -> Result<BTreeMap<String, crate::skills::discovery::SkillCommand>> {
    let mut skills = BTreeMap::new();
    for installed in list(context)?.into_iter().filter(|p| p.enabled) {
        let bundle = enabled_bundle(context, &installed.id)?;
        if bundle.skill_paths.is_empty() && bundle.command_paths.is_empty() {
            continue;
        }
        let root = materialize(context, &bundle)?;
        skills.extend(crate::skills::discovery::discover_plugin(
            &root,
            &bundle.id,
            &bundle.skill_paths,
            &bundle.command_paths,
        )?);
    }
    Ok(skills)
}

pub fn command_hooks(context: &ServiceContext) -> Result<Vec<(PathBuf, Value)>> {
    let mut hooks = Vec::new();
    for installed in list(context)?.into_iter().filter(|p| p.enabled) {
        let bundle = enabled_bundle(context, &installed.id)?;
        if bundle.lifecycle_hooks.is_empty() {
            continue;
        }
        let root = materialize(context, &bundle)?;
        let mut config = serde_json::json!({"hooks":bundle.lifecycle_hooks});
        official::replace_plugin_root(&mut config, &root.to_string_lossy().replace('\\', "/"));
        hooks.push((root.join(".claude-plugin/plugin.json"), config));
    }
    Ok(hooks)
}

/// Synchronize only names reserved for plugin-owned stdio servers. Changes to
/// user-managed servers remain untouched. Call at startup and before each turn.
pub async fn sync_mcp(
    context: &ServiceContext,
    manager: &crate::mcp::McpServerManager,
) -> Result<()> {
    let mut desired = BTreeMap::new();
    for installed in list(context)?.into_iter().filter(|p| p.enabled) {
        let bundle = enabled_bundle(context, &installed.id)?;
        if bundle.mcp_servers.is_empty() {
            continue;
        }
        let root = materialize(context, &bundle)?;
        for (name, mut server) in bundle.mcp_servers {
            official::replace_plugin_root(&mut server, &root.to_string_lossy().replace('\\', "/"));
            let id = format!("plugin:{}:{name}", bundle.id);
            let mut config = crate::mcp::McpServerConfig {
                id: id.clone(),
                name: id.clone(),
                command: server["command"]
                    .as_str()
                    .context("MCP command missing")?
                    .into(),
                args: serde_json::from_value(
                    server
                        .get("args")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                )?,
                env: serde_json::from_value(
                    server
                        .get("env")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                )?,
                cwd: Some(root.clone()),
                enabled: true,
                ..Default::default()
            };
            config
                .env
                .entry("CLAUDE_PLUGIN_ROOT".into())
                .or_insert_with(|| root.to_string_lossy().into_owned());
            desired.insert(id, config);
        }
    }
    let current = manager.list_servers_full().await;
    for server in current
        .iter()
        .filter(|s| s.config.id.starts_with("plugin:"))
    {
        if !desired.contains_key(&server.config.id) {
            manager.remove_server(&server.config.id).await?;
        }
    }
    for (id, config) in desired {
        match current.iter().find(|s| s.config.id == id) {
            None => manager.add_server(config).await?,
            Some(server) if server.config != config => manager.update_server(&id, config).await?,
            Some(_) => {}
        }
        if !manager
            .get_server(&id)
            .await
            .is_some_and(|status| status.running)
        {
            manager
                .start_server(&id)
                .await
                .with_context(|| format!("Starting plugin MCP server {id}"))?;
        }
    }
    Ok(())
}
