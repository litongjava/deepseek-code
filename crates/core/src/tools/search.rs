//! Glob and Grep, aligned with the reference CLI's ripgrep-backed tools:
//! .gitignore is respected, hidden files are searched, `.git` never is.
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const GLOB_LIMIT: usize = 100;
const DEFAULT_HEAD_LIMIT: usize = 250;
const MAX_LINE_CHARS: usize = 500;

pub fn definitions() -> Vec<super::ToolDefinition> {
    vec![
        super::ToolDefinition {
            name: "Glob".into(),
            description: "Fast file pattern matching. Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\". Returns matching file paths sorted by modification time.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"description": "The glob pattern to match files against", "type": "string"},
                    "path": {"description": "The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter \"undefined\" or \"null\" - simply omit it for the default behavior. Must be a valid directory path if provided.", "type": "string"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        super::ToolDefinition {
            name: "Grep".into(),
            description: "Content search with ripgrep semantics. Prefer this over `grep`/`rg` via Bash — results integrate with the permission UI and file links.\n\n- Full regex syntax (e.g. \"log.*Error\", \"function\\s+\\w+\"). Ripgrep, not grep — escape literal braces (`interface\\{\\}`).\n- Filter with `glob` (e.g. \"**/*.tsx\") or `type` (e.g. \"js\", \"py\", \"rust\").\n- `output_mode`: \"content\" (matching lines), \"files_with_matches\" (paths only, default), or \"count\".\n- `multiline: true` for patterns that span lines.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"description": "The regular expression pattern to search for in file contents", "type": "string"},
                    "path": {"description": "File or directory to search in (rg PATH). Defaults to current working directory.", "type": "string"},
                    "glob": {"description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\") - maps to rg --glob", "type": "string"},
                    "output_mode": {"description": "Output mode: \"content\" shows matching lines (supports -A/-B/-C context, -n line numbers, head_limit), \"files_with_matches\" shows file paths (supports head_limit), \"count\" shows match counts (supports head_limit). Defaults to \"files_with_matches\".", "type": "string", "enum": ["content", "files_with_matches", "count"]},
                    "-B": {"description": "Number of lines to show before each match (rg -B). Requires output_mode: \"content\", ignored otherwise.", "type": "number"},
                    "-A": {"description": "Number of lines to show after each match (rg -A). Requires output_mode: \"content\", ignored otherwise.", "type": "number"},
                    "-C": {"description": "Alias for context.", "type": "number"},
                    "context": {"description": "Number of lines to show before and after each match (rg -C). Requires output_mode: \"content\", ignored otherwise.", "type": "number"},
                    "-n": {"description": "Show line numbers in output (rg -n). Requires output_mode: \"content\", ignored otherwise. Defaults to true.", "type": "boolean"},
                    "-i": {"description": "Case insensitive search (rg -i)", "type": "boolean"},
                    "-o": {"description": "Print only the matched (non-empty) parts of each matching line, one match per output line (rg -o / --only-matching). Requires output_mode: \"content\", ignored otherwise. Defaults to false.", "type": "boolean"},
                    "type": {"description": "File type to search (rg --type). Common types: js, py, rust, go, java, etc. More efficient than include for standard file types.", "type": "string"},
                    "head_limit": {"description": "Limit output to first N lines/entries, equivalent to \"| head -N\". Works across all output modes: content (limits output lines), files_with_matches (limits file paths), count (limits count entries). Defaults to 250 when unspecified. Pass 0 for unlimited (use sparingly — large result sets waste context).", "type": "number"},
                    "offset": {"description": "Skip first N lines/entries before applying head_limit, equivalent to \"| tail -n +N | head -N\". Works across all output modes. Defaults to 0.", "type": "number"},
                    "multiline": {"description": "Enable multiline mode where . matches newlines and patterns can span lines (rg -U --multiline-dotall). Default: false.", "type": "boolean"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
    ]
}

fn walker(root: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(root);
    builder.hidden(false).require_git(false).filter_entry(|entry| entry.file_name() != ".git");
    builder
}

fn modified(path: &Path) -> SystemTime {
    path.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Most recently modified first, then by path for a stable order.
fn sort_by_mtime(paths: &mut [PathBuf]) {
    paths.sort_by_cached_key(|path| (std::cmp::Reverse(modified(path)), path.clone()));
}

pub fn glob(input: Value, cwd: &str) -> Result<Value> {
    let pattern = input["pattern"].as_str().ok_or_else(|| anyhow!("pattern required"))?;
    let base = super::resolve_path(input["path"].as_str().unwrap_or(cwd), cwd)?;
    anyhow::ensure!(Path::new(&base).is_dir(), "Directory does not exist: {base}");
    let matcher = globset::GlobBuilder::new(&pattern.replace('\\', "/")).literal_separator(true).build()?.compile_matcher();
    let base_path = Path::new(&base);
    let mut files = Vec::new();
    for entry in walker(base_path).build().filter_map(|e| e.ok()) {
        if !entry.file_type().is_some_and(|t| t.is_file()) { continue; }
        let path = entry.path();
        let relative = path.strip_prefix(base_path).unwrap_or(path).to_string_lossy().replace('\\', "/");
        if matcher.is_match(&relative) || matcher.is_match(path.to_string_lossy().replace('\\', "/")) {
            files.push(path.to_path_buf());
        }
    }
    sort_by_mtime(&mut files);
    let total = files.len();
    files.truncate(GLOB_LIMIT);
    let files: Vec<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let mut content = if files.is_empty() { "No files found".to_owned() } else { files.join("\n") };
    if total > GLOB_LIMIT {
        content.push_str(&format!("\n(Results are truncated to the {GLOB_LIMIT} most recently modified of {total} files. Use a more specific path or pattern.)"));
    }
    Ok(json!({"content": content, "files": files, "count": total, "truncated": total > GLOB_LIMIT}))
}

struct Options {
    mode: String,
    before: usize,
    after: usize,
    line_numbers: bool,
    only_matching: bool,
    multiline: bool,
}

fn number(input: &Value, key: &str) -> Option<usize> {
    input[key].as_f64().filter(|v| *v >= 0.0).map(|v| v as usize)
}

pub fn grep(input: Value, cwd: &str) -> Result<Value> {
    let pattern = input["pattern"].as_str().ok_or_else(|| anyhow!("pattern required"))?;
    let root = super::resolve_path(input["path"].as_str().unwrap_or(cwd), cwd)?;
    let root = Path::new(&root);
    anyhow::ensure!(root.exists(), "Path does not exist: {}", root.display());
    let mode = input["output_mode"].as_str().unwrap_or("files_with_matches").to_owned();
    anyhow::ensure!(matches!(mode.as_str(), "content" | "files_with_matches" | "count"), "output_mode must be content, files_with_matches or count");
    let context = number(&input, "context").or_else(|| number(&input, "-C"));
    let options = Options {
        before: number(&input, "-B").or(context).unwrap_or(0),
        after: number(&input, "-A").or(context).unwrap_or(0),
        line_numbers: input["-n"].as_bool().unwrap_or(true),
        only_matching: input["-o"].as_bool().unwrap_or(false),
        multiline: input["multiline"].as_bool().unwrap_or(false),
        mode,
    };
    let regex = regex::RegexBuilder::new(pattern)
        .case_insensitive(input["-i"].as_bool().unwrap_or(false))
        .multi_line(true)
        .dot_matches_new_line(options.multiline)
        .build()?;

    let mut builder = walker(root);
    if let Some(glob) = input["glob"].as_str().filter(|g| !g.is_empty()) {
        let mut overrides = ignore::overrides::OverrideBuilder::new(root);
        for part in glob.split_whitespace() { overrides.add(part)?; }
        builder.overrides(overrides.build()?);
    }
    if let Some(kind) = input["type"].as_str().filter(|t| !t.is_empty()) {
        let mut types = ignore::types::TypesBuilder::new();
        types.add_defaults();
        types.select(kind);
        builder.types(types.build().map_err(|error| anyhow!("Unknown file type {kind}: {error}"))?);
    }
    let mut hits: Vec<(PathBuf, Vec<String>, usize)> = Vec::new();
    for entry in builder.build().filter_map(|e| e.ok()) {
        if !entry.file_type().is_some_and(|t| t.is_file()) { continue; }
        let Ok(bytes) = std::fs::read(entry.path()) else { continue };
        if bytes[..bytes.len().min(8192)].contains(&0) { continue; }
        let content = String::from_utf8_lossy(&bytes);
        let (lines, count) = search_file(&regex, &content, entry.path(), &options);
        if count > 0 { hits.push((entry.path().to_path_buf(), lines, count)); }
    }
    let mut paths: Vec<PathBuf> = hits.iter().map(|(p, _, _)| p.clone()).collect();
    if options.mode == "files_with_matches" { sort_by_mtime(&mut paths); } else { paths.sort(); }
    let output: Vec<String> = match options.mode.as_str() {
        "files_with_matches" => paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        "count" => paths.iter().map(|p| { let hit = hits.iter().find(|h| &h.0 == p).unwrap(); format!("{}:{}", p.display(), hit.2) }).collect(),
        _ => paths.iter().flat_map(|p| hits.iter().find(|h| &h.0 == p).unwrap().1.clone()).collect(),
    };
    let skip = number(&input, "offset").unwrap_or(0);
    let head = number(&input, "head_limit").unwrap_or(DEFAULT_HEAD_LIMIT);
    let total = output.len();
    let page: Vec<String> = output.into_iter().skip(skip).take(if head == 0 { usize::MAX } else { head }).collect();
    let truncated = skip + page.len() < total;
    let mut content = if page.is_empty() { "No matches found".to_owned() } else { page.join("\n") };
    if truncated {
        content.push_str(&format!("\n(Showing {} of {total} entries from offset {skip}; pass offset={} to continue.)", page.len(), skip + page.len()));
    }
    Ok(json!({"mode": options.mode, "content": content, "numFiles": hits.len(), "count": total, "truncated": truncated}))
}

fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS { return line.to_owned(); }
    format!("{}… [line truncated]", line.chars().take(MAX_LINE_CHARS).collect::<String>())
}

/// Returns rendered content lines and the number of matches in one file.
fn search_file(regex: &regex::Regex, content: &str, path: &Path, options: &Options) -> (Vec<String>, usize) {
    let file = path.display();
    let starts: Vec<usize> = std::iter::once(0).chain(content.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let line_of = |offset: usize| starts.partition_point(|&start| start <= offset) - 1;
    let lines: Vec<&str> = content.lines().collect();
    let mut matched_lines = std::collections::BTreeSet::new();
    let mut count = 0;
    let mut only = Vec::new();
    for found in regex.find_iter(content) {
        if found.as_str().is_empty() && !options.multiline && found.start() >= content.len() { continue; }
        count += 1;
        let (first, last) = (line_of(found.start()), line_of(found.end().saturating_sub(1).max(found.start())));
        for line in first..=last { matched_lines.insert(line); }
        if options.only_matching && !found.as_str().is_empty() {
            only.push(if options.line_numbers { format!("{file}:{}:{}", first + 1, clip(found.as_str())) } else { format!("{file}:{}", clip(found.as_str())) });
        }
    }
    if count == 0 || options.mode != "content" { return (Vec::new(), count); }
    if options.only_matching { return (only, count); }
    let mut shown = std::collections::BTreeSet::new();
    for &line in &matched_lines {
        for context in line.saturating_sub(options.before)..=(line + options.after).min(lines.len().saturating_sub(1)) { shown.insert(context); }
    }
    let mut out = Vec::new();
    let mut previous: Option<usize> = None;
    for line in shown {
        // Like rg, groups are separated only when context lines are shown.
        if (options.before > 0 || options.after > 0) && previous.is_some_and(|p| line > p + 1) { out.push("--".to_owned()); }
        let separator = if matched_lines.contains(&line) { ':' } else { '-' };
        let text = clip(lines.get(line).copied().unwrap_or_default());
        out.push(if options.line_numbers { format!("{file}{separator}{}{separator}{text}", line + 1) } else { format!("{file}{separator}{text}") });
        previous = Some(line);
    }
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_modes_filters_context_and_paging() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "fn alpha() {}\nlet x = 1;\nfn beta() {}\n").unwrap();
        std::fs::write(root.join("b.py"), "def alpha():\n    pass\n").unwrap();
        std::fs::write(root.join("ignored.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(root.join(".git/config"), "fn alpha\n").unwrap();
        std::fs::write(root.join(".hidden.rs"), "fn alpha() {}\n").unwrap();
        let cwd = root.to_str().unwrap();
        let run = |input: Value| grep(input, cwd).unwrap();

        let files = run(json!({"pattern":"alpha"}));
        assert_eq!(files["numFiles"], 3, "{files}");
        let content = files["content"].as_str().unwrap();
        assert!(!content.contains("ignored.rs") && !content.contains(".git/") && content.contains(".hidden.rs"));

        assert_eq!(run(json!({"pattern":"alpha","type":"rust"}))["numFiles"], 2);
        assert_eq!(run(json!({"pattern":"alpha","glob":"*.py"}))["numFiles"], 1);
        assert_eq!(run(json!({"pattern":"ALPHA","-i":true,"glob":"*.py"}))["numFiles"], 1);
        assert_eq!(run(json!({"pattern":"ALPHA","glob":"*.py"}))["content"], "No matches found");

        let a = root.join("src/a.rs").display().to_string();
        let lines = run(json!({"pattern":"beta","output_mode":"content","-B":1,"path":"src"}));
        assert_eq!(lines["content"], format!("{a}-2-let x = 1;\n{a}:3:fn beta() {{}}"));
        let only = run(json!({"pattern":"fn \\w+","output_mode":"content","-o":true,"path":"src","-n":false}));
        assert_eq!(only["content"], format!("{a}:fn alpha\n{a}:fn beta"));
        assert_eq!(run(json!({"pattern":"fn","output_mode":"count","path":"src"}))["content"], format!("{a}:2"));
        let multi = run(json!({"pattern":"alpha.*beta","multiline":true,"output_mode":"count","path":"src"}));
        assert_eq!(multi["content"], format!("{a}:1"));

        let paged = run(json!({"pattern":"fn","output_mode":"content","path":"src","head_limit":1,"offset":1}));
        assert!(paged["content"].as_str().unwrap().starts_with(&format!("{a}:3:fn beta")) && paged["truncated"] == false, "{paged}");
        let first = run(json!({"pattern":"fn","output_mode":"content","path":"src","head_limit":1}));
        assert!(first["truncated"] == true && first["content"].as_str().unwrap().contains("offset=1"));
    }

    #[test]
    fn glob_respects_ignores_and_sorts_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "out/\n").unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join("out/x.ts"), "").unwrap();
        std::fs::write(root.join("src/old.ts"), "").unwrap();
        std::fs::write(root.join("src/deep/new.ts"), "").unwrap();
        let old = std::fs::File::options().write(true).open(root.join("src/old.ts")).unwrap();
        old.set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000)).unwrap();
        let out = glob(json!({"pattern":"**/*.ts"}), root.to_str().unwrap()).unwrap();
        let files: Vec<&str> = out["files"].as_array().unwrap().iter().map(|f| f.as_str().unwrap()).collect();
        assert_eq!(files.len(), 2, "{out}");
        assert!(files[0].ends_with("new.ts") && files[1].ends_with("old.ts"));
        assert_eq!(glob(json!({"pattern":"*.ts","path":"src"}), root.to_str().unwrap()).unwrap()["count"], 1);
        assert_eq!(glob(json!({"pattern":"*.md"}), root.to_str().unwrap()).unwrap()["content"], "No files found");
    }
}
