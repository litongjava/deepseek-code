//! Known language servers: the default file mapping, discovery on PATH or in
//! the managed install directory, install hints, and opt-in managed installs.
//! Nothing is ever installed without an explicit request.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub struct Language {
    /// Name used by `/lsp install ID`.
    pub id: &'static str,
    pub name: &'static str,
    /// Extensions with their LSP language identifiers.
    pub extensions: &'static [(&'static str, &'static str)],
    /// Executable looked up on PATH.
    pub command: &'static str,
    pub args: &'static [&'static str],
}

pub const LANGUAGES: &[Language] = &[
    Language { id: "rust", name: "Rust", extensions: &[("rs", "rust")], command: "rust-analyzer", args: &[] },
    Language { id: "python", name: "Python", extensions: &[("py", "python")], command: "pylsp", args: &[] },
    Language {
        id: "typescript",
        name: "TypeScript/JavaScript",
        extensions: &[
            ("ts", "typescript"), ("mts", "typescript"), ("cts", "typescript"), ("tsx", "typescriptreact"),
            ("js", "javascript"), ("mjs", "javascript"), ("cjs", "javascript"), ("jsx", "javascriptreact"),
        ],
        command: "typescript-language-server",
        args: &["--stdio"],
    },
    Language { id: "html", name: "HTML", extensions: &[("html", "html"), ("htm", "html")], command: "vscode-html-language-server", args: &["--stdio"] },
    Language {
        id: "css",
        name: "CSS/SCSS/Less",
        extensions: &[("css", "css"), ("scss", "scss"), ("less", "less")],
        command: "vscode-css-language-server",
        args: &["--stdio"],
    },
    Language { id: "vue", name: "Vue", extensions: &[("vue", "vue")], command: "vue-language-server", args: &["--stdio"] },
    Language { id: "go", name: "Go", extensions: &[("go", "go")], command: "gopls", args: &[] },
    Language {
        id: "cpp",
        name: "C/C++",
        extensions: &[("c", "c"), ("h", "c"), ("cpp", "cpp"), ("cc", "cpp"), ("cxx", "cpp"), ("hpp", "cpp"), ("hh", "cpp")],
        command: "clangd",
        args: &[],
    },
    Language { id: "java", name: "Java", extensions: &[("java", "java")], command: "jdtls", args: &[] },
    Language { id: "csharp", name: "C#", extensions: &[("cs", "csharp")], command: "csharp-ls", args: &[] },
];

/// Red Hat's Java extension on Open VSX ships jdtls with a matching Java
/// runtime per platform, so no particular local JDK version is required.
const JAVA_PACKAGE_API: &str = "https://open-vsx.org/api/redhat/java";

pub fn by_id(id: &str) -> Option<&'static Language> {
    let id = id.to_ascii_lowercase();
    let id = match id.as_str() {
        "py" => "python",
        "scss" | "less" => "css",
        "htm" => "html",
        "ts" | "js" | "javascript" | "node" => "typescript",
        "c" | "c++" | "clangd" => "cpp",
        "golang" => "go",
        "cs" | "c#" | "dotnet" => "csharp",
        other => other,
    };
    LANGUAGES.iter().find(|language| language.id == id)
}

/// The language and its LSP identifier for a file extension.
pub fn for_extension(extension: &str) -> Option<(&'static Language, &'static str)> {
    let extension = extension.to_ascii_lowercase();
    LANGUAGES.iter().find_map(|language| {
        language
            .extensions
            .iter()
            .find(|(candidate, _)| *candidate == extension)
            .map(|(_, language_id)| (language, *language_id))
    })
}

/// `<data dir>/lsp`, where managed installs live.
pub fn install_dir() -> Result<PathBuf> {
    Ok(crate::runtime::resolve_default_data_dir()?.join("lsp"))
}

/// An executable on PATH, including Windows command shims.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let candidates: Vec<String> = if cfg!(windows) && Path::new(name).extension().is_none() {
        ["exe", "cmd", "bat"].iter().map(|ext| format!("{name}.{ext}")).collect()
    } else {
        vec![name.to_owned()]
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        candidates.iter().map(|candidate| dir.join(candidate)).find(|path| path.is_file())
    })
}

/// How to start a server: an executable plus arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub command: PathBuf,
    pub args: Vec<String>,
}

