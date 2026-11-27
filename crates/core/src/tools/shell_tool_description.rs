//! The Bash and Monitor tool descriptions depend on the machine's default
//! shell: models otherwise mix shell syntaxes (for example PowerShell
//! arguments quoted bash-style).
//!
//! The description is part of the cached prompt prefix, so it is computed once
//! per process and rebuilt only after in-process shell settings change.

use super::background_shell;
use crate::services::shell_settings::{self, ResolvedShell, ShellSettings};
use std::sync::Mutex;

const BASH_DESCRIPTION: &str = "Run a command and read its output. Each call spawns a new process; foreground calls start in the workspace and keep their directory across calls, so do not repeat a leading `cd` to the workspace. Explicit background tasks start in the workspace.

Use `Read`, `Grep`, and `Glob` instead of `cat`/`sed`/`head`/`tail`/`grep`/`find` so read and search calls run in parallel. Combine dependent steps with `&&` and independent checks or test runs with `;` or `&&` in one compound command; fix all errors before re-running.

Background tasks (`run_in_background: true`) and timed-out managed foreground commands notify and wake you automatically on completion. Never poll with foreground `sleep` loops or repeated TaskGet calls: continue other work or finish your turn, and call TaskGet only for intermediate output or `block: true`, or TaskStop to stop it. Without a managed session, a foreground timeout stops the process tree.

When a call ends, every process it started is stopped, including background children and auto-spawned helper daemons. To start a long-running service that must survive across calls—such as a frontend dev server (`vite`, `pnpm dev`), a backend/API server, or a local browser/database daemon—always set `detach: true` (returns PID and log paths; stop it explicitly when done). Use `run_in_background: true` only for finite tasks you expect to complete and notify you, not for persistent services.

Ordinary capture keeps up to 1 MiB per stream; long results show head and tail within the preview budget (100 KiB per stream by default). Command permission checks also apply outside the sandbox.";

const GIT_BASH_NOTE: &str = "The default is Git Bash. Use $NAME for variables, /dev/null for discarded output and slash-separated paths. Native Windows executables receive translated path arguments. Set MSYS_NO_PATHCONV=1 for a command that needs its arguments verbatim, or use cygpath -w to supply a Windows path explicitly.";
const POWER_SHELL_NOTE: &str = "The default is Windows PowerShell 5.1 with UTF-8 I/O. Read environment values with $env:NAME and discard output with Out-Null. Use a newline between statements and test $? for conditional execution. Backticks escape special characters.";
const PWSH_NOTE: &str = "The default is PowerShell 7 with UTF-8 I/O. Use $env:NAME for the environment, Out-Null to discard output and a backtick for escaping. Conditional chains accept && and ||.";
const CMD_NOTE: &str = "The default is cmd.exe. Expand variables with %NAME%, redirect to NUL to discard output and quote paths containing spaces with double quotes.";
const FISH_NOTE: &str =
    "The default is fish. Use fish syntax, including set -x to export a variable.";
const WSL_NOTE_PREFIX: &str = "The default is WSL";
const WSL_NOTE_BODY: &str = ". Work with Linux paths inside the distribution. Drive C is normally mounted at /mnt/c, and launching Windows executables requires WSL interop.";
const OTHER_SHELL_HINT: &str =
    "To run a script for another interpreter listed in the `shell` parameter, pass `shell` directly instead of wrapping it inside the default shell, and write the command in that interpreter's syntax.";

/// Shell-aware text for the Bash and Monitor tools.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BashTool {
    description: String,
    monitor_suffix: String,
    shell_parameter: String,
}

impl BashTool {
    pub(super) fn description(&self) -> &str {
        &self.description
    }

    pub(super) fn monitor_suffix(&self) -> &str {
        &self.monitor_suffix
    }

    pub(super) fn shell_parameter(&self) -> &str {
        &self.shell_parameter
    }
}

static CACHE: Mutex<Option<(u64, BashTool)>> = Mutex::new(None);

