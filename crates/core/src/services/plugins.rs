//! Versioned JSON bundles. Installation never executes plugin code.
use super::ServiceContext;
use anyhow::{ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Component, Path},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
mod legacy;
mod official;
mod materialized;
mod claude_code;
pub use materialized::{skill_commands, command_hooks, sync_mcp};
pub use legacy::import_legacy;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommand {
    pub description: String,
    pub interpreter: String,
    pub script: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Historical scripts receive string arguments as --key=value.
    #[serde(default)]
    pub legacy_arguments: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub id: String,
    pub version: String,
    pub description: String,
    /// Declarative system-prompt hook, loaded only while enabled.
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default, rename = "lspServers")]
    pub lsp_servers: BTreeMap<String, Value>,
    pub commands: BTreeMap<String, PluginCommand>,
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub binary_files: BTreeMap<String, String>,
    #[serde(default)]
    pub executable_files: Vec<String>,
    #[serde(default)]
    pub skill_paths: Vec<String>,
    #[serde(default)]
    pub command_paths: Vec<String>,
    #[serde(default)]
    pub lifecycle_hooks: BTreeMap<String, Value>,
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, Value>,
}
#[derive(Debug, Serialize)]
pub struct Installed {
    pub id: String,
    pub version: String,
    pub description: String,
    pub enabled: bool,
    pub source: String,
    pub commands: Vec<String>,
    pub capabilities: Vec<String>,
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':'])
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
impl Bundle {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.lsp_servers.len() <= 32, "Too many plugin language servers");
        for (name, config) in &self.lsp_servers {
            ensure!(valid_name(name) && config.is_object(), "Invalid language server configuration");
            crate::tools::validate_lsp_configuration(config)?;
        }
        ensure!(valid_name(&self.id), "Invalid plugin ID");
        semver::Version::parse(&self.version)?;
        ensure!(
            self.system_prompt.len() <= 16_000
                && self.files.len() + self.binary_files.len() <= 1000
                && self.commands.len() <= 30,
            "Plugin limits exceeded"
        );
        ensure!(
            self.files.keys().chain(self.binary_files.keys()).all(|name| safe_path(name)),
            "Plugin path escapes package"
        );
        ensure!(
            self.files.values().chain(self.binary_files.values()).map(String::len).sum::<usize>() <= 35_000_000,
            "Plugin files exceed package size limit"
        );
        ensure!(self.executable_files.iter().all(|path| safe_path(path) && (self.files.contains_key(path) || self.binary_files.contains_key(path))), "Invalid executable plugin path");
        ensure!(self.skill_paths.iter().chain(&self.command_paths).all(|path| safe_path(path)), "Plugin feature path escapes package");
        ensure!(self.mcp_servers.len() <= 32, "Too many plugin MCP servers");
        for (name, config) in &self.mcp_servers {
            ensure!(valid_name(name) && config.is_object() && config["command"].as_str().is_some_and(|v| !v.is_empty()), "Invalid stdio MCP server");
            ensure!(config.get("url").is_none() && config.get("type").and_then(Value::as_str).is_none_or(|v| v == "stdio"), "Only stdio plugin MCP servers are supported");
        }
        for (name, command) in &self.commands {
            ensure!(
                command.args.len() <= 64 && command.args.iter().all(|a| a.len() <= 4096),
                "Plugin argument limits exceeded"
            );
            ensure!(
                valid_name(name)
                    && matches!(command.interpreter.as_str(), "node" | "python" | "python3")
                    && self.files.contains_key(&command.script)
                    && safe_path(&command.script),
                "Invalid plugin command {name}"
            );
        }
        Ok(())
    }
}

/// Language servers from this application's plugins, then from plugins the
/// user installed in Claude Code (`~/.claude/plugins`). Earlier entries win.
pub fn all_language_servers(context: &ServiceContext) -> Result<Vec<Value>> {
    let mut servers = language_servers(context)?;
    if let Some(home) = dirs::home_dir() {
        servers.extend(claude_code::language_servers(&home.join(".claude")));
    }
    Ok(servers)
}