fn managed_executable(root: &Path, language: &Language) -> Option<PathBuf> {
    let bin = |dir: PathBuf, name: &str| -> Option<PathBuf> {
        let names: Vec<String> = if cfg!(windows) {
            vec![format!("{name}.exe"), format!("{name}.cmd")]
        } else {
            vec![name.to_owned()]
        };
        names.into_iter().map(|name| dir.join(name)).find(|path| path.is_file())
    };
    match language.id {
        "python" => bin(root.join("python").join(if cfg!(windows) { "Scripts" } else { "bin" }), "pylsp"),
        "typescript" | "html" | "css" | "vue" => bin(root.join("node").join("node_modules").join(".bin"), language.command),
        _ => bin(root.join("bin"), language.command),
    }
}

fn jdtls_launch(root: &Path, workspace: &Path) -> Option<Launch> {
    let home = root.join("jdtls");
    let launcher = std::fs::read_dir(home.join("plugins")).ok()?.flatten().map(|entry| entry.path()).find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("org.eclipse.equinox.launcher_") && name.ends_with(".jar"))
    })?;
    let configuration = if cfg!(windows) {
        "config_win"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") && home.join("config_mac_arm").is_dir() { "config_mac_arm" } else { "config_mac" }
    } else if cfg!(target_arch = "aarch64") && home.join("config_linux_arm").is_dir() {
        "config_linux_arm"
    } else {
        "config_linux"
    };
    let bundled = root.join("jre").join("bin").join(if cfg!(windows) { "java.exe" } else { "java" });
    let java = Some(bundled).filter(|path| path.is_file()).or_else(java_executable)?;
    // jdtls keeps per-project index data; one directory per workspace path.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&workspace, &mut hasher);
    let data = root.join("jdtls-data").join(format!("{:016x}", std::hash::Hasher::finish(&hasher)));
    let args = [
        "-Declipse.application=org.eclipse.jdt.ls.core.id1",
        "-Dosgi.bundles.defaultStartLevel=4",
        "-Declipse.product=org.eclipse.jdt.ls.core.product",
        "-Xmx1G",
        "--add-modules=ALL-SYSTEM",
        "--add-opens",
        "java.base/java.util=ALL-UNNAMED",
        "--add-opens",
        "java.base/java.lang=ALL-UNNAMED",
        "-jar",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .chain([
        launcher.to_string_lossy().into_owned(),
        "-configuration".into(),
        home.join(configuration).to_string_lossy().into_owned(),
        "-data".into(),
        data.to_string_lossy().into_owned(),
    ])
    .collect();
    Some(Launch { command: java, args })
}

fn java_executable() -> Option<PathBuf> {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin").join(if cfg!(windows) { "java.exe" } else { "java" }))
        .filter(|path| path.is_file())
        .or_else(|| find_on_path("java"))
}

/// Finds an installed server: PATH first, then the managed directory.
pub fn locate(language: &Language, workspace: &Path) -> Option<Launch> {
    let args = || language.args.iter().map(|arg| arg.to_string()).collect();
    if let Some(command) = find_on_path(language.command) {
        return Some(Launch { command, args: args() });
    }
    let root = install_dir().ok()?;
    if language.id == "java" {
        return jdtls_launch(&root, workspace);
    }
    managed_executable(&root, language).map(|command| Launch { command, args: args() })
}

/// Packages installed with npm for languages served by Node.js servers.
fn npm_packages(language: &Language) -> Option<&'static [&'static str]> {
    match language.id {
        "typescript" => Some(&["typescript", "typescript-language-server"]),
        "html" | "css" => Some(&["vscode-langservers-extracted"]),
        // Version 3 only works through a TypeScript server plugin.
        "vue" => Some(&["typescript", "@vue/language-server@2"]),
        _ => None,
    }
}

/// Initialization options a server needs to start, if any.
pub fn initialization_options(language: &Language) -> Option<Value> {
    if language.id != "vue" {
        return None;
    }
    let managed = install_dir().ok().map(|root| root.join("node/node_modules/typescript/lib"));
    let tsdk = managed.filter(|path| path.is_dir()).or_else(|| {
        // A global tsc lives at <prefix>/node_modules/typescript/bin/tsc.
        let tsc = find_on_path("tsc")?.canonicalize().ok()?;
        Some(tsc.parent()?.parent()?.join("lib")).filter(|path| path.is_dir())
    })?;
    Some(json!({"typescript": {"tsdk": tsdk.to_string_lossy()}, "vue": {"hybridMode": false}}))
}

