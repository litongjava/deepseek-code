//! Explicit cross-platform environment bootstrap commands.
use anyhow::{ensure, Context, Result};
use deepseek_code_core::runtime::RuntimePaths;
use std::path::Path;

fn script(target: &str, windows: bool) -> &'static str {
    match (target, windows) {
        ("node", true) => {
            r#"$ErrorActionPreference='Stop'
if (-not (Get-Command nvm.exe -ErrorAction SilentlyContinue)) {
  winget install --id CoreyButler.NVMforWindows --exact --accept-source-agreements --accept-package-agreements
  if ($LASTEXITCODE -ne 0) { throw 'nvm installation failed' }
}
$env:NVM_HOME=[Environment]::GetEnvironmentVariable('NVM_HOME','User')
if (-not $env:NVM_HOME) { $env:NVM_HOME=[Environment]::GetEnvironmentVariable('NVM_HOME','Machine') }
$env:NVM_SYMLINK=[Environment]::GetEnvironmentVariable('NVM_SYMLINK','User')
if (-not $env:NVM_SYMLINK) { $env:NVM_SYMLINK=[Environment]::GetEnvironmentVariable('NVM_SYMLINK','Machine') }
$env:Path="$env:NVM_HOME;$env:NVM_SYMLINK;$env:Path"
nvm install lts
if ($LASTEXITCODE -ne 0) { throw 'Node installation failed' }
nvm use lts
if ($LASTEXITCODE -ne 0) { throw 'Node activation failed; an elevated terminal may be required' }
node --version
if ($LASTEXITCODE -ne 0) { throw 'Node verification failed' }
"#
        }
        ("node", false) => {
            r#"set -eu
export NVM_DIR="${NVM_DIR:-$HOME/.nvm}"
if [ ! -s "$NVM_DIR/nvm.sh" ]; then
  installer=$(mktemp)
  trap 'rm -f "$installer"' EXIT
  curl -fLsS https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.3/install.sh -o "$installer"
  bash "$installer"
fi
. "$NVM_DIR/nvm.sh"
nvm install --lts
nvm use --lts
node --version
"#
        }
        ("python", true) => {
            r#"$ErrorActionPreference='Stop'
$prefix=Join-Path $env:USERPROFILE 'miniconda3'
$conda=Join-Path $prefix 'Scripts\conda.exe'
if (-not (Test-Path -LiteralPath $conda)) {
  if (Test-Path -LiteralPath $prefix) { throw 'miniconda3 exists without conda; choose or repair that installation first' }
  $installer=Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString()+'.exe')
  try {
    Invoke-WebRequest 'https://repo.anaconda.com/miniconda/Miniconda3-latest-Windows-x86_64.exe' -OutFile $installer
    $process=Start-Process -FilePath $installer -ArgumentList @('/S','/InstallationType=JustMe','/RegisterPython=0',"/D=$prefix") -Wait -PassThru -WindowStyle Hidden
    if ($process.ExitCode -ne 0) { throw 'Miniconda installation failed' }
  } finally { Remove-Item -LiteralPath $installer -ErrorAction SilentlyContinue }
}
if (-not (Test-Path -LiteralPath (Join-Path $prefix 'envs\dcl-python\python.exe'))) {
& $conda create --yes --name dcl-python --override-channels --channel conda-forge python=3
if ($LASTEXITCODE -ne 0) { throw 'Python environment creation failed' }
}
& $conda init powershell
if ($LASTEXITCODE -ne 0) { throw 'Conda shell initialization failed' }
& $conda run --name dcl-python python --version
if ($LASTEXITCODE -ne 0) { throw 'Python verification failed' }
"#
        }
        ("python", false) => {
            r#"set -eu
prefix="$HOME/miniconda3"
if [ ! -x "$prefix/bin/conda" ]; then
  [ ! -e "$prefix" ] || { echo 'miniconda3 already exists without conda' >&2; exit 1; }
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) platform=MacOSX-arm64;;
    Darwin-x86_64) platform=MacOSX-x86_64;;
    Linux-x86_64) platform=Linux-x86_64;;
    Linux-aarch64|Linux-arm64) platform=Linux-aarch64;;
    *) echo 'Unsupported Miniconda platform' >&2; exit 1;;
  esac
  installer=$(mktemp)
  trap 'rm -f "$installer"' EXIT
  curl -fLsS "https://repo.anaconda.com/miniconda/Miniconda3-latest-$platform.sh" -o "$installer"
  bash "$installer" -b -p "$prefix"
