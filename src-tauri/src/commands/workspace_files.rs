use anyhow::{ensure, Context, Result};
use deepseek_code_core::services::workspace_files::{self as files, FileRef, Scope};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

fn scoped_path(app: &tauri::AppHandle, scope: &Scope, file: &FileRef) -> Result<PathBuf> {
    let db = app
        .try_state::<Arc<crate::db::DbManager>>()
        .context("Database is unavailable")?;
    let roots = db.with_conn(|conn| -> Result<_> {
        let database = conn
            .path()
            .context("Workspace database path is unavailable")?;
        let private_root = Path::new(database)
            .parent()
            .context("Workspace database has no parent")?
            .join("workspaces");
        files::roots(conn, &private_root, scope)
    })??;
    files::resolve(&roots, file, false)
}

fn display_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    PathBuf::from(if let Some(unc) = text.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned()
    })
}
fn parent(path: &Path) -> Result<&Path> {
    if path.is_dir() {
        Ok(path)
    } else {
        path.parent().context("File has no parent directory")
    }
}

#[cfg(windows)]
fn wsl_program() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("SystemRoot")?).join("System32/wsl.exe");
    path.is_file().then_some(path)
}
#[cfg(windows)]
fn decode_wsl(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.iter().skip(1).step_by(2).any(|byte| *byte == 0) {
        String::from_utf16_lossy(
            &bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        )
        .trim_start_matches('\u{feff}')
        .to_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}
#[cfg(windows)]
async fn wsl_output(args: &[&str]) -> Result<String> {
    let mut command = tokio::process::Command::new(wsl_program().context("WSL is not installed")?);
    command.args(args).kill_on_drop(true);
    command.creation_flags(0x08000000);
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), command.output())
        .await
        .context("WSL operation timed out")??;
    ensure!(
        output.status.success(),
        "WSL operation failed: {}",
        decode_wsl(&output.stderr).trim()
    );
    Ok(decode_wsl(&output.stdout).trim().to_owned())
}
#[cfg(windows)]
async fn distributions() -> Result<Vec<String>> {
    Ok(wsl_output(&["--list", "--quiet"])
        .await?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}
#[cfg(windows)]
fn unc_wsl(path: &Path) -> Result<Option<(String, String)>> {
    let text = display_path(path).to_string_lossy().into_owned();
    if !text.starts_with("\\\\") {
        return Ok(None);
    }
    let mut parts = text[2..].split('\\');
    let host = parts.next().unwrap_or_default();
    ensure!(
        host.eq_ignore_ascii_case("wsl$") || host.eq_ignore_ascii_case("wsl.localhost"),
        "WSL does not support this UNC path"
    );
    let distro = parts
        .next()
        .filter(|value| !value.is_empty())
        .context("WSL UNC path has no distribution")?;
    Ok(Some((
        distro.into(),
        format!("/{}", parts.collect::<Vec<_>>().join("/")),
    )))
}
#[cfg(windows)]
async fn wsl_path(path: &Path, distribution: &str) -> Result<String> {
    if let Some((owner, path)) = unc_wsl(path)? {
        ensure!(
            owner.eq_ignore_ascii_case(distribution),
            "UNC path belongs to another WSL distribution"
        );
        return Ok(path);
    }
    let path = display_path(path).to_string_lossy().into_owned();
    let bytes = path.as_bytes();
    ensure!(
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\',
        "WSL requires a drive path or distribution UNC path"
    );
    let converted = wsl_output(&[
        "--distribution",
        distribution,
        "--exec",
        "wslpath",
        "-a",
        "-u",
        &path,
    ])
    .await?;
    ensure!(
        converted.starts_with('/') && !converted.contains(['\r', '\n', '\0']),
        "WSL returned an invalid path"
    );
    Ok(converted)
}

#[tauri::command]
pub async fn workspace_file_capabilities(
    app: tauri::AppHandle,
    scope: Scope,
    file: FileRef,
) -> Result<Value, String> {
    let path = scoped_path(&app, &scope, &file).map_err(|error| error.to_string())?;
    let mut reason: Option<String> = None;
    let mut wsl_distributions = Vec::<String>::new();
    #[cfg(windows)]
    match distributions().await {
        Ok(distributions) => {
            for distribution in distributions {
                match wsl_path(
                    parent(&path).map_err(|error| error.to_string())?,
                    &distribution,
                )
                .await
                {
                    Ok(_) => wsl_distributions.push(distribution),
                    Err(error) => reason = Some(error.to_string()),
                }
            }
        }
        Err(error) => reason = Some(error.to_string()),
    }
    #[cfg(not(windows))]
    {
        reason = Some("WSL terminals are available only on Windows".into());
    }
    let supported = cfg!(any(windows, target_os = "macos", target_os = "linux"));
    Ok(
        json!({"reveal":supported,"defaultApp":supported && path.is_file(),"terminal":supported,"saveAs":path.is_file(),"wslDistributions":wsl_distributions,"reason":reason}),
    )
}

#[derive(Debug)]
struct Launch {
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
}
fn reveal_launch(path: &Path, platform: &str) -> Result<Launch> {
    let cwd = parent(path)?.to_path_buf();
    let target = display_path(path).to_string_lossy().into_owned();
    let (program, args) = match platform {
        "windows" => ("explorer.exe", vec!["/select,".into(), target]),
        "macos" => ("open", vec!["-R".into(), target]),
        "linux" => ("xdg-open", vec![cwd.to_string_lossy().into_owned()]),
        _ => anyhow::bail!("File reveal is not supported on this platform"),
    };
    Ok(Launch {
        program: program.into(),
        args,
        cwd,
    })
}
fn launch(spec: Launch) -> Result<()> {
    let mut command = std::process::Command::new(spec.program);
    command.args(spec.args).current_dir(spec.cwd);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.spawn()?;
    Ok(())
}

fn open_default(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "shell32")]
        extern "system" {
            fn ShellExecuteW(
                window: *mut std::ffi::c_void,
                operation: *const u16,
                file: *const u16,
                parameters: *const u16,
                directory: *const u16,
                show: i32,
            ) -> isize;
        }
        let wide = |path: &Path| {
            path.as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        };
        let file = wide(&display_path(path));
        let directory = wide(&display_path(parent(path)?));
        let operation = "open\0".encode_utf16().collect::<Vec<_>>();
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                directory.as_ptr(),
                1,
            )
        };
        ensure!(
            result > 32,
            "The system could not open this file (error {result})"
        );
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let program = match std::env::consts::OS {
            "macos" => "open",
            "linux" => "xdg-open",
            _ => anyhow::bail!("Default applications are not supported on this platform"),
        };
        launch(Launch {
            program: program.into(),
            args: vec![path.to_string_lossy().into_owned()],
            cwd: parent(path)?.into(),
        })
    }
}