/// Resolves a configured or requested server command. Known servers that are
/// missing from PATH fall back to a managed install; others are left as given.
pub fn resolve_command(command: &str, workspace: &Path) -> Option<Launch> {
    if Path::new(command).components().count() > 1 {
        return None;
    }
    if let Some(path) = find_on_path(command) {
        return Some(Launch { command: path, args: Vec::new() });
    }
    let language = LANGUAGES.iter().find(|language| language.command == command)?;
    let mut launch = locate(language, workspace)?;
    // Configured arguments replace the defaults for an ordinary executable.
    if language.id != "java" {
        launch.args.clear();
    }
    Some(launch)
}

/// Platform-specific command a user can run to install the server.
pub fn install_hint(language: &Language) -> String {
    match language.id {
        "rust" => "rustup component add rust-analyzer".into(),
        "python" => "pip install --user python-lsp-server".into(),
        "typescript" => "npm install -g typescript typescript-language-server".into(),
        "html" | "css" => "npm install -g vscode-langservers-extracted".into(),
        "vue" => "npm install -g typescript @vue/language-server@2".into(),
        "go" => "go install golang.org/x/tools/gopls@latest".into(),
        "cpp" if cfg!(windows) => "winget install LLVM.LLVM".into(),
        "cpp" if cfg!(target_os = "macos") => "brew install llvm (or xcode-select --install)".into(),
        "cpp" => "sudo apt install clangd (or your distribution's clangd package)".into(),
        "csharp" => "dotnet tool install --global csharp-ls (requires the .NET SDK)".into(),
        "java" => "download Eclipse JDT Language Server and put its jdtls launcher on PATH (requires a recent JDK)".into(),
        _ => format!("install {} and put it on PATH", language.command),
    }
}

/// What an install does, shown before the user confirms it.
pub fn install_description(language: &Language) -> String {
    match language.id {
        "rust" => "runs `rustup component add rust-analyzer`".into(),
        "python" => "creates a Python virtual environment and installs python-lsp-server from PyPI".into(),
        "typescript" => "installs typescript and typescript-language-server from npm".into(),
        "html" | "css" => "installs vscode-langservers-extracted (HTML, CSS, SCSS and Less servers) from npm".into(),
        "vue" => "installs typescript and @vue/language-server@2 from npm".into(),
        "go" => "runs `go install golang.org/x/tools/gopls@latest`".into(),
        "csharp" => "runs `dotnet tool install csharp-ls` into the managed directory; needs the .NET SDK".into(),
        "java" => "downloads Red Hat's Java language support from open-vsx.org (about 140 MB: jdtls plus its own Java runtime)".into(),
        _ => format!("cannot be installed automatically; {}", install_hint(language)),
    }
}

pub fn can_install(language: &Language) -> bool {
    language.id != "cpp"
}

/// Explains a missing server with the commands that fix it.
pub fn missing_message(language: &Language) -> String {
    let mut message = format!(
        "The {} language server ({}) is not installed, so LSP features are unavailable for these files. Install it with: {}",
        language.name,
        language.command,
        install_hint(language)
    );
    if can_install(language) {
        message.push_str(&format!(
            ". Or ask the user to run `/lsp install {}` in dcl (or `dcl lsp install {}`), which installs it under the dcl data directory without administrator rights.",
            language.id, language.id
        ));
    }
    message
}

/// One row per known language for `/lsp`.
pub fn status(workspace: &Path) -> Vec<Value> {
    LANGUAGES
        .iter()
        .map(|language| {
            let launch = locate(language, workspace);
            json!({
                "id": language.id,
                "name": language.name,
                "extensions": language.extensions.iter().map(|(extension, _)| format!(".{extension}")).collect::<Vec<_>>(),
                "server": language.command,
                "installed": launch.is_some(),
                "path": launch.map(|launch| launch.command.to_string_lossy().into_owned()),
                "install": if can_install(language) { format!("/lsp install {}", language.id) } else { install_hint(language) },
                "hint": install_hint(language),
            })
        })
        .collect()
}