/// The shell-aware description for this machine, computed once per process and
/// shell-settings change. It must not vary between requests of a session.
pub(super) fn bash_tool() -> BashTool {
    let generation = shell_settings::generation();
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some((cached, tool)) = cache.as_ref() {
        if *cached == generation {
            return tool.clone();
        }
    }
    let settings = background_shell::shell_settings().unwrap_or_default();
    let resolved = settings.resolve(None, false).ok();
    let tool = build(
        &settings,
        resolved.as_ref(),
        std::env::consts::OS,
        &|name| super::find_shell(name).is_some(),
    );
    *cache = Some((generation, tool.clone()));
    tool
}

fn build(
    settings: &ShellSettings,
    default: Option<&ResolvedShell>,
    host_os: &str,
    found: &dyn Fn(&str) -> bool,
) -> BashTool {
    let mut shells = Vec::new();
    for name in [
        "bash",
        "zsh",
        "fish",
        "sh",
        "cmd",
        "powershell",
        "pwsh",
        "wsl",
    ] {
        let usable = match name {
            "bash" => true,
            "cmd" | "powershell" => host_os == "windows",
            "wsl" => host_os == "windows" && settings.wsl.distribution.is_some(),
            _ => found(name),
        };
        if usable {
            shells.push(name);
        }
    }
    let available = default
        .map(|shell| shell.name.as_str())
        .filter(|name| shells.contains(name))
        .unwrap_or("bash");
    let is_git_bash_default =
        default.is_some_and(|shell| shell.name == "bash" && is_git_bash(&shell.program));

    let mut description = BASH_DESCRIPTION.to_owned();
    let mut monitor_suffix = String::new();
    if let Some(shell) = default {
        description.push_str("\n\n");
        if let Some(syntax) = shell_note(shell) {
            monitor_suffix = format!("\n\n{syntax}");
            description.push_str(&syntax);
        } else {
            description.push_str(&format!("The default is {}.", shell.name));
        }
        append_execution_note(&mut description, shell, host_os);
    }
    let default_family = syntax_family(available, available, false);
    if shells
        .iter()
        .any(|name| syntax_family(name, available, is_git_bash_default) != default_family)
    {
        for text in [&mut description, &mut monitor_suffix] {
            text.push_str("\n\n");
            text.push_str(OTHER_SHELL_HINT);
        }
    }

    BashTool {
        description,
        monitor_suffix,
        shell_parameter: format!("Choose from {} (default {available}).", shells.join(", ")),
    }
}

/// Foreground directory tracking, pipeline status and PTY availability for this shell.
fn append_execution_note(note: &mut String, shell: &ResolvedShell, host_os: &str) {
    let name = shell.name.as_str();
    note.push_str(if name == "fish" {
        " Foreground commands start in the workspace."
    } else {
        " A foreground call starts in the previous foreground call's directory. It reports `cwd` on the first call, when the directory changes or resets, and once after resume or context compaction; when `cwd` is omitted, assume the last reported directory and do not repeat `cd`. Add a leading `cd` only to change directories, or use `(cd ... && ...)` or `git -C` for a temporary switch. Other tools still resolve relative paths against the primary working directory."
    });
    if super::pipeline_failure_flag(shell.command_flag, name, shell.wsl.is_some()) {
        note.push_str(" With pipefail enabled, a failing pipeline stage can surface even when the last stage succeeds.");
    }
    if matches!(name, "bash" | "sh" | "zsh" | "wsl") {
        note.push_str(
            " Status 141 indicates partial output because a pipeline reader closed early.",
        );
    }
    note.push_str(match (host_os, name) {
        ("windows", "cmd" | "wsl") => " PTY interaction is unavailable with this shell.",
        ("linux" | "windows", _) => " A background PTY merges stdout and stderr and requires a run without a sandbox policy or process timeout.",
        _ => " PTY interaction is unavailable on this host.",
    });
}