fi
if [ ! -x "$prefix/envs/dcl-python/bin/python" ]; then
  "$prefix/bin/conda" create --yes --name dcl-python --override-channels --channel conda-forge python=3
fi
"$prefix/bin/conda" init bash zsh
"$prefix/bin/conda" run --name dcl-python python --version
"#
        }
        _ => "",
    }
}

pub async fn run(
    paths: &RuntimePaths,
    workspace: &Path,
    target: &str,
    dry_run: bool,
) -> Result<()> {
    let targets: &[&str] = if target == "all" {
        &["node", "python", "codegraph"]
    } else {
        &[target]
    };
    for target in targets {
        println!("Initializing {target} in {}", workspace.display());
        if *target == "codegraph" {
            if dry_run {
                println!("npm install --global @colbymchenry/codegraph\ncodegraph install --yes --init\nRegister codegraph MCP using the installed Node executable and package entry point.");
                continue;
            }
            codegraph(paths, workspace).await?;
            continue;
        }
        let body = script(target, cfg!(windows));
        ensure!(!body.is_empty(), "Unknown environment: {target}");
        if dry_run {
            println!("{body}");
            continue;
        }
        let mut command = tokio::process::Command::new(if cfg!(windows) {
            "powershell.exe"
        } else {
            "bash"
        });
        if cfg!(windows) {
            command.args(["-NoProfile", "-NonInteractive", "-Command"]);
        } else {
            command.arg("-c");
        }
        let status = command
            .arg(body)
            .current_dir(workspace)
            .kill_on_drop(true)
            .status()
            .await?;
        ensure!(
            status.success(),
            "{target} initialization failed ({status}); completed steps are preserved"
        );
    }
    if dry_run {
        println!("Preview complete; no installation was performed.");
    } else {
        println!("Environment initialization complete. Open a new terminal to reload shell configuration.");
    }
    Ok(())
}