/// Read active plugin language servers without starting processes or exposing
/// credentials in the CLI's plugin list. The LSP manager starts them on demand.
pub fn language_servers(context: &ServiceContext) -> Result<Vec<Value>> {
    let mut servers = Vec::new();
    for item in list(context)?.into_iter().filter(|item| item.enabled) {
        let bundle = enabled_bundle(context, &item.id)?;
        let root = materialized::materialize(context, &bundle)?;
        for (name, mut config) in bundle.lsp_servers {
            official::replace_plugin_root(&mut config, &root.to_string_lossy().replace('\\', "/"));
            config["name"] = Value::String(format!("{}:{name}", item.id));
            servers.push(config);
        }
    }
    Ok(servers)
}
async fn source_bytes(source: &str) -> Result<Vec<u8>> {
    let bytes = if source.starts_with("https://") || source.starts_with("http://") {
        super::validate_endpoint(source)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut response = client.get(source).send().await?.error_for_status()?;
        ensure!(
            response.status().is_success(),
            "Unexpected plugin source status"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 6_000_000,
                "Plugin bundle exceeds 6 MB"
            );
            bytes.extend_from_slice(&chunk);
        }
        bytes
    } else {
        ensure!(
            std::fs::metadata(source)?.len() <= 6_000_000,
            "Plugin bundle exceeds 6 MB"
        );
        std::fs::read(source)?
    };
    Ok(bytes)
}
pub async fn source(source: &str) -> Result<Bundle> {
    if Path::new(source).is_dir() {
        return official::load(Path::new(source));
    }
    let bundle: Bundle = serde_json::from_slice(&source_bytes(source).await?)?;
    bundle.validate()?;
    Ok(bundle)
}
pub async fn install(
    context: &ServiceContext,
    from: &str,
    update: Option<&str>,
) -> Result<Installed> {
    let bundle = source(from).await?;
    materialized::materialize(context, &bundle)?;
    if let Some(id) = update {
        ensure!(
            bundle.id == id,
            "Update bundle ID differs from installed plugin"
        );
    }
    let payload = serde_json::to_string(&bundle)?;
    context.db.with_conn(|c| -> Result<()> {
        let tx = crate::db::write_transaction(&c)?;
        let current: Option<(String, bool)> = tx
            .query_row(
                "SELECT bundle,enabled FROM installed_plugins WHERE id=?1",
                [&bundle.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match (update, current) {
            (None, None) => {
                tx.execute(
                    "INSERT INTO installed_plugins(id,bundle,source,enabled) VALUES(?1,?2,?3,1)",
                    params![bundle.id, payload, from],
                )?;
            }
            (Some(_), Some((old, _))) => {
                let old: Bundle = serde_json::from_str(&old)?;
                ensure!(
                    semver::Version::parse(&bundle.version)?
                        > semver::Version::parse(&old.version)?,
                    "Update must have a newer version"
                );
                tx.execute(
                    "UPDATE installed_plugins SET bundle=?1,source=?2 WHERE id=?3",
                    params![payload, from, bundle.id],
                )?;
            }
            (None, Some(_)) => anyhow::bail!("Plugin already installed; use update"),
            (Some(_), None) => anyhow::bail!("Plugin not installed"),
        }
        tx.commit()?;
        Ok(())
    })??;
    list(context)?
        .into_iter()
        .find(|p| p.id == bundle.id)
        .context("Installed plugin missing")
}
pub fn list(context: &ServiceContext) -> Result<Vec<Installed>> {
    context.db.with_conn(|c| -> Result<_> {
        let mut stmt =
            c.prepare("SELECT bundle,enabled,source FROM installed_plugins ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|r| {
            let (raw, enabled, source) = r?;
            let b: Bundle = serde_json::from_str(&raw)?;
            let capabilities = [
                (!b.lsp_servers.is_empty(), "lsp"),
                (!b.skill_paths.is_empty(), "skills"),
                (!b.command_paths.is_empty(), "slash_commands"),
                (!b.lifecycle_hooks.is_empty(), "command_hooks"),
                (!b.mcp_servers.is_empty(), "stdio_mcp"),
                (!b.commands.is_empty(), "script_commands"),
            ].into_iter().filter_map(|(enabled, name)| enabled.then(|| name.to_string())).collect();
            Ok(Installed {
                id: b.id,
                version: b.version,
                description: b.description,
                enabled,
                source,
                commands: b.commands.keys().cloned().collect(),
                capabilities,
            })
        })
        .collect()
    })?
}
pub fn enable(context: &ServiceContext, id: &str, enabled: bool) -> Result<()> {
    ensure!(
        context.db.with_conn(|c| c.execute(
            "UPDATE installed_plugins SET enabled=?1 WHERE id=?2",
            params![enabled, id]
        ))?? == 1,
        "Plugin not installed"
    );
    Ok(())
}
pub fn remove(context: &ServiceContext, id: &str) -> Result<()> {
    ensure!(
        context
            .db
            .with_conn(|c| c.execute("DELETE FROM installed_plugins WHERE id=?1", [id]))??
            == 1,
        "Plugin not installed"
    );
    Ok(())
}
fn enabled_bundle(context: &ServiceContext, id: &str) -> Result<Bundle> {
    let raw: String = context
        .db
        .with_conn(|c| {
            c.query_row(
                "SELECT bundle FROM installed_plugins WHERE id=?1 AND enabled=1",
                [id],
                |r| r.get(0),
            )
            .optional()
        })??
        .context("Plugin is disabled or not installed")?;
    Ok(serde_json::from_str(&raw)?)
}
pub fn prompt_hooks(context: &ServiceContext) -> Result<String> {
    let mut text = String::new();
    for item in list(context)?.into_iter().filter(|p| p.enabled) {
        let bundle = enabled_bundle(context, &item.id)?;
        if !bundle.system_prompt.is_empty() {
            text.push_str(&format!(
                "\nEnabled plugin {}:\n{}\n",
                bundle.id, bundle.system_prompt
            ));
        }
    }
    Ok(text)
}

/// Callers must obtain the same execution permission as shell tools.
pub async fn run(
    context: &ServiceContext,
    id: &str,
    name: &str,
    input: Value,
    workspace: &Path,
) -> Result<Value> {
    let bundle = enabled_bundle(context, id)?;
    let command = bundle
        .commands
        .get(name)
        .context("Plugin command not registered")?;
    bundle.validate()?;
    let bytes = serde_json::to_vec(&input)?;
    ensure!(bytes.len() <= 1_048_576, "Plugin input exceeds 1 MiB");
    let mut legacy_args = Vec::new();
    if command.legacy_arguments {
        for (key, value) in input
            .as_object()
            .context("Legacy plugin input must be an object of strings")?
        {
            ensure!(valid_name(key), "Invalid legacy argument name");
            legacy_args.push(format!(
                "--{key}={}",
                value
                    .as_str()
                    .context("Legacy plugin argument values must be strings")?
            ));
        }
    }
    let directory = context.paths.data_dir.join("plugin-runs");
    std::fs::create_dir_all(&directory)?;
    let temporary = tempfile::tempdir_in(directory)?;
    for (path, content) in &bundle.files {
        let target = temporary.path().join(path);
        std::fs::create_dir_all(target.parent().unwrap())?;
        std::fs::write(target, content)?;
    }
    let mut process = tokio::process::Command::new(&command.interpreter);
    process
        .arg(temporary.path().join(&command.script))
        .args(&command.args)
        .args(legacy_args)
        .current_dir(std::fs::canonicalize(workspace)?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    process.creation_flags(0x08000000);
    let mut child = process.spawn().context("Plugin interpreter unavailable")?;
    let mut stdin = child.stdin.take().context("Plugin stdin unavailable")?;
    let stdout = child.stdout.take().context("Plugin stdout unavailable")?;
    let stderr = child.stderr.take().context("Plugin stderr unavailable")?;
    let bytes = if command.legacy_arguments {
        Vec::new()
    } else {
        bytes
    };
    let mut stdout = stdout.take(1_048_577);
    let mut stderr = stderr.take(1_048_577);
    let result = {
        let operation = async {
            let mut output = Vec::new();
            let mut errors = Vec::new();
            let input = async move {
                stdin.write_all(&bytes).await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, std::io::Error>(())
            };
            let (_, _, _, status) = tokio::try_join!(
                input,
                stdout.read_to_end(&mut output),
                stderr.read_to_end(&mut errors),
                child.wait()
            )?;
            ensure!(
                output.len() <= 1_048_576 && errors.len() <= 1_048_576,
                "Plugin output exceeds 1 MiB"
            );
            ensure!(
                status.success(),
                "Plugin failed: {}",
                String::from_utf8_lossy(&errors)
            );
            Ok::<_, anyhow::Error>(
                serde_json::json!({"stdout":String::from_utf8(output)?,"stderr":String::from_utf8(errors)?}),
            )
        };
        let operation = tokio::time::timeout(Duration::from_secs(60), operation);
        tokio::pin!(operation);
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {result=&mut operation=>break result.unwrap_or_else(|_|Err(anyhow::anyhow!("Plugin timed out"))),_=tick.tick()=>{if enabled_bundle(context,id).map(|b|b.version!=bundle.version).unwrap_or(true){break Err(anyhow::anyhow!("Plugin disabled, removed or updated during execution"));}}}
        }
    };
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub version: String,
    pub description: String,
    pub source: String,
}
/// Operator-provided catalog; no bundled marketplace or automatic installation.
pub async fn search(registry: &str, query: &str) -> Result<Vec<CatalogEntry>> {
    let entries: Vec<CatalogEntry> = serde_json::from_slice(&source_bytes(registry).await?)?;
    ensure!(entries.len() <= 1000, "Catalog exceeds 1000 entries");
    for entry in &entries {
        ensure!(valid_name(&entry.id), "Invalid catalog plugin ID");
        semver::Version::parse(&entry.version)?;
        super::validate_endpoint(&entry.source)?;
    }
    let query = query.to_lowercase();
    Ok(entries
        .into_iter()
        .filter(|e| {
            format!("{} {}", e.id, e.description)
                .to_lowercase()
                .contains(&query)
        })
        .collect())
}