/// Shells whose syntax differs when chosen explicitly. POSIX shells share one
/// family, but WSL counts separately from Git Bash and Windows shells because
/// its paths and available tools differ.
fn syntax_family(shell: &str, default: &str, is_git_bash_default: bool) -> &'static str {
    match shell {
        "bash" | "sh" | "zsh" => "posix",
        "wsl" => {
            if is_git_bash_default || matches!(default, "powershell" | "pwsh" | "cmd") {
                "wsl"
            } else {
                "posix"
            }
        }
        "powershell" | "pwsh" => "powershell",
        "cmd" => "cmd",
        "fish" => "fish",
        _ => "other",
    }
}

/// Syntax guidance shared with Monitor; ordinary POSIX shells need no extra syntax hint.
fn shell_note(shell: &ResolvedShell) -> Option<String> {
    if let Some(wsl) = &shell.wsl {
        let distribution = wsl
            .distribution
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        return Some(match distribution {
            Some(distribution) => {
                format!("{WSL_NOTE_PREFIX} ({distribution}){WSL_NOTE_BODY}")
            }
            None => format!("{WSL_NOTE_PREFIX}{WSL_NOTE_BODY}"),
        });
    }
    match shell.name.as_str() {
        "bash" if is_git_bash(&shell.program) => Some(GIT_BASH_NOTE.into()),
        "powershell" => Some(POWER_SHELL_NOTE.into()),
        "pwsh" => Some(PWSH_NOTE.into()),
        "cmd" => Some(CMD_NOTE.into()),
        "fish" => Some(FISH_NOTE.into()),
        _ => None,
    }
}

