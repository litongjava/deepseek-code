//! Conservative recognition of shell commands that only read. Plan mode runs
//! these without asking, and so do ask and accept-edits modes when every path
//! stays in the workspace; anything unrecognised still needs the user's approval.

/// Programs that never write files or run other commands with ordinary arguments.
const READ_ONLY_PROGRAMS: &[&str] = &[
    "ls", "dir", "cat", "head", "tail", "wc", "pwd", "echo", "grep", "egrep", "fgrep", "rg",
    "tree", "file", "stat", "du", "df", "which", "whereis", "type", "printenv", "date", "uname",
    "whoami", "id", "hostname", "uniq", "cut", "diff", "cmp", "basename", "dirname", "realpath",
    "readlink", "nl", "column", "jq", "true", "false", "test",
];

/// PowerShell cmdlets and aliases that only read (compared case-insensitively).
const READ_ONLY_CMDLETS: &[&str] = &[
    "get-childitem", "gci", "get-content", "gc", "select-string", "sls", "get-location", "gl",
    "test-path", "get-item", "gi", "get-itemproperty", "resolve-path", "select-object", "select",
    "format-table", "ft", "format-list", "fl", "format-wide", "out-string", "measure-object",
    "measure", "sort-object", "get-command", "gcm", "get-date", "get-filehash", "get-psdrive",
    "write-output", "write-host", "convertto-json", "convertfrom-json", "join-path",
    "split-path",
];

/// Git subcommands that only inspect the repository.
const READ_ONLY_GIT: &[&str] = &[
    "status", "log", "diff", "show", "rev-parse", "ls-files", "ls-tree", "blame", "describe",
    "shortlog", "grep", "cat-file", "rev-list", "merge-base", "show-ref", "for-each-ref",
];

pub fn is_read_only(command: &str) -> bool {
    let mut command = command.trim().to_owned();
    // Discarding or merging output streams writes nothing.
    for harmless in ["2>&1", "2>/dev/null", ">/dev/null", "2> /dev/null", "> /dev/null", "2>$null", ">$null"] {
        command = command.replace(harmless, " ");
    }
    if command.is_empty()
        || command.contains(['>', '`', '{', '}'])
        || command.contains("$(")
        || command.contains("<(")
        || command.replace("&&", "").contains('&')
    {
        return false;
    }
    command
        .split(['\n', ';', '|'])
        .flat_map(|part| part.split("&&"))
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .all(segment_read_only)
}

fn segment_read_only(segment: &str) -> bool {
    let words: Vec<String> = segment
        .split_whitespace()
        .map(|word| word.trim_matches(|c| c == '\'' || c == '"').to_owned())
        .collect();
    let Some(program) = words.first() else {
        return true;
    };
    let program = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let lower = program.to_ascii_lowercase();
    let lower = lower.strip_suffix(".exe").unwrap_or(&lower);
    let arguments = &words[1..];
    if arguments.len() == 1 && matches!(arguments[0].as_str(), "--version" | "-V" | "-v" | "version") {
        return true;
    }
    match lower {
        "git" => git_read_only(arguments),
        "find" => !arguments.iter().any(|argument| {
            argument.starts_with("-exec")
                || argument.starts_with("-ok")
                || argument.starts_with("-fprint")
                || argument.starts_with("-fls")
                || argument == "-delete"
        }),
        "sort" => !arguments.iter().any(|argument| argument == "-o" || argument.starts_with("--output")),
        _ => READ_ONLY_PROGRAMS.contains(&lower) || READ_ONLY_CMDLETS.contains(&lower),
    }
}

fn git_read_only(arguments: &[String]) -> bool {
    let mut rest = arguments.iter().skip_while(|argument| argument.starts_with('-'));
    let Some(subcommand) = rest.next() else {
        return false;
    };
    let options: Vec<&str> = rest.map(String::as_str).collect();
    match subcommand.as_str() {
        "branch" | "tag" => options.iter().all(|option| {
            matches!(*option, "-a" | "-r" | "-v" | "-vv" | "--list" | "-l" | "--all" | "--show-current")
        }),
        "remote" => options.iter().all(|option| matches!(*option, "-v" | "--verbose" | "show" | "get-url"))
            || options.first() == Some(&"show"),
        "config" => options.iter().any(|option| matches!(*option, "--get" | "--list" | "-l" | "--get-all")),
        "stash" => options.first().is_some_and(|option| matches!(*option, "list" | "show")),
        subcommand => READ_ONLY_GIT.contains(&subcommand),
    }
}