async fn run(program: &Path, args: &[&str], envs: &[(&str, &Path)], progress: &(dyn Fn(&str) + Sync)) -> Result<()> {
    progress(&format!("$ {} {}", program.display(), args.join(" ")));
    let mut command = tokio::process::Command::new(program);
    command.args(args).stdin(std::process::Stdio::null());
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command
        .output()
        .await
        .with_context(|| format!("Cannot run {}", program.display()))?;
    if !output.status.success() {
        // Some tools (python -m venv) report failures on stdout.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let text = if stderr.trim().is_empty() { stdout } else { stderr };
        let tail: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).rev().take(8).collect();
        bail!(
            "{} failed ({}):\n{}",
            program.display(),
            output.status,
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    Ok(())
}

fn require(program: &str, why: &str) -> Result<PathBuf> {
    find_on_path(program).with_context(|| format!("{program} was not found on PATH; {why}"))
}

/// Installs a server under `install_dir()`. Only call after the user agreed.
pub async fn install(language: &Language, progress: &(dyn Fn(&str) + Sync)) -> Result<Launch> {
    let root = install_dir()?;
    std::fs::create_dir_all(&root)?;
    match language.id {
        "rust" => {
            let rustup = require("rustup", "install Rust with rustup first (https://rustup.rs)")?;
            run(&rustup, &["component", "add", "rust-analyzer"], &[], progress).await?;
        }
        "python" => {
            let venv = root.join("python");
            let venv_arg = venv.to_string_lossy().into_owned();
            let result = async {
                // uv needs neither ensurepip nor python3-venv.
                if let Some(uv) = find_on_path("uv") {
                    run(&uv, &["venv", "--quiet", "--clear", &venv_arg], &[], progress).await?;
                    let python = venv.join(if cfg!(windows) { "Scripts/python.exe" } else { "bin/python" });
                    let python_arg = python.to_string_lossy().into_owned();
                    return run(&uv, &["pip", "install", "--quiet", "--python", &python_arg, "python-lsp-server"], &[], progress).await;
                }
                let python = find_on_path("python3")
                    .or_else(|| find_on_path("python"))
                    .context("Python 3 was not found on PATH; install Python 3 first")?;
                run(&python, &["-m", "venv", &venv_arg], &[], progress)
                    .await
                    .context("Creating a virtual environment failed; on Debian/Ubuntu install the python3-venv package, or install uv")?;
                let pip = venv.join(if cfg!(windows) { "Scripts" } else { "bin" }).join(if cfg!(windows) { "pip.exe" } else { "pip" });
                run(&pip, &["install", "--quiet", "python-lsp-server"], &[], progress).await
            }
            .await;
            if result.is_err() {
                let _ = std::fs::remove_dir_all(&venv);
            }
            result?;
        }
        _ if npm_packages(language).is_some() => {
            let npm = require("npm", "install Node.js first (https://nodejs.org)")?;
            let prefix = root.join("node");
            std::fs::create_dir_all(&prefix)?;
            let prefix_arg = prefix.to_string_lossy().into_owned();
            let mut args = vec!["install", "--prefix", &prefix_arg, "--no-fund", "--no-audit"];
            args.extend(npm_packages(language).unwrap_or_default());
            run(&npm, &args, &[], progress).await?;
        }
        "go" => {
            let go = require("go", "install Go first (https://go.dev/dl)")?;
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin)?;
            run(&go, &["install", "golang.org/x/tools/gopls@latest"], &[("GOBIN", bin.as_path())], progress).await?;
        }
        "csharp" => {
            let dotnet = require("dotnet", "install the .NET SDK first (https://dotnet.microsoft.com/download)")?;
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin)?;
            let bin_arg = bin.to_string_lossy().into_owned();
            run(&dotnet, &["tool", "install", "csharp-ls", "--tool-path", &bin_arg], &[], progress).await?;
        }
        "java" => install_jdtls(&root, progress).await?,
        _ => bail!(
            "{} cannot be installed automatically; install it with: {}",
            language.command,
            install_hint(language)
        ),
    }
    let workspace = std::env::current_dir().unwrap_or_else(|_| root.clone());
    locate(language, &workspace).with_context(|| {
        format!("{} was installed but could not be found afterwards; check {}", language.command, root.display())
    })
}

fn java_platform() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("macos", "aarch64") => "darwin-arm64",
        ("windows", "x86_64") => "win32-x64",
        ("windows", "aarch64") => "win32-arm64",
        (os, arch) => bail!("No packaged Java language server for {os}/{arch}; install jdtls manually"),
    })
}