/// Git for Windows keeps `bash.exe` under a `Git` install directory.
fn is_git_bash(program: &str) -> bool {
    let normalized = program.replace('\\', "/").to_ascii_lowercase();
    let file = normalized.rsplit('/').next().unwrap_or_default();
    matches!(file, "bash.exe" | "git-bash.exe")
        && normalized.split('/').any(|component| component == "git")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::shell_settings::WslSettings;

    fn resolved(name: &str, program: &str) -> ResolvedShell {
        ResolvedShell {
            name: name.into(),
            program: program.into(),
            command_flag: "-c",
            source: "detected",
            wsl: None,
        }
    }

    fn wsl(distribution: Option<&str>) -> ResolvedShell {
        ResolvedShell {
            name: "wsl".into(),
            program: "wsl.exe".into(),
            command_flag: "-c",
            source: "settings",
            wsl: Some(WslSettings {
                distribution: distribution.map(str::to_owned),
                bash_path: "/bin/bash".into(),
            }),
        }
    }

    fn settings(default: &str, distribution: Option<&str>) -> ShellSettings {
        ShellSettings {
            tool_default: default.into(),
            wsl: WslSettings {
                distribution: distribution.map(str::to_owned),
                bash_path: "/bin/bash".into(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn note_for_each_default_shell() {
        assert_eq!(
            shell_note(&resolved("bash", r"C:\Program Files\Git\bin\bash.exe")),
            Some(GIT_BASH_NOTE.to_string())
        );
        assert_eq!(
            shell_note(&resolved("bash", r"D:\tools\Git\usr\bin\bash.exe")),
            Some(GIT_BASH_NOTE.to_string())
        );
        assert_eq!(shell_note(&resolved("bash", "/bin/bash")), None);
        assert_eq!(shell_note(&resolved("bash", "/usr/bin/bash")), None);
        assert_eq!(shell_note(&resolved("zsh", "/bin/zsh")), None);
        assert_eq!(shell_note(&resolved("sh", "/bin/sh")), None);
        assert_eq!(
            shell_note(&wsl(Some("Ubuntu"))),
            Some(format!("{WSL_NOTE_PREFIX} (Ubuntu){WSL_NOTE_BODY}"))
        );
        assert_eq!(
            shell_note(&wsl(None)),
            Some(format!("{WSL_NOTE_PREFIX}{WSL_NOTE_BODY}"))
        );
        assert_eq!(
            shell_note(&resolved("powershell", "powershell.exe")),
            Some(POWER_SHELL_NOTE.to_string())
        );
        assert_eq!(
            shell_note(&resolved("pwsh", "pwsh.exe")),
            Some(PWSH_NOTE.to_string())
        );
        assert_eq!(
            shell_note(&resolved("cmd", "cmd.exe")),
            Some(CMD_NOTE.to_string())
        );
        assert_eq!(
            shell_note(&resolved("fish", "fish")),
            Some(FISH_NOTE.to_string())
        );
    }

    #[test]
    fn git_bash_detection_needs_a_git_install_and_exe() {
        assert!(is_git_bash(r"C:\Program Files\Git\bin\bash.exe"));
        assert!(is_git_bash("C:/Program Files/Git/usr/bin/bash.exe"));
        assert!(is_git_bash(r"C:\Program Files\Git\git-bash.exe"));
        assert!(!is_git_bash("/bin/bash"));
        assert!(!is_git_bash("/home/git/bin/bash"));
        assert!(!is_git_bash(r"C:\msys64\usr\bin\bash.exe"));
    }

    #[test]
    fn linux_lists_only_detected_shells_and_keeps_bash() {
        let tool = build(
            &settings("bash", None),
            Some(&resolved("bash", "/bin/bash")),
            "linux",
            &|name| name == "bash",
        );
        assert_eq!(tool.shell_parameter(), "Choose from bash (default bash).");
        assert_eq!(tool.monitor_suffix(), "");

        let tool = build(
            &settings("bash", None),
            Some(&resolved("bash", "/bin/bash")),
            "linux",
            &|name| matches!(name, "bash" | "zsh" | "fish" | "sh" | "pwsh"),
        );
        assert_eq!(
            tool.shell_parameter(),
            "Choose from bash, zsh, fish, sh, pwsh (default bash)."
        );
        assert_eq!(tool.monitor_suffix(), format!("\n\n{OTHER_SHELL_HINT}"));
    }

    #[test]
    fn hint_only_appears_for_another_syntax_family() {
        let posix_only = build(
            &settings("bash", None),
            Some(&resolved("bash", "/bin/bash")),
            "linux",
            &|name| matches!(name, "bash" | "sh" | "zsh"),
        );
        assert_eq!(
            posix_only.shell_parameter(),
            "Choose from bash, zsh, sh (default bash)."
        );
        assert_eq!(posix_only.monitor_suffix(), "");
        assert!(!posix_only.description().contains(OTHER_SHELL_HINT));

        let with_pwsh = build(
            &settings("bash", None),
            Some(&resolved("bash", "/bin/bash")),
            "linux",
            &|name| matches!(name, "bash" | "sh" | "pwsh"),
        );
        assert_eq!(
            with_pwsh.shell_parameter(),
            "Choose from bash, sh, pwsh (default bash)."
        );
        assert_eq!(
            with_pwsh.monitor_suffix(),
            format!("\n\n{OTHER_SHELL_HINT}")
        );

        let git_bash = build(
            &settings("bash", None),
            Some(&resolved("bash", r"C:\Program Files\Git\bin\bash.exe")),
            "windows",
            &|name| matches!(name, "bash" | "sh" | "zsh"),
        );
        assert_eq!(
            git_bash.shell_parameter(),
            "Choose from bash, zsh, sh, cmd, powershell (default bash)."
        );
        assert_eq!(
            git_bash.monitor_suffix(),
            format!("\n\n{GIT_BASH_NOTE}\n\n{OTHER_SHELL_HINT}")
        );

        let powershell_default = build(
            &settings("powershell", None),
            Some(&resolved("powershell", "powershell.exe")),
            "windows",
            &|name| name == "bash",
        );
        assert_eq!(
            powershell_default.shell_parameter(),
            "Choose from bash, cmd, powershell (default powershell)."
        );
        assert_eq!(
            powershell_default.monitor_suffix(),
            format!("\n\n{POWER_SHELL_NOTE}\n\n{OTHER_SHELL_HINT}")
        );
    }

    #[test]
    fn wsl_counts_as_a_separate_family_from_git_bash_and_windows_shells() {
        let git_bash_with_wsl = build(
            &settings("bash", Some("Ubuntu")),
            Some(&resolved("bash", r"C:\Program Files\Git\bin\bash.exe")),
            "windows",
            &|_| false,
        );
        assert_eq!(
            git_bash_with_wsl.shell_parameter(),
            "Choose from bash, cmd, powershell, wsl (default bash)."
        );
        assert!(git_bash_with_wsl
            .monitor_suffix()
            .ends_with(OTHER_SHELL_HINT));

        assert_eq!(syntax_family("wsl", "bash", true), "wsl");
        assert_eq!(syntax_family("wsl", "bash", false), "posix");
        assert_eq!(syntax_family("wsl", "wsl", false), "posix");
        assert_eq!(syntax_family("wsl", "powershell", false), "wsl");
    }

    #[test]
    fn non_windows_never_offers_cmd_powershell_or_wsl() {
        let tool = build(&settings("cmd", Some("Ubuntu")), None, "linux", &|name| {
            matches!(
                name,
                "bash" | "zsh" | "fish" | "sh" | "cmd" | "powershell" | "pwsh" | "wsl"
            )
        });
        assert_eq!(
            tool.shell_parameter(),
            "Choose from bash, zsh, fish, sh, pwsh (default bash)."
        );
        assert!(!tool.shell_parameter().contains("cmd"));
        assert!(!tool.shell_parameter().contains("powershell"));
        assert!(!tool.shell_parameter().contains("wsl"));
    }

    #[test]
    fn windows_lists_builtin_shells_and_configured_wsl() {
        let tool = build(
            &settings("powershell", Some("Ubuntu")),
            Some(&resolved(
                "powershell",
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            )),
            "windows",
            &|name| name == "pwsh",
        );
        assert_eq!(
            tool.shell_parameter(),
            "Choose from bash, cmd, powershell, pwsh, wsl (default powershell)."
        );
        assert!(tool
            .monitor_suffix()
            .starts_with(&format!("\n\n{POWER_SHELL_NOTE}")));
        assert!(tool.monitor_suffix().ends_with(OTHER_SHELL_HINT));

        let tool = build(
            &settings("bash", None),
            Some(&resolved("bash", r"C:\Program Files\Git\bin\bash.exe")),
            "windows",
            &|name| matches!(name, "bash" | "zsh" | "fish" | "sh"),
        );
        assert_eq!(
            tool.shell_parameter(),
            "Choose from bash, zsh, fish, sh, cmd, powershell (default bash)."
        );
        assert_eq!(
            tool.monitor_suffix(),
            format!("\n\n{GIT_BASH_NOTE}\n\n{OTHER_SHELL_HINT}")
        );
    }

    #[test]
    fn default_other_than_bash_states_the_actual_default() {
        let tool = build(
            &settings("zsh", None),
            Some(&resolved("zsh", "/bin/zsh")),
            "linux",
            &|name| matches!(name, "bash" | "zsh"),
        );
        assert_eq!(
            tool.shell_parameter(),
            "Choose from bash, zsh (default zsh)."
        );
        assert_eq!(tool.monitor_suffix(), "");
    }

    #[test]
    fn description_matches_the_resolved_shell_and_host() {
        assert!(
            BASH_DESCRIPTION.chars().count() <= 1700,
            "BASH_DESCRIPTION length: {}",
            BASH_DESCRIPTION.chars().count()
        );
        for (label, host, shell) in [
            (
                "Git Bash",
                "windows",
                resolved("bash", r"C:\Program Files\Git\bin\bash.exe"),
            ),
            (
                "PowerShell 5.1",
                "windows",
                resolved("powershell", "powershell.exe"),
            ),
            ("PowerShell 7", "windows", resolved("pwsh", "pwsh.exe")),
            ("cmd", "windows", resolved("cmd", "cmd.exe")),
            ("bash on Linux", "linux", resolved("bash", "/bin/bash")),
            ("zsh on macOS", "macos", resolved("zsh", "/bin/zsh")),
            ("fish", "linux", resolved("fish", "/usr/bin/fish")),
            ("WSL", "windows", wsl(Some("Ubuntu"))),
            ("sh", "linux", resolved("sh", "/bin/sh")),
        ] {
            let name = shell.name.as_str();
            let tool = build(&settings(name, Some("Ubuntu")), Some(&shell), host, &|n| {
                matches!(n, "sh" | "pwsh") || n == name
            });
            let text = tool.description();
            let lower = text.to_ascii_lowercase();
            let words: Vec<_> = lower.split(|c: char| !c.is_ascii_alphanumeric()).collect();
            let interpreter = if name == "pwsh" { "powershell" } else { name };
            for other in [
                "bash",
                "powershell",
                "pwsh",
                "cmd",
                "sh",
                "zsh",
                "fish",
                "wsl",
            ] {
                assert_eq!(
                    words.contains(&other),
                    interpreter == other,
                    "{label}: {other}"
                );
            }
            assert_eq!(text.contains("Git Bash"), label == "Git Bash");
            assert_eq!(text.contains("PowerShell 5.1"), name == "powershell");
            assert_eq!(text.contains("PowerShell 7"), name == "pwsh");
            assert_eq!(
                text.contains("when the directory changes or resets, and once after resume or context compaction"),
                name != "fish"
            );
            assert_eq!(
                text.contains("Foreground commands start in the workspace"),
                name == "fish"
            );
            assert_eq!(text.contains("pipefail"), matches!(name, "bash" | "zsh"));
            assert_eq!(
                text.contains("141"),
                matches!(name, "bash" | "zsh" | "sh" | "wsl")
            );
            let supports_pty = host != "macos" && !matches!(name, "cmd" | "wsl");
            assert_eq!(text.contains("PTY merges stdout and stderr"), supports_pty);
            assert_eq!(
                text.contains("PTY interaction is unavailable"),
                !supports_pty
            );
            assert!(text.ends_with(OTHER_SHELL_HINT));
            println!(
                "{}",
                serde_json::json!({
                    "shell_fixture": label, "description": text, "shell_parameter": tool.shell_parameter()
                })
            );
        }
    }

    #[test]
    fn description_says_calls_stop_their_processes_and_detach_survives() {
        for (label, host, shell) in [
            (
                "Git Bash",
                "windows",
                resolved("bash", r"C:\Program Files\Git\bin\bash.exe"),
            ),
            ("bash on Linux", "linux", resolved("bash", "/bin/bash")),
        ] {
            let tool = build(&settings("bash", None), Some(&shell), host, &|n| {
                n == "bash"
            });
            let text = tool.description();
            assert!(
                text.contains("every process it started is stopped"),
                "{label}: {text}"
            );
            assert!(
                text.contains("auto-spawned helper daemons"),
                "{label}: {text}"
            );
            assert!(text.contains("`detach: true`"), "{label}: {text}");
            assert!(
                text.contains("must survive across calls"),
                "{label}: {text}"
            );
            assert!(
                text.contains("Use `run_in_background: true` only for finite tasks"),
                "{label}: {text}"
            );
        }
    }

    #[test]
    fn cached_description_is_stable_for_the_process() {
        assert_eq!(bash_tool(), bash_tool());
        assert!(!bash_tool().shell_parameter().is_empty());
    }

    #[test]
    fn bash_description_reinforces_efficiency_rules() {
        assert!(BASH_DESCRIPTION.contains("do not repeat a leading `cd`"));
        assert!(BASH_DESCRIPTION.contains("Use `Read`, `Grep`, and `Glob` instead"));
        assert!(BASH_DESCRIPTION.contains("Combine dependent steps with `&&`"));
        assert!(BASH_DESCRIPTION.contains("fix all errors before re-running"));
        assert!(BASH_DESCRIPTION.contains("Never poll with foreground `sleep` loops"));
    }
}
