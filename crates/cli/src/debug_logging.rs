use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, RwLock},
};
use tracing_subscriber::{prelude::*, reload, EnvFilter, Registry};

type FilterHandle = reload::Handle<EnvFilter, Registry>;
static FILTER: OnceLock<FilterHandle> = OnceLock::new();
static DESTINATION: RwLock<Option<PathBuf>> = RwLock::new(None);
static FILE: RwLock<Option<Arc<Mutex<std::fs::File>>>> = RwLock::new(None);

struct RedactingWriter {
    output: Box<dyn Write + Send>,
    bytes: Vec<u8>,
}
impl Write for RedactingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let available = 65536usize.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(available)]);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for RedactingWriter {
    fn drop(&mut self) {
        let text =
            deepseek_code_core::services::feedback::redact(&String::from_utf8_lossy(&self.bytes), &[]);
        let _ = self.output.write_all(text.as_bytes());
        let _ = self.output.flush();
    }
}
struct SharedFile(Arc<Mutex<std::fs::File>>);
impl Write for SharedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

pub fn install(verbose: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if verbose { "info" } else { "error" }));
    let (filter, handle) = reload::Layer::new(filter);
    let _ = FILTER.set(handle);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(|| -> Box<dyn Write + Send> {
                    let file = FILE.read().unwrap_or_else(|e| e.into_inner()).clone();
                    let output: Box<dyn Write + Send> = match file {
                        Some(file) => Box::new(SharedFile(file)),
                        None => Box::new(io::stderr()),
                    };
                    Box::new(RedactingWriter {
                        output,
                        bytes: Vec::new(),
                    })
                }),
        )
        .init();
}

pub fn configure(path: Option<PathBuf>) -> anyhow::Result<()> {
    let file = if let Some(path) = &path {
        let mut options = std::fs::OpenOptions::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.create(true).append(true).open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Some(Arc::new(Mutex::new(file)))
    } else {
        None
    };
    let enabled = path.is_some();
    let handle = FILTER
        .get()
        .ok_or_else(|| anyhow::anyhow!("Logging is not initialized"))?;
    *DESTINATION.write().unwrap_or_else(|e| e.into_inner()) = path;
    *FILE.write().unwrap_or_else(|e| e.into_inner()) = file;
    handle.reload(EnvFilter::new(if enabled {
        "error,deepseek_code_core=debug,claude_code=debug"
    } else {
        "error"
    }))?;
    Ok(())
}

pub fn destination() -> Option<PathBuf> {
    DESTINATION
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn category_filter(categories: &str) -> anyhow::Result<EnvFilter> {
    use anyhow::{bail, ensure};
    if categories.trim().is_empty() {
        return Ok(EnvFilter::new("error,deepseek_code_core=debug,claude_code=debug"));
    }
    let parts: Vec<_> = categories.split(',').map(str::trim).collect();
    let has_inclusions = parts.iter().any(|part| !part.starts_with('!'));
    let mut filter = if has_inclusions {
        "error".to_owned()
    } else {
        "error,deepseek_code_core=debug,claude_code=debug".to_owned()
    };
    for part in parts {
        let (enabled, name) = part
            .strip_prefix('!')
            .map(|name| (false, name))
            .unwrap_or((true, part));
        ensure!(!name.is_empty(), "Empty debug category");
        let target = match name {
            "api" => "deepseek_code_core::native_engine",
            "hooks" => "deepseek_code_core::services::tool_hooks",
            "mcp" => "deepseek_code_core::mcp",
            "lsp" => "deepseek_code_core::tools::lsp",
            "ide" => "deepseek_code_core::services::ide",
            "skills" => "deepseek_code_core::skills",
            "permissions" => "deepseek_code_core::permissions",
            "cli" => "claude_code",
            "1p" => "deepseek_code_core::native_engine::anthropic_client",
            "file" => "deepseek_code_core::tools",
            _ => bail!("Unknown debug category '{name}'; use api,hooks,mcp,lsp,ide,skills,permissions,cli,1p,file (prefix ! to exclude)"),
        };
        filter.push_str(&format!(
            ",{target}={}",
            if enabled { "debug" } else { "off" }
        ));
    }
    Ok(EnvFilter::try_new(filter)?)
}

pub fn configure_filtered(path: PathBuf, categories: &str) -> anyhow::Result<()> {
    // Parse before creating the log so invalid options do not alter the destination.
    let filter = category_filter(categories)?;
    configure(Some(path))?;
    FILTER
        .get()
        .ok_or_else(|| anyhow::anyhow!("Logging is not initialized"))?
        .reload(filter)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn debug_categories_preserve_exclusions_and_reject_misspellings() {
        let filter = category_filter("api,hooks,!1p").unwrap().to_string();
        assert!(filter.contains("deepseek_code_core::native_engine=debug"));
        assert!(filter.contains("deepseek_code_core::native_engine::anthropic_client=off"));
        assert!(category_filter("!file")
            .unwrap()
            .to_string()
            .contains("deepseek_code_core=debug"));
        assert!(category_filter("mistyped").is_err());
        assert!(category_filter("api,,hooks").is_err());
    }
    #[test]
    fn diagnostic_writer_redacts_credentials_across_write_fragments() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let output = SharedFile(Arc::new(Mutex::new(temp.reopen().unwrap())));
        let mut writer = RedactingWriter {
            output: Box::new(output),
            bytes: Vec::new(),
        };
        writer.write_all(b"request Authorization: Bear").unwrap();
        writer.write_all(b"er secretcredential123\n").unwrap();
        writer.write_all(b"api_key=anothersecret\n").unwrap();
        drop(writer);
        let text = std::fs::read_to_string(temp.path()).unwrap();
        assert!(text.contains("[REDACTED]"));
        assert!(!text.contains("secretcredential123"));
        assert!(!text.contains("anothersecret"));
    }
}