async fn install_jdtls(root: &Path, progress: &(dyn Fn(&str) + Sync)) -> Result<()> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    let platform = java_platform()?;
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(1800)).build()?;
    let release: Value = client
        .get(format!("{JAVA_PACKAGE_API}/{platform}/latest"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("open-vsx.org did not return package metadata")?;
    let url = release["files"]["download"].as_str().context("open-vsx.org returned no download link")?;
    progress(&format!("Downloading redhat.java {} ({platform})", release["version"].as_str().unwrap_or("latest")));
    let response = client.get(url).send().await?.error_for_status()?;
    let archive = root.join("java-package.vsix");
    let mut file = tokio::fs::File::create(&archive).await?;
    let mut stream = response.bytes_stream();
    let (mut written, mut reported) = (0u64, 0u64);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        if written - reported >= 20 * 1_048_576 {
            reported = written;
            progress(&format!("{} MB", written / 1_048_576));
        }
    }
    file.flush().await?;
    drop(file);
    let mut magic = [0u8; 2];
    std::io::Read::read_exact(&mut std::fs::File::open(&archive)?, &mut magic)?;
    if magic != *b"PK" {
        let _ = std::fs::remove_file(&archive);
        bail!("The download is not a package archive (the server may be unavailable); try again later");
    }
    progress(&format!("Downloaded {:.1} MB; unpacking", written as f64 / 1_048_576.0));
    // Unpack beside the old install and swap, so a failure keeps it working.
    let staging = root.join("java.new");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let result = unzip(&archive, &staging, progress).await;
    let _ = std::fs::remove_file(&archive);
    result?;
    let extension = staging.join("extension");
    let server = extension.join("server");
    ensure!(server.join("plugins").is_dir(), "The package has no language server");
    let jre = std::fs::read_dir(extension.join("jre"))
        .ok()
        .and_then(|mut entries| entries.find_map(|entry| entry.ok().map(|entry| entry.path())));
    for (name, source) in [("jdtls", Some(server)), ("jre", jre)] {
        let target = root.join(name);
        let _ = std::fs::remove_dir_all(&target);
        if let Some(source) = source {
            std::fs::rename(source, &target)?;
        }
    }
    let _ = std::fs::remove_dir_all(&staging);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for entry in std::fs::read_dir(root.join("jre").join("bin")).into_iter().flatten().flatten() {
            let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o755));
        }
    }
    Ok(())
}

/// Extracts a zip archive with the platform's own tools.
async fn unzip(archive: &Path, target: &Path, progress: &(dyn Fn(&str) + Sync)) -> Result<()> {
    let archive_arg = archive.to_string_lossy().into_owned();
    let target_arg = target.to_string_lossy().into_owned();
    if !cfg!(target_os = "linux") {
        // bsdtar on Windows 10+ and macOS reads zip archives.
        let tar = require("tar", "it is needed to unpack the package")?;
        return run(&tar, &["-xf", &archive_arg, "-C", &target_arg], &[], progress).await;
    }
    if let Some(unzip) = find_on_path("unzip") {
        return run(&unzip, &["-q", &archive_arg, "extension/server/*", "extension/jre/*", "-d", &target_arg], &[], progress).await;
    }
    let python = find_on_path("python3").context("Neither unzip nor python3 was found; install unzip")?;
    run(&python, &["-m", "zipfile", "-e", &archive_arg, &target_arg], &[], progress).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_to_languages_and_ids_accept_aliases() {
        assert_eq!(for_extension("JAVA").map(|(language, id)| (language.id, id)), Some(("java", "java")));
        assert_eq!(for_extension("tsx").map(|(_, id)| id), Some("typescriptreact"));
        assert!(for_extension("txt").is_none());
        assert_eq!(by_id("js").map(|language| language.id), Some("typescript"));
        assert_eq!(by_id("c++").map(|language| language.id), Some("cpp"));
        assert_eq!(by_id("C#").map(|language| language.command), Some("csharp-ls"));
        assert_eq!(for_extension("scss").map(|(language, id)| (language.id, id)), Some(("css", "scss")));
        assert_eq!(for_extension("vue").map(|(language, _)| language.command), Some("vue-language-server"));
        assert!(initialization_options(by_id("html").unwrap()).is_none());
    }

    #[test]
    fn missing_servers_explain_how_to_install() {
        let java = by_id("java").unwrap();
        let message = missing_message(java);
        assert!(message.contains("jdtls") && message.contains("/lsp install java"), "{message}");
        let cpp = by_id("cpp").unwrap();
        assert!(!missing_message(cpp).contains("/lsp install"));
    }
}