#[tauri::command]
pub async fn workspace_file_action(
    app: tauri::AppHandle,
    scope: Scope,
    file: FileRef,
    action: String,
    distribution: Option<String>,
) -> Result<(), String> {
    async fn run(
        app: tauri::AppHandle,
        scope: Scope,
        file: FileRef,
        action: String,
        distribution: Option<String>,
    ) -> Result<()> {
        let path = scoped_path(&app, &scope, &file)?;
        let cwd = parent(&path)?;
        match action.as_str() {
            "reveal" => launch(reveal_launch(&path, std::env::consts::OS)?)?,
            "default-app" => {
                ensure!(path.is_file(), "Select a file first");
                open_default(&path)?;
            }
            "terminal" => crate::worktree::changes::open_workspace(&display_path(cwd), "terminal")?,
            "wsl-terminal" => {
                #[cfg(windows)]
                {
                    let distribution = distribution.context("Select a WSL distribution")?;
                    ensure!(
                        distributions().await?.contains(&distribution),
                        "WSL distribution is unavailable"
                    );
                    let target = wsl_path(cwd, &distribution).await?;
                    crate::worktree::changes::open_console(
                        &display_path(cwd),
                        &wsl_program().context("WSL is unavailable")?,
                        &["--distribution", &distribution, "--cd", &target],
                    )?;
                }
                #[cfg(not(windows))]
                {
                    let _ = distribution;
                    anyhow::bail!("WSL terminals are available only on Windows");
                }
            }
            _ => anyhow::bail!("Unknown file action"),
        }
        Ok(())
    }
    run(app, scope, file, action, distribution)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn workspace_file_save_as(
    app: tauri::AppHandle,
    scope: Scope,
    file: FileRef,
) -> Result<Value, String> {
    tokio::task::spawn_blocking(move || -> Result<Value> {
        let source = scoped_path(&app, &scope, &file)?;
        ensure!(source.is_file(), "Save as requires a regular file");
        let mut input = std::fs::File::open(&source)?;
        ensure!(
            input.metadata()?.is_file(),
            "Save as requires a regular file"
        );
        let Some(target) = app
            .dialog()
            .file()
            .set_file_name(
                source
                    .file_name()
                    .context("File has no name")?
                    .to_string_lossy(),
            )
            .blocking_save_file()
        else {
            return Ok(json!({"status":"cancelled"}));
        };
        let target = target.into_path().map_err(anyhow::Error::msg)?;
        ensure!(target.is_absolute(), "Save target must be absolute");
        let target = target
            .parent()
            .context("Save target has no parent")?
            .canonicalize()?
            .join(target.file_name().context("Save target has no filename")?);
        if target.canonicalize().ok().as_ref() == Some(&source) {
            return Ok(json!({"status":"same-file"}));
        }
        let approved = files::target_version(&target)?;
        if approved.is_some()
            && !app
                .dialog()
                .message(format!("Replace {}?", display_path(&target).display()))
                .title("Confirm replacement")
                .buttons(MessageDialogButtons::OkCancel)
                .blocking_show()
        {
            return Ok(json!({"status":"cancelled"}));
        }
        // Recheck trust after the modal interaction, keeping destination authority native.
        ensure!(
            scoped_path(&app, &scope, &file)? == source,
            "Source workspace changed while the dialog was open"
        );
        files::save_copy(&mut input, &target, &approved)?;
        Ok(json!({"status":"saved"}))
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reveal_uses_separate_arguments_and_linux_parent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("中文 space ' & file.txt");
        std::fs::write(&file, "content").unwrap();
        let windows = reveal_launch(&file, "windows").unwrap();
        assert_eq!(windows.args, vec!["/select,", file.to_str().unwrap()]);
        assert_eq!(windows.cwd, dir.path());
        let linux = reveal_launch(&file, "linux").unwrap();
        assert_eq!(linux.args, vec![dir.path().to_str().unwrap()]);
    }
    #[cfg(windows)]
    #[test]
    fn wsl_decodes_utf16_and_validates_unc() {
        let bytes = "Ubuntu\r\nDebian\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(decode_wsl(&bytes), "Ubuntu\r\nDebian\r\n");
        assert_eq!(
            unc_wsl(Path::new("\\\\wsl.localhost\\Ubuntu\\home\\user")).unwrap(),
            Some(("Ubuntu".into(), "/home/user".into()))
        );
        assert!(unc_wsl(Path::new("\\\\server\\share\\file")).is_err());
    }
}