/// A read-only command whose path arguments all stay inside `workspace`, as
/// the reference CLI runs without asking in its default mode. Home-relative
/// paths and variable expansions are treated as outside.
pub fn reads_only_inside(command: &str, workspace: &std::path::Path) -> bool {
    if !is_read_only(command) {
        return false;
    }
    command
        .split(['\n', ';', '|', '&'])
        .flat_map(|segment| segment.split_whitespace().skip(1))
        .map(|word| word.trim_matches(|c| c == '\'' || c == '"'))
        .flat_map(|word| [Some(word), word.split_once('=').map(|(_, value)| value)])
        .flatten()
        .all(|word| path_inside(word, workspace))
}

fn path_inside(word: &str, workspace: &std::path::Path) -> bool {
    use std::path::{Component, Path, PathBuf};
    if word.contains('$') || word.starts_with('~') || word.starts_with('%') {
        return false;
    }
    let drive = word.len() >= 2 && word.as_bytes()[1] == b':' && word.as_bytes()[0].is_ascii_alphabetic();
    let path = Path::new(word);
    let absolute = path.is_absolute() || drive || word.starts_with('\\') || word.starts_with('/');
    if !absolute && !path.components().any(|part| matches!(part, Component::ParentDir)) {
        return true;
    }
    let joined = if absolute { path.to_path_buf() } else { workspace.join(path) };
    let mut normal = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                if !normal.pop() {
                    return false;
                }
            }
            Component::CurDir => {}
            other => normal.push(other.as_os_str()),
        }
    }
    normal.starts_with(workspace)
}

#[cfg(test)]
mod tests {
    use super::is_read_only;

    #[test]
    fn default_mode_reads_must_stay_in_the_workspace() {
        let workspace = std::path::Path::new("/work/project");
        for command in ["ls -la src", "cat ./README.md", "git diff --stat", "grep -rn TODO /work/project/src", "ls src/../docs"] {
            assert!(super::reads_only_inside(command, workspace), "{command}");
        }
        for command in ["cat /etc/passwd", "ls ..", "cat ~/.ssh/id_rsa", "cat $HOME/.netrc", "ls ../other", "grep -r x --file=/etc/shadow", "rm -rf src"] {
            assert!(!super::reads_only_inside(command, workspace), "{command}");
        }
    }


    #[test]
    fn inspection_commands_and_pipelines_are_read_only() {
        for command in [
            "ls -la",
            "git status",
            "git log --oneline -5 && git diff --stat",
            "cat package.json | head -20",
            "grep -rn TODO src 2>/dev/null",
            "find . -name '*.rs' | wc -l",
            "Get-ChildItem -Force | Select-Object Name, Mode | Format-Table -AutoSize | Out-String -Width 200",
            "Get-Content -LiteralPath 'package.json' -Encoding UTF8",
            "node --version",
            "git branch -a",
            "echo hello",
        ] {
            assert!(is_read_only(command), "{command}");
        }
    }

    #[test]
    fn writes_executions_and_unknown_programs_need_approval() {
        for command in [
            "rm -rf build",
            "echo hi > notes.txt",
            "cat a >> b",
            "git commit -m x",
            "git branch -D old",
            "git config user.name x",
            "find . -name '*.tmp' -delete",
            "find . -exec rm {} \\;",
            "ls; rm x",
            "ls && npm install",
            "cat $(which sh)",
            "sleep 10 &",
            "Get-ChildItem | Where-Object { Remove-Item $_ }",
            "Remove-Item x",
            "cargo build",
            "sort -o out.txt in.txt",
            "",
        ] {
            assert!(!is_read_only(command), "{command}");
        }
    }
}