async fn codegraph(paths: &RuntimePaths, workspace: &Path) -> Result<()> {
    // A fresh shell loads nvm after an earlier bootstrap step updated its profile.
    let bootstrap = if cfg!(windows) {
        r#"$ErrorActionPreference='Stop'; $env:NVM_HOME=[Environment]::GetEnvironmentVariable('NVM_HOME','User'); if (-not $env:NVM_HOME) { $env:NVM_HOME=[Environment]::GetEnvironmentVariable('NVM_HOME','Machine') }; $env:NVM_SYMLINK=[Environment]::GetEnvironmentVariable('NVM_SYMLINK','User'); if (-not $env:NVM_SYMLINK) { $env:NVM_SYMLINK=[Environment]::GetEnvironmentVariable('NVM_SYMLINK','Machine') }; $env:Path=[Environment]::ExpandEnvironmentVariables([Environment]::GetEnvironmentVariable('Path','Machine')+';'+[Environment]::GetEnvironmentVariable('Path','User'))+';'+$env:Path; npm.cmd install --global @colbymchenry/codegraph; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $prefix=npm.cmd prefix --global; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; & (Join-Path $prefix.Trim() 'codegraph.cmd') install --yes --init; exit $LASTEXITCODE"#
    } else {
        r#"set -eu; export NVM_DIR="${NVM_DIR:-$HOME/.nvm}"; if [ -s "$NVM_DIR/nvm.sh" ]; then . "$NVM_DIR/nvm.sh"; fi; npm install --global @colbymchenry/codegraph; codegraph install --yes --init"#
    };
    let mut command = tokio::process::Command::new(if cfg!(windows) {
        "powershell.exe"
    } else {
        "bash"
    });
    command.args(if cfg!(windows) {
        vec!["-NoProfile", "-Command"]
    } else {
        vec!["-c"]
    });
    ensure!(
        command
            .arg(bootstrap)
            .current_dir(workspace)
            .kill_on_drop(true)
            .status()
            .await?
            .success(),
        "CodeGraph installation failed"
    );
    let probe = if cfg!(windows) {
        r#"$env:NVM_HOME=[Environment]::GetEnvironmentVariable('NVM_HOME','User'); if (-not $env:NVM_HOME) { $env:NVM_HOME=[Environment]::GetEnvironmentVariable('NVM_HOME','Machine') }; $env:NVM_SYMLINK=[Environment]::GetEnvironmentVariable('NVM_SYMLINK','User'); if (-not $env:NVM_SYMLINK) { $env:NVM_SYMLINK=[Environment]::GetEnvironmentVariable('NVM_SYMLINK','Machine') }; $env:Path=[Environment]::ExpandEnvironmentVariables([Environment]::GetEnvironmentVariable('Path','Machine')+';'+[Environment]::GetEnvironmentVariable('Path','User'))+';'+$env:Path; [Console]::OutputEncoding=[Text.UTF8Encoding]::new(); node -p process.execPath; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; npm.cmd root --global; exit $LASTEXITCODE"#
    } else {
        r#"set -eu; export NVM_DIR="${NVM_DIR:-$HOME/.nvm}"; if [ -s "$NVM_DIR/nvm.sh" ]; then . "$NVM_DIR/nvm.sh"; fi; node -p process.execPath; npm root --global"#
    };
    let mut command = tokio::process::Command::new(if cfg!(windows) {
        "powershell.exe"
    } else {
        "bash"
    });
    command.args(if cfg!(windows) {
        vec!["-NoProfile", "-Command"]
    } else {
        vec!["-c"]
    });
    let result = command.arg(probe).current_dir(workspace).output().await?;
    ensure!(
        result.status.success(),
        "Cannot locate installed Node and CodeGraph"
    );
    let output = String::from_utf8(result.stdout)?;
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let node = lines.next().context("Node executable missing")?.trim();
    let package = Path::new(lines.next().context("npm global root missing")?.trim())
        .join("@colbymchenry/codegraph");
    let metadata: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(package.join("package.json"))?)?;
    let bin = metadata["bin"]["codegraph"]
        .as_str()
        .or_else(|| metadata["bin"].as_str())
        .context("CodeGraph executable missing")?;
    let entry = package.join(bin).canonicalize()?;
    let manager =
        deepseek_code_core::mcp::McpServerManager::new(paths.data_dir.join("mcp-servers.json"));
    std::fs::create_dir_all(&paths.data_dir)?;
    manager.initialize().await?;
    if manager.get_server("codegraph").await.is_some() {
        println!("Existing codegraph MCP configuration preserved.");
        return Ok(());
    }
    manager
        .add_server(deepseek_code_core::mcp::McpServerConfig {
            id: "codegraph".into(),
            name: "codegraph".into(),
            command: node.into(),
            args: vec![
                entry.to_string_lossy().into_owned(),
                "serve".into(),
                "--mcp".into(),
            ],
            cwd: Some(workspace.to_path_buf()),
            enabled: true,
            env: Default::default(),
            transport: None,
            url: None,
            headers: Default::default(),
        })
        .await?;
    println!("CodeGraph MCP registered.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scripts_cover_platforms_without_fixed_machine_paths() {
        for windows in [false, true] {
            assert!(script("node", windows).contains("nvm"));
            assert!(script("python", windows).contains("python=3"));
            assert!(!script("python", windows).contains("D:/"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn node_bootstrap_reuses_nvm_and_stops_on_install_failure() {
        for failure in [false, true] {
            let prefix = format!(
                r#"function nvm.exe {{}}
function winget {{ throw 'Existing nvm should be reused' }}
function nvm {{
  Write-Output ('NVM_STEP:' + ($args -join ' '))
  $global:LASTEXITCODE = if ({failure} -and $args[0] -eq 'install') {{ 9 }} else {{ 0 }}
}}
function node {{ Write-Output 'NODE_VERIFIED'; $global:LASTEXITCODE=0 }}
"#,
                failure = if failure { "$true" } else { "$false" }
            );
            let result = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command"])
                .arg(format!("{prefix}\n{}", script("node", true)))
                .output()
                .unwrap();
            let output = String::from_utf8_lossy(&result.stdout);
            assert_eq!(result.status.success(), !failure, "{output}");
            assert!(output.contains("NVM_STEP:install lts"));
            assert_eq!(output.contains("NVM_STEP:use lts"), !failure);
            assert_eq!(output.contains("NODE_VERIFIED"), !failure);
        }
    }
}
