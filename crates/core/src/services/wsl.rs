//! Explicit WSL launches with cancellation limited to the owned Linux process group.
use super::shell_settings::WslSettings;
use anyhow::{ensure, Context, Result};
use std::{path::Path, time::Duration};
use tokio::process::Command;

pub fn executable() -> Result<String> {
    ensure!(cfg!(windows), "WSL is available only on Windows");
    let path = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .map(|root| root.join("System32/wsl.exe"))
        .filter(|path| path.is_file())
        .or_else(|| crate::tools::lsp_servers::find_on_path("wsl"))
        .context("wsl.exe not found; install WSL first")?;
    Ok(path.to_string_lossy().into_owned())
}

fn process() -> Result<Command> {
    let mut command = Command::new(executable()?);
    command.kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    Ok(command)
}

fn decode(bytes: &[u8]) -> String {
    if bytes.contains(&0) {
        String::from_utf16_lossy(
            &bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

async fn output(mut command: Command) -> Result<String> {
    command.stdin(std::process::Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .context("WSL startup timed out")??;
    ensure!(
        output.status.success(),
        "WSL failed ({}): {}{}",
        output.status,
        decode(&output.stderr),
        decode(&output.stdout)
    );
    Ok(decode(&output.stdout)
        .trim_start_matches('\u{feff}')
        .trim()
        .into())
}

pub async fn distributions() -> Result<Vec<String>> {
    let mut command = process()?;
    command.args(["--list", "--quiet"]);
    Ok(output(command)
        .await?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

pub async fn linux_path(settings: &WslSettings, path: &Path) -> Result<String> {
    let distribution = settings
        .distribution
        .as_deref()
        .context("Select a WSL distribution explicitly")?;
    let mut command = process()?;
    command
        .args([
            "--distribution",
            distribution,
            "--exec",
            &settings.bash_path,
            "-c",
            "wslpath -a -u -- \"$1\"",
            "dcl-path",
        ])
        .arg(path);
    output(command).await
}

/// Removing the lease also works when a future is dropped before Linux starts.
pub struct Guard(tempfile::TempDir);
impl Guard {
    pub fn cancel(&self) {
        let _ = std::fs::write(self.0.path().join("cancel"), b"cancel");
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub guard: Guard,
}

// The group leader waits after the user's command exits so its PID cannot be
// reused before cleanup. The monitor kills this group once, then reaps it.
const SUPERVISOR: &str = r#"
trap '' HUP TERM
lease=$1; bash=$2; mode=$3; payload=$4
[ -d "$lease" ] && [ ! -e "$lease/cancel" ] || exit 125
command -v setsid >/dev/null || { printf 'WSL requires setsid\n' >&2; exit 126; }
setsid "$bash" -c '
lease=$1; bash=$2; mode=$3; payload=$4
[ -d "$lease" ] && [ ! -e "$lease/cancel" ] || exit 125
if [ "$mode" = terminal ]; then
    "$bash" --noprofile --norc +m -i <&0 &
else
    "$bash" -c "$payload" <&0 &
fi
wait $!
status=$?
printf "%s" "$status" > "$lease/result.tmp"
mv "$lease/result.tmp" "$lease/result"
while :; do sleep 1; done
' dcl-group "$lease" "$bash" "$mode" "$payload" <&0 &
group=$!
while [ -d "$lease" ] && [ ! -e "$lease/cancel" ] && [ ! -e "$lease/result" ]; do
    kill -0 "$group" 2>/dev/null || break
    sleep 0.05
done
kill -KILL -- "-$group" 2>/dev/null
wait "$group" 2>/dev/null
status=$?
if [ -f "$lease/result" ]; then read -r status < "$lease/result"; fi
exit "$status"
"#;

pub async fn launch(settings: &WslSettings, cwd: &Path, text: Option<&str>) -> Result<Launch> {
    let distribution = settings
        .distribution
        .as_deref()
        .context("Select a WSL distribution explicitly")?;
    ensure!(
        distributions()
            .await?
            .iter()
            .any(|name| name == distribution),
        "WSL distribution is not installed: {distribution}"
    );
    let guard = Guard(tempfile::Builder::new().prefix("dcl-wsl-").tempdir()?);
    let lease = linux_path(settings, guard.0.path()).await?;
    let cwd = linux_path(settings, cwd).await?;
    Ok(Launch {
        program: executable()?,
        args: [
            "--distribution",
            distribution,
            "--cd",
            &cwd,
            "--exec",
            &settings.bash_path,
            "-c",
            SUPERVISOR,
            "dcl-supervisor",
            &lease,
            &settings.bash_path,
            if text.is_some() {
                "command"
            } else {
                "terminal"
            },
            text.unwrap_or(""),
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        guard,
    })
}

impl Launch {
    pub fn command(self) -> (Command, Guard) {
        let mut command = Command::new(self.program);
        command.args(self.args).kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        (command, self.guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribution_output_decodes_utf16_and_utf8() {
        let bytes: Vec<_> = "Ubuntu\r\n测试发行版\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode(&bytes), "Ubuntu\r\n测试发行版\r\n");
        assert_eq!(decode(b"Ubuntu\n"), "Ubuntu\n");
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "requires the explicitly selected dcl-linux-test WSL distribution"]
    async fn real_wsl_status_unicode_and_cancellation() {
        let settings = WslSettings {
            distribution: Some("dcl-linux-test".into()),
            ..Default::default()
        };
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("目录 space");
        std::fs::create_dir(&cwd).unwrap();
        let (mut command, _guard) =
            launch(&settings, &cwd, Some("printf '中文输出\\n'; pwd; exit 7"))
                .await
                .unwrap()
                .command();
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.status.code(), Some(7), "{}", decode(&output.stderr));
        assert!(decode(&output.stdout).contains("中文输出"));
        assert!(decode(&output.stdout).contains("目录 space"));
        let marker = linux_path(&settings, &cwd.join("leaked")).await.unwrap();
        let text = format!(
            "(sleep 2; printf leaked > '{}') & printf ready; wait",
            marker.replace('\'', "'\"'\"'")
        );
        let (mut command, guard) = launch(&settings, &cwd, Some(&text))
            .await
            .unwrap()
            .command();
        command.stdout(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();
        use tokio::io::AsyncReadExt;
        let mut ready = [0u8; 5];
        tokio::time::timeout(
            Duration::from_secs(10),
            child.stdout.as_mut().unwrap().read_exact(&mut ready),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&ready, b"ready");
        drop(guard);
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !cwd.join("leaked").exists(),
            "Linux grandchild survived cancellation"
        );
    }
}
