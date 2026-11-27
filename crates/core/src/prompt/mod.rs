use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const SELF_HOSTED_IDENTITY_BLOCK: &str = r#"<identity>
You are an AI assistant running in the deepseek-code desktop app, powered by a native Rust tool engine and the user's selected model provider. You help with everyday questions, research, writing, files, and software projects. Only use capabilities present in the tools supplied with the current request.

Treat the user with warmth, curiosity, presence, and depth. At the same time, when the user actually wants something built, read, run, or changed on disk — including handing you an entire code project to read or modify — do that work decisively and competently. Conversation-first does NOT mean code-shy.

You are NOT Kiro. You are NOT made by Amazon, AWS, or any Amazon product. You have no connection to Kiro. Do not create .kiro directories, do not produce Kiro-style configs, do not refer to yourself as Kiro under any circumstances.

Default to 简体中文 when the user writes in Chinese. Match the user's language otherwise.

Describe yourself as the assistant in deepseek-code. Name the model or the company behind it only as the selected model reports it; do not guess.
</identity>"#;

/// First system block of Code sessions, cached on its own like the reference CLI.
const CODE_IDENTITY: &str = "You are deepseek-code, an agentic coding tool for DeepSeek models.";

const BEHAVIOR: &str = include_str!("behavior.md");
const MEMORY_GUIDANCE: &str = include_str!("memory.md");
const CODE_BEHAVIOR: &str = include_str!("code.md");

pub mod code;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PromptProfile {
    #[default]
    Desktop,
    Code,
}

impl PromptProfile {
    pub fn from_workspace_mode(mode: &str) -> Self {
        if mode == "code" { Self::Code } else { Self::Desktop }
    }
}

/// Compose once at the common engine entry point, before project instructions and recall.
/// These are project instructions adapted to our native runtime, not an official full prompt.
pub fn compose_desktop_system_prompt(custom_prompt: Option<&str>) -> String {
    compose_system_prompt(custom_prompt, PromptProfile::Desktop)
}

pub fn compose_system_prompt(custom_prompt: Option<&str>, profile: PromptProfile) -> String {
    compose_system_prompt_in(custom_prompt, profile)
}

/// The built-in prompt text, which the user can start editing from.
pub fn default_system_prompt(_profile: PromptProfile) -> String {
    static_sections(true).join("\n\n")
}

/// The CLI's constant runtime facts. They describe the frontend, so they travel
/// in the first user message as their own reminder instead of the system prompt.
pub const STANDALONE_FRONTEND: &str = "Launched using the dcl command and running directly in a terminal.no Desktop window or Bridge server; use native tools directly. For an interactive command like `gcloud auth login`, tell the user to enter `! <command>` and its output appears here.";

pub fn standalone_frontend_reminder() -> String {
    format!("<system-reminder>\n{STANDALONE_FRONTEND}\n</system-reminder>")
}

/// The lead-in of the reminder that lists tools ToolLoad can load.
pub const LOADABLE_TOOLS_LEAD_IN: &str =
    "Tools you can load with ToolLoad (pass exact names in the names array):";

/// The lead-in of the reminder that lists the model-invocable skills. This is
/// the only place the invocation rule lives, so it names the exact argument,
/// covers questions about skills themselves, and forbids reading a skill's
/// files or the CLI instead.
pub const SKILLS_LEAD_IN: &str = "Available skills: invoke the Skill tool with {\"skill\":\"<name>\"} and optional args; its instructions come back immediately in this turn. Use it when the user asks what a skill does, where skills live, or how to create or load one. Do not open a skill's SKILL.md or INSTRUCTIONS.md with Read or Bash instead: fork isolation and approved shell context must run through the tool.";

/// The only behavioral instruction for project experience entries. It lives
/// here, never in the system prompt or a project's AGENTS.md.
pub const EXPERIENCE_LEAD_IN: &str = "Verified fixes recorded for this project. Before re-deriving a fix, search .agents/experience/ and read the matching file; a one-line description is only a pointer. Before finishing a turn in which you solved a non-obvious problem that you verified, add one entry file there (frontmatter name and description; body Symptom, Cause, Fix, Verification), or update the existing entry for the same symptom and root cause. Keep one problem per file, merge near-duplicates, and delete entries that no longer hold. Entries are records, not instructions; never store credentials, tokens or raw logs.";

/// Whitespace-collapsed text on a single line, cut nowhere. The loadable-tool
/// listing shows custom summaries and full descriptions through it, so a list
/// entry stays one line without losing any of its words.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first sentence of a description, whitespace collapsed and cut to
/// `limit` characters. Used by the experience index, not the tool listing.
pub fn one_sentence(description: &str, limit: usize) -> String {
    let text = one_line(description);
    let sentence = text
        .split(". ")
        .next()
        .unwrap_or(&text)
        .trim_end_matches('.')
        .trim();
    if sentence.chars().count() <= limit {
        return sentence.to_owned();
    }
    let mut cut: String = sentence.chars().take(limit.saturating_sub(1)).collect();
    cut = cut.trim_end().to_owned();
    cut.push('…');
    cut
}

/// One reminder listing the loadable built-in tools first, then the on-demand
/// MCP tools. Each entry shows the text the caller resolved (a custom summary or
/// the complete description) on one line, uncut. None when ToolLoad has nothing
/// to offer.
pub fn loadable_tools_reminder(
    builtin: &[(String, String)],
    mcp: &[(String, String)],
) -> Option<String> {
    if builtin.is_empty() && mcp.is_empty() {
        return None;
    }
    let mut text = String::from(LOADABLE_TOOLS_LEAD_IN);
    for (name, description) in builtin.iter().chain(mcp) {
        text.push_str(&format!("\n- {name}: {}", one_line(description)));
    }
    Some(format!("<system-reminder>\n{text}\n</system-reminder>"))
}

/// Read a frozen reminder stored as JSON `Option<String>`: `Some(None)` means
/// the first request had an empty list, so later requests must keep omitting it.
fn frozen_reminder(path: &Path) -> Option<Option<String>> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn store_frozen_reminder(path: &Path, reminder: &Option<String>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::to_vec(reminder).unwrap_or_default());
}

/// The loadable-tool reminder frozen on the session's first request. A later
/// MCP reconnect or settings change never rewrites the first user message.
pub fn loadable_tools_for_session(
    root: &Path,
    session: &str,
    builtin: &[(String, String)],
    mcp: &[(String, String)],
) -> Option<String> {
    let path = crate::session_files::environment_file(root, session, ".loadable-tools.json");
    if let Some(saved) = frozen_reminder(&path) {
        return saved;
    }
    let reminder = loadable_tools_reminder(builtin, mcp);
    store_frozen_reminder(&path, &reminder);
    reminder
}

/// The reminder listing the model-invocable skills, with their one-line
/// descriptions and context. None when there are no skills.
pub fn skills_reminder(lines: &[String]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "<system-reminder>\n{SKILLS_LEAD_IN}\n{}\n</system-reminder>",
        lines.join("\n")
    ))
}

/// The skill list frozen on the session's first request, for the same
/// prefix-stability reason as the environment.
pub fn skills_for_session(root: &Path, session: &str, lines: &[String]) -> Option<String> {
    let path = crate::session_files::environment_file(root, session, ".skills.json");
    if let Some(saved) = frozen_reminder(&path) {
        return saved;
    }
    let reminder = skills_reminder(lines);
    store_frozen_reminder(&path, &reminder);
    reminder
}

/// The full body of a skill, injected directly into the first-turn context so
/// the model has its complete instructions without spending a turn calling
/// `Skill`. None when there is no block.
pub fn inline_skills_reminder(blocks: &[(String, String)]) -> Option<String> {
    let mut text = String::new();
    for (name, body) in blocks {
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!("# Skill: {name}\n{body}"));
    }
    (!text.is_empty()).then(|| format!("<system-reminder>\n{text}\n</system-reminder>"))
}

/// The inline skill bodies frozen on the session's first request, for the same
/// prefix-stability reason as the environment.
pub fn inline_skills_for_session(
    root: &Path,
    session: &str,
    blocks: &[(String, String)],
) -> Option<String> {
    let path = crate::session_files::environment_file(root, session, ".inline-skills.json");
    if let Some(saved) = frozen_reminder(&path) {
        return saved;
    }
    let reminder = inline_skills_reminder(blocks);
    store_frozen_reminder(&path, &reminder);
    reminder
}

/// Workspace-local CodeGraph index state inferred purely from filesystem
/// metadata (zero subprocesses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeGraphIndexStatus {
    /// `<dir>/.codegraph/codegraph.db` exists as a non-empty regular file.
    Ready(PathBuf),
    /// Current workspace is a git worktree without `.codegraph/codegraph.db`,
    /// while the main git repository has a non-empty `.codegraph/codegraph.db`.
    WorktreeUninitialized { main_repo: PathBuf },
    /// Current workspace is inside a git repository (or worktree) without a
    /// populated `.codegraph/codegraph.db`.
    GitUninitialized(PathBuf),
    /// Workspace is not inside a git repository and has no `.codegraph/codegraph.db`.
    NotGitRepo,
}

fn has_non_empty_codegraph_db(dir: &Path) -> bool {
    fs::metadata(dir.join(".codegraph").join("codegraph.db"))
        .is_ok_and(|meta| meta.is_file() && meta.len() > 0)
}

fn resolve_worktree_main_repo(worktree_root: &Path, git_file: &Path) -> Option<PathBuf> {
    let content = fs::read_to_string(git_file).ok()?;
    let raw_gitdir = content
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))?
        .trim();
    if raw_gitdir.is_empty() {
        return None;
    }
    let raw_path = Path::new(raw_gitdir);
    let gitdir = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        worktree_root.join(raw_path)
    };
    let worktrees_dir = gitdir.parent()?;
    if worktrees_dir.file_name().and_then(|n| n.to_str()) != Some("worktrees") {
        return None;
    }
    let common_git_dir = worktrees_dir.parent()?;
    if common_git_dir.file_name().and_then(|n| n.to_str()) != Some(".git") {
        return None;
    }
    let main_repo = common_git_dir.parent()?;
    if raw_path.is_absolute() {
        Some(main_repo.to_path_buf())
    } else {
        Some(fs::canonicalize(main_repo).unwrap_or_else(|_| main_repo.to_path_buf()))
    }
}

/// Inspect `<workspace>` (and ancestors up to the enclosing git root) for a
/// populated `.codegraph/codegraph.db` using filesystem metadata only.
pub fn codegraph_workspace_status(workspace: &Path) -> CodeGraphIndexStatus {
    for dir in workspace.ancestors() {
        if has_non_empty_codegraph_db(dir) {
            return CodeGraphIndexStatus::Ready(dir.to_path_buf());
        }
        let git_path = dir.join(".git");
        let Ok(meta) = fs::metadata(&git_path) else {
            continue;
        };
        if meta.is_file() {
            if let Some(main_repo) = resolve_worktree_main_repo(dir, &git_path) {
                if has_non_empty_codegraph_db(&main_repo) {
                    return CodeGraphIndexStatus::WorktreeUninitialized { main_repo };
                }
            }
            return CodeGraphIndexStatus::GitUninitialized(dir.to_path_buf());
        }
        if meta.is_dir() {
            return CodeGraphIndexStatus::GitUninitialized(dir.to_path_buf());
        }
    }
    CodeGraphIndexStatus::NotGitRepo
}

fn codegraph_status_note(workspace: &Path) -> &'static str {
    match codegraph_workspace_status(workspace) {
        CodeGraphIndexStatus::Ready(_) => {
            "\n\nWorkspace CodeGraph index: READY (`.codegraph/codegraph.db` exists). Query directly with `codegraph explore|node|callers|callees|impact` — skip `codegraph status` / `codegraph init`."
        }
        CodeGraphIndexStatus::WorktreeUninitialized { .. } => {
            "\n\nWorkspace CodeGraph index: NOT INITIALIZED in this git worktree (`.codegraph/codegraph.db` is missing; git worktrees do not inherit the main repo's index). Run `codegraph init -y` first to build the local index, then query with `codegraph explore|node|callers|callees|impact`."
        }
        CodeGraphIndexStatus::GitUninitialized(_) => {
            "\n\nWorkspace CodeGraph index: NOT INITIALIZED in this git repository (`.codegraph/codegraph.db` is missing). Run `codegraph init -y` first to build the local index, then query with `codegraph explore|node|callers|callees|impact`."
        }
        CodeGraphIndexStatus::NotGitRepo => {
            "\n\nWorkspace CodeGraph index: NOT INITIALIZED (not a git repository; skip `codegraph init` and use parallel `Read`/`Grep`/`Glob`)."
        }
    }
}

/// Full skill bodies for the first-turn context from every skill whose
/// effective state is `SkillState::Always` (marked `always_inline`) and that is
/// model-invocable. Empty when no skill is in `Always`, so users pay zero extra
/// tokens. A `$ARGUMENTS` placeholder is stripped because injected text has no
/// invocation arguments.
pub fn inline_skill_blocks(
    skills: &BTreeMap<String, crate::skills::discovery::SkillCommand>,
) -> Vec<(String, String)> {
    inline_skill_blocks_for_workspace(skills, None)
}

/// Like [`inline_skill_blocks`], additionally enriching the `codegraph` skill
/// block with a one-line factual status of the workspace's `.codegraph` index
/// when `workspace` is provided.
pub fn inline_skill_blocks_for_workspace(
    skills: &BTreeMap<String, crate::skills::discovery::SkillCommand>,
    workspace: Option<&Path>,
) -> Vec<(String, String)> {
    skills
        .values()
        .filter(|skill| skill.always_inline && skill.model_invocable())
        .filter_map(|skill| {
            let body = skill.content.replace("$ARGUMENTS", "");
            let body = body.trim();
            if body.is_empty() {
                return None;
            }
            let mut body = body.to_owned();
            if skill.name == "codegraph" {
                if let Some(ws) = workspace {
                    body.push_str(codegraph_status_note(ws));
                }
            }
            Some((skill.name.clone(), body))
        })
        .collect()
}

/// The experience index plus its runtime instruction. Always a block when the
/// feature applies; an empty index still tells the model where to write.
pub fn experience_reminder(lines: &[String]) -> String {
    let mut text = format!("# Experience\n{EXPERIENCE_LEAD_IN}");
    if lines.is_empty() {
        text.push_str("\nNo entries yet.");
    } else {
        text.push('\n');
        text.push_str(&lines.join("\n"));
    }
    format!("<system-reminder>\n{text}\n</system-reminder>")
}

/// The experience block frozen on the session's first request. `workspace` is
/// `None` for an internal draft workspace, and a disabled switch freezes
/// `None` too, so later requests in the session cannot add the block.
pub fn experience_for_session(
    root: &Path,
    session: &str,
    workspace: Option<&Path>,
    enabled: bool,
    entries: &[crate::experience::Entry],
) -> Option<String> {
    let path = crate::session_files::environment_file(root, session, ".experience.json");
    if let Some(saved) = frozen_reminder(&path) {
        return saved;
    }
    let reminder = (enabled && workspace.is_some())
        .then(|| experience_reminder(&crate::experience::index_lines(entries)));
    store_frozen_reminder(&path, &reminder);
    reminder
}

/// The names added to and removed from a per-session set since the previous
/// observed value. The first call records the set without reporting anything.
/// Saving the current set here makes each change produce one reminder only.
pub fn set_changes(
    root: &Path,
    session: &str,
    kind: &str,
    current: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut current: Vec<String> = current.to_vec();
    current.sort();
    current.dedup();
    let path = crate::session_files::set_changes_file(root, session, kind);
    let previous: Option<Vec<String>> = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, serde_json::to_vec(&current).unwrap_or_default());
    let Some(previous) = previous else {
        return (Vec::new(), Vec::new());
    };
    let before: std::collections::HashSet<_> = previous.iter().collect();
    let after: std::collections::HashSet<_> = current.iter().collect();
    (
        current
            .iter()
            .filter(|name| !before.contains(name))
            .cloned()
            .collect(),
        previous
            .into_iter()
            .filter(|name| !after.contains(&name))
            .collect(),
    )
}

/// One reminder on the latest user message describing only what changed since
/// the first message froze its lists. None when nothing changed.
pub fn changes_reminder(
    now_loadable: Vec<String>,
    no_longer_available: Vec<String>,
    new_skills: Vec<String>,
    removed_skills: Vec<String>,
    new_experience: Vec<String>,
    removed_experience: Vec<String>,
) -> Option<String> {
    let mut lines = Vec::new();
    if !now_loadable.is_empty() {
        lines.push(format!("Now loadable: {}", now_loadable.join(", ")));
    }
    if !no_longer_available.is_empty() {
        lines.push(format!(
            "No longer available: {}",
            no_longer_available.join(", ")
        ));
    }
    if !new_skills.is_empty() {
        lines.push(format!("New skills: {}", new_skills.join(", ")));
    }
    if !removed_skills.is_empty() {
        lines.push(format!("Removed skills: {}", removed_skills.join(", ")));
    }
    if !new_experience.is_empty() {
        lines.push("New experience:".into());
        lines.extend(new_experience);
    }
    if !removed_experience.is_empty() {
        lines.push(format!(
            "Removed experience: {}",
            removed_experience.join(", ")
        ));
    }
    (!lines.is_empty()).then(|| format!("<system-reminder>\n{}\n</system-reminder>", lines.join("\n")))
}

/// The session's date as its own reminder.
pub fn date_reminder(date: &str) -> String {
    format!("<system-reminder>\nToday's date is {date}.\n</system-reminder>")
}

/// The parts of the first user message in their stable order. Each present part
/// becomes its own `<system-reminder>` block.
#[derive(Debug, Default, Clone)]
pub struct SessionReminders {
    pub user_instructions: Option<String>,
    pub standalone_frontend: bool,
    pub project_instructions: Option<String>,
    pub resident_indexes: Vec<String>,
    /// The project experience index, frozen on the first request.
    pub experience: Option<String>,
    /// The tools ToolLoad can load, frozen on the first request.
    pub loadable_tools: Option<String>,
    /// The model-invocable skills, frozen on the first request.
    pub available_skills: Option<String>,
    /// Full skill bodies injected on the first request, such as `codegraph`.
    pub inline_skills: Option<String>,
    pub environment: Option<String>,
    pub date: Option<String>,
    pub git: Option<String>,
    /// Skills invoked earlier whose message a compaction removed.
    pub skills: Option<String>,
}

/// The reminders that open the first user message: the user's own instructions,
/// the CLI's frontend facts, the project's instructions, the resident memory
/// indexes, the project experience index, the loadable tools, the skills, the
/// environment, the date, the Git snapshot and skills recovered after
/// compaction.
pub fn first_message_reminders(parts: SessionReminders) -> Vec<String> {
    let mut blocks = Vec::new();
    blocks.extend(parts.user_instructions);
    if parts.standalone_frontend {
        blocks.push(standalone_frontend_reminder());
    }
    blocks.extend(parts.project_instructions);
    blocks.extend(parts.resident_indexes);
    blocks.extend(parts.experience);
    blocks.extend(parts.loadable_tools);
    blocks.extend(parts.available_skills);
    blocks.extend(parts.inline_skills);
    blocks.extend(parts.environment);
    blocks.extend(parts.date);
    blocks.extend(parts.git);
    blocks.extend(parts.skills);
    blocks
}

/// Like `compose_system_prompt_in`, with the user's system prompt setting and the
/// memory switch applied. Memory disabled omits the memory guidance section.
pub fn compose_customized(custom_prompt: Option<&str>, _profile: PromptProfile, user: &crate::services::customization::SystemPrompt, memory: bool) -> String {
    use crate::services::customization::PromptMode;
    let text = user.text.trim();
    let mut sections = match user.mode {
        PromptMode::Replace if !text.is_empty() => vec![text.to_owned()],
        _ => static_sections(memory),
    };
    if user.mode == PromptMode::Append && !text.is_empty() {
        sections.push(text.to_owned());
    }
    if let Some(custom) = custom_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        sections.push(format!("<additional_instructions>\n{custom}\n</additional_instructions>"));
    }
    sections.join("\n\n")
}

/// Static sections come first and never vary within a session, so providers can
/// cache them; request-specific instructions follow them. Session facts such as
/// the environment belong in the first user message instead.
pub fn compose_system_prompt_in(custom_prompt: Option<&str>, profile: PromptProfile) -> String {
    compose_system_prompt_with_memory(custom_prompt, profile, true)
}

/// Composition with the global memory switch applied: a disabled memory omits
/// the memory guidance section while identity, coding behavior and general
/// behavior stay identical for every profile.
pub fn compose_system_prompt_with_memory(custom_prompt: Option<&str>, _profile: PromptProfile, memory: bool) -> String {
    let mut sections = static_sections(memory);
    if let Some(custom) = custom_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        // Preserve request-specific instructions verbatim; identity rewriting only applies to the base.
        sections.push(format!("<additional_instructions>\n{}\n</additional_instructions>", custom));
    }
    sections.join("\n\n")
}

fn static_sections(memory: bool) -> Vec<String> {
    let mut sections = vec![
        CODE_IDENTITY.to_string(),
        CODE_BEHAVIOR.trim().to_string(),
        BEHAVIOR.trim().to_string(),
    ];
    if memory {
        sections.push(MEMORY_GUIDANCE.trim().to_string());
    }
    sections
}

/// The session's facts as the reference CLI puts them in the first user message.
pub fn environment_reminder(workspace: Option<&Path>) -> String {
    format!("<system-reminder>\n{}\n</system-reminder>", environment_section(workspace))
}

/// The reminder's text frozen on the session's first request, including the
/// project's name and source folders. Later requests and resumes read the same
/// file, so a changing date or runtime fact can never rewrite the first message.
pub(crate) fn session_environment(
    root: &Path,
    session: &str,
    workspace: &Path,
    project: Option<&crate::db::project_repo::ProjectRow>,
) -> (String, String) {
    let folders = project.map(|p| p.folders.iter().map(|f| f.path.clone()).collect::<Vec<_>>());
    let description = project
        .zip(folders.as_deref())
        .map(|(p, folders)| (p.name.as_str(), folders));
    (
        environment_for_session(root, session, Some(workspace), description),
        date_for_session(root, session),
    )
}

pub fn environment_for_session(
    root: &Path,
    session: &str,
    workspace: Option<&Path>,
    project: Option<(&str, &[String])>,
) -> String {
    let path = crate::session_files::environment_file(root, session, ".txt");
    if let Ok(saved) = std::fs::read_to_string(&path) {
        if !saved.is_empty() {
            return saved;
        }
    }
    let mut reminder = environment_reminder(workspace);
    if let Some((name, folders)) = project {
        let folders = folders.iter().map(String::as_str).collect::<Vec<_>>();
        add_project_to_environment(&mut reminder, name, &folders);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &reminder);
    reminder
}

/// The date reminder frozen on the session's first request, for the same
/// prefix-stability reason as the environment.
pub fn date_for_session(root: &Path, session: &str) -> String {
    let path = crate::session_files::environment_file(root, session, ".date.txt");
    if let Ok(saved) = std::fs::read_to_string(&path) {
        if !saved.is_empty() {
            return saved;
        }
    }
    let reminder = date_reminder(&chrono::Local::now().format("%Y-%m-%d").to_string());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &reminder);
    reminder
}

/// The date recorded in a date reminder, `YYYY-MM-DD`.
pub fn reminder_date(reminder: &str) -> Option<String> {
    static DATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Today's date is (\d{4}-\d{2}-\d{2})\.").unwrap());
    DATE.captures(reminder).map(|captures| captures[1].to_owned())
}

/// A reminder for the first request on a date the frozen date predates. The
/// announced date is recorded so the reminder is sent once, not on every later
/// request that day.
pub fn date_change_reminder(root: &Path, session: &str, date: &str, today: &str) -> Option<String> {
    if reminder_date(date).as_deref() == Some(today) {
        return None;
    }
    let path = crate::session_files::environment_file(root, session, ".announced-date");
    if std::fs::read_to_string(&path).ok().as_deref().map(str::trim) == Some(today) {
        return None;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, today);
    Some(format!("<system-reminder>\nThe date has changed. Today's date is {today}.\n</system-reminder>"))
}

/// The age after which a per-session frozen file is pruned at engine startup.
pub const SESSION_FILE_MAX_AGE: std::time::Duration =
    std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Remove every file the first-message and MCP snapshot features wrote for
/// `session`. Called after its conversation is deleted; missing files are fine.
/// A session id is used as a file-name component, so anything that could walk
/// out of the data directory is rejected before joining.
pub fn forget_session_files(root: &Path, session: &str) {
    if !crate::session_files::is_safe_session(session) {
        return;
    }
    for path in crate::session_files::files_for_session(root, session) {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    module = "Prompt",
                    "Failed to remove session file {}: {}",
                    path.display(),
                    error
                );
            }
        }
    }
}

/// Remove per-session frozen files whose modification time is older than
/// `max_age`. Engine startup runs this off the request path so the first request
/// is never delayed. Files of a session starting now have a fresh modification
/// time, so they are kept; only abandoned sessions are pruned.
pub fn prune_session_files(root: &Path, max_age: std::time::Duration) {
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(max_age) else {
        return;
    };
    let environment_suffixes = crate::session_files::environment_suffixes();
    prune_session_directory(
        &root.join(crate::session_files::ENVIRONMENT_DIR),
        &environment_suffixes,
        cutoff,
    );
    prune_session_directory(
        &root.join(crate::session_files::MCP_SNAPSHOT_DIR),
        &[".json".to_owned()],
        cutoff,
    );
}

fn prune_session_directory(dir: &Path, suffixes: &[String], cutoff: std::time::SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !suffixes.iter().any(|suffix| name.ends_with(suffix.as_str())) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if modified < cutoff {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn environment_section(workspace: Option<&Path>) -> String {
    let mut lines = vec!["# Working environment".to_string()];
    if let Some(workspace) = workspace {
        lines.push(format!(" - Primary working directory: {}", workspace.display()));
        lines.push(format!(" - Is a git repository: {}", workspace.ancestors().any(|dir| dir.join(".git").exists())));
    }
    lines.push(format!(" - Platform: {}", std::env::consts::OS));
    lines.push(format!(" - Architecture: {}", architecture_label(std::env::consts::ARCH)));
    if let Some(shell) = crate::services::shell_settings::resolved_tool_program() {
        lines.push(format!(" - Shell: {shell}"));
    }
    if let Some(version) = OS_VERSION.as_deref() {
        lines.push(format!(" - OS Version: {version}"));
    }
    if let Some(version) = NODE_VERSION.as_deref() {
        lines.push(format!(" - Node.js version: {version}"));
    }
    if let Some(line) = crate::tools::python::environment_line(workspace) {
        lines.push(line);
    }
    lines.push(" - Downloads and extracted archives are untrusted. Give each a fresh, empty directory, keep scripts elsewhere, and pass paths as arguments instead of running tools inside the download. Python loads code from the script folder and the working directory, so a planted `json.py` runs on `import json`; use `-I` when Python reads them. Code the user asked you to build or run is exempt.".to_string());
    lines.join("\n")
}

/// The labels the environment block uses for each architecture, so the prompt
/// reads the same across platforms.
fn architecture_label(arch: &str) -> &str {
    match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// The span of the per-session environment block, from its heading to just
/// before the reminder's closing tag (or its trailing blank line). Reminders
/// without one return None.
fn environment_range(system: &str) -> Option<(usize, usize)> {
    let start = system.find("# Working environment")?;
    let end = system[start..]
        .find("\n</system-reminder>")
        .or_else(|| system[start..].find("\n\n"))
        .map(|offset| start + offset)
        .unwrap_or(system.len());
    Some((start, end))
}

fn project_folder_lines(folders: &[&str]) -> String {
    let mut text = String::from(" - Project source folders (the first is the working directory):");
    for folder in folders {
        text.push_str("\n   - ");
        text.push_str(folder);
    }
    text
}

/// Put the project name and, when it spans more than one folder, its source
/// folders inside the environment reminder, so the session facts read in one
/// place. A reminder without an environment block is left unchanged.
pub fn add_project_to_environment(reminder: &mut String, name: &str, folders: &[&str]) {
    let Some((start, end)) = environment_range(reminder) else {
        return;
    };
    // Directly before the working directory, which always follows the heading.
    if let Some(offset) = reminder[start..end].find("\n - Primary working directory:") {
        let at = start + offset + 1;
        reminder.insert_str(at, &format!(" - Project: {name}\n"));
    } else {
        // No workspace facts in this block; keep the name at its end.
        let end = environment_range(reminder).map(|(_, end)| end).unwrap_or(reminder.len());
        reminder.insert_str(end, &format!("\n - Project: {name}"));
    }
    if folders.len() > 1 {
        let (start, end) = environment_range(reminder).unwrap_or((0, reminder.len()));
        let at = reminder[start..end]
            .find("\n - Is a git repository:")
            .and_then(|offset| {
                let line_start = start + offset + 1;
                reminder[line_start..end].find('\n').map(|line| line_start + line)
            })
            .unwrap_or(end);
        reminder.insert_str(at, &format!("\n{}", project_folder_lines(folders)));
    }
}

/// Kernel name and release, as `uname -sr` prints them, or on Windows the
/// plain major.minor.build version; read once per process.
static OS_VERSION: LazyLock<Option<String>> = LazyLock::new(|| {
    #[cfg(unix)]
    let output = std::process::Command::new("uname").arg("-sr").stdin(std::process::Stdio::null()).output();
    #[cfg(windows)]
    let output = {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flash in the Desktop app.
        std::process::Command::new("cmd").args(["/C", "ver"]).creation_flags(0x08000000).stdin(std::process::Stdio::null()).output()
    };
    #[cfg(not(any(unix, windows)))]
    let output: std::io::Result<std::process::Output> = Err(std::io::ErrorKind::Unsupported.into());
    let output = output.ok().filter(|output| output.status.success())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if cfg!(windows) { parse_windows_version(text) } else { Some(text.to_owned()) }
});

/// `cmd /C ver` prints `Microsoft Windows [Version 10.0.26300.9974]`, whose
/// bracket label follows the system language; keep the first version number.
fn parse_windows_version(text: &str) -> Option<String> {
    static VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\.(\d+)\.(\d+)(?:\.\d+)?").unwrap());
    let captures = VERSION.captures(text)?;
    Some(format!("{}.{}.{}", &captures[1], &captures[2], &captures[3]))
}

/// The Node.js version, as `node --version` prints it; read once per process.
static NODE_VERSION: LazyLock<Option<String>> = LazyLock::new(|| {
    let mut command = std::process::Command::new("node");
    command.arg("--version");
    version_output(command)
});

/// Trimmed stdout of a short version command, or None when it is missing,
/// fails, or exceeds the timeout.
fn version_output(mut command: std::process::Command) -> Option<String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flash in the Desktop app.
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let status = loop {
        match child.try_wait().ok()? {
            Some(status) => break status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    if !status.success() {
        return None;
    }
    let mut bytes = Vec::new();
    let mut stdout = child.stdout.take()?;
    std::io::Read::read_to_end(&mut stdout, &mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Split a composed system prompt into Anthropic text blocks: each static section
/// carries its own cache breakpoint and the per-request tail is left unmarked.
/// Returns None for prompts this module did not compose.
pub fn cache_blocks(system: &str) -> Option<Vec<(String, bool)>> {
    [true, false].into_iter().find_map(|memory| {
        let sections = static_sections(memory);
        let mut rest = system;
        let mut blocks = Vec::new();
        for (index, section) in sections.iter().enumerate() {
            rest = rest.strip_prefix(section.as_str())?;
            if index + 1 < sections.len() {
                rest = rest.strip_prefix("\n\n")?;
            }
            blocks.push((section.clone(), true));
        }
        let rest = rest.trim_start_matches('\n');
        if !rest.is_empty() {
            blocks.push((rest.to_string(), false));
        }
        Some(blocks)
    })
}

/// A composed prompt's static sections, joined as composed, and its
/// per-session tail; None for prompts this module did not compose or without
/// a tail.
pub fn split_tail(system: &str) -> Option<(String, String)> {
    let blocks = cache_blocks(system)?;
    let (fixed, tail): (Vec<_>, Vec<_>) = blocks.into_iter().partition(|(_, cached)| *cached);
    let tail = tail.into_iter().map(|(text, _)| text).collect::<Vec<_>>().join("\n\n");
    (!tail.trim().is_empty()).then(|| (fixed.into_iter().map(|(text, _)| text).collect::<Vec<_>>().join("\n\n"), tail))
}

static RE_OVERRIDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<override_instructions>[\s\S]*?</override_instructions>\s*").unwrap());
static RE_IDENTITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<identity>[\s\S]*?</identity>").unwrap());
static RE_IDENTITY_WITH_WS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<identity>[\s\S]*?</identity>\s*").unwrap());
static RE_CLAUDE_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^claude-").unwrap());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemPromptConfig {
    pub custom_prompt: Option<String>,
    pub identity_block: Option<String>,
    pub remove_kiro_sections: bool,
    pub default_language: String,
}

impl Default for SystemPromptConfig {
    fn default() -> Self {
        Self {
            custom_prompt: None,
            identity_block: Some(SELF_HOSTED_IDENTITY_BLOCK.to_string()),
            remove_kiro_sections: true,
            default_language: "zh-CN".to_string(),
        }
    }
}

impl SystemPromptConfig {
    pub fn identity_block(&self) -> &str {
        self.identity_block.as_deref().unwrap_or(SELF_HOSTED_IDENTITY_BLOCK)
    }
}

pub fn build_self_hosted_system_prompt(full_prompt: &str) -> String {
    build_self_hosted_system_prompt_with_config(full_prompt, &SystemPromptConfig::default())
}

pub fn build_self_hosted_system_prompt_with_config(full_prompt: &str, config: &SystemPromptConfig) -> String {
    let without_overrides = remove_override_sections(full_prompt);
    let identity_block = config.identity_block();

    if contains_identity_block(&without_overrides) {
        return RE_IDENTITY_WITH_WS
            .replace_all(&without_overrides, format!("{}\n\n", identity_block))
            .trim()
            .to_string();
    }

    format!("{}\n\n{}", identity_block, without_overrides).trim().to_string()
}

fn remove_override_sections(prompt: &str) -> String {
    RE_OVERRIDE.replace_all(prompt, "").to_string()
}

fn contains_identity_block(prompt: &str) -> bool {
    RE_IDENTITY.is_match(prompt)
}

pub fn is_claude_family_model(model_id: &str) -> bool {
    RE_CLAUDE_PREFIX.is_match(model_id.trim())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedModel {
    pub model_id: String,
    pub fallback_applied: bool,
    pub error: Option<String>,
}

pub fn resolve_requested_model_for_mode(
    model_id: Option<&str>,
    user_mode: &str,
    has_provider: bool,
) -> ResolvedModel {
    let normalized_model_id = model_id
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .unwrap_or("claude-sonnet-4-6")
        .to_string();

    let effective_user_mode = if user_mode == "selfhosted" {
        "selfhosted"
    } else {
        "clawparrot"
    };

    if effective_user_mode == "clawparrot" && !is_claude_family_model(&normalized_model_id) {
        return ResolvedModel {
            model_id: "claude-sonnet-4-6".to_string(),
            fallback_applied: true,
            error: None,
        };
    }

    if effective_user_mode == "selfhosted" && !has_provider && !is_claude_family_model(&normalized_model_id) {
        return ResolvedModel {
            model_id: normalized_model_id.clone(),
            fallback_applied: false,
            error: Some(format!(
                "No enabled self-hosted provider found for model \"{}\".",
                normalized_model_id
            )),
        };
    }

    ResolvedModel {
        model_id: normalized_model_id,
        fallback_applied: false,
        error: None,
    }
}

pub fn build_system_prompt_for_chat(
    custom_prompt: Option<&str>,
    config: &SystemPromptConfig,
    model_id: &str,
    user_mode: &str,
    has_provider: bool,
) -> (String, ResolvedModel) {
    let resolved = resolve_requested_model_for_mode(Some(model_id), user_mode, has_provider);

    let base_prompt = match custom_prompt {
        Some(cp) if !cp.is_empty() => cp.to_string(),
        _ => String::new(),
    };

    let prompt = build_self_hosted_system_prompt_with_config(&base_prompt, config);
    (prompt, resolved)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTemplate {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub content: String,
    pub variables: Vec<String>,
    pub category: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateManager {
    templates_dir: PathBuf,
}

impl TemplateManager {
    pub fn new(base_dir: &Path) -> Self {
        let templates_dir = base_dir.join("prompt_templates");
        if !templates_dir.exists() {
            let _ = fs::create_dir_all(&templates_dir);
        }
        Self { templates_dir }
    }

    pub fn create_template(&self, template: &PromptTemplate) -> Result<(), anyhow::Error> {
        let file_path = self.templates_dir.join(format!("{}.json", template.id));
        let content = serde_json::to_string_pretty(template)?;
        let mut file = File::create(&file_path)?;
        file.write_all(content.as_bytes())?;
        Ok(())
    }

    pub fn get_template(&self, id: &str) -> Result<Option<PromptTemplate>, anyhow::Error> {
        let file_path = self.templates_dir.join(format!("{}.json", id));
        if !file_path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&file_path)?;
        let template: PromptTemplate = serde_json::from_str(&content)?;
        Ok(Some(template))
    }

    pub fn update_template(&self, template: &PromptTemplate) -> Result<(), anyhow::Error> {
        self.create_template(template)
    }

    pub fn delete_template(&self, id: &str) -> Result<bool, anyhow::Error> {
        let file_path = self.templates_dir.join(format!("{}.json", id));
        if !file_path.exists() {
            return Ok(false);
        }
        fs::remove_file(&file_path)?;
        Ok(true)
    }

    pub fn list_templates(&self) -> Result<Vec<PromptTemplate>, anyhow::Error> {
        let mut templates = Vec::new();
        if !self.templates_dir.exists() {
            return Ok(templates);
        }
        
        for entry in fs::read_dir(&self.templates_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(id) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(Some(template)) = self.get_template(id) {
                        templates.push(template);
                    }
                }
            }
        }
        
        templates.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(templates)
    }

    pub fn render_template(&self, id: &str, variables: &HashMap<String, String>) -> Result<String, anyhow::Error> {
        let template = self.get_template(id)?
            .ok_or_else(|| anyhow::anyhow!("Template not found: {}", id))?;
        
        Self::render_content(&template.content, variables)
    }

    pub fn render_content(content: &str, variables: &HashMap<String, String>) -> Result<String, anyhow::Error> {
        let mut result = content.to_string();
        
        for (key, value) in variables {
            let placeholder = format!("{{{{{}}}}}", key);
            result = result.replace(&placeholder, value);
        }
        
        Ok(result)
    }

    pub fn extract_variables(content: &str) -> Vec<String> {
        let re = Regex::new(r"\{\{(\w+)\}\}").unwrap();
        let mut variables = Vec::new();
        
        for cap in re.captures_iter(content) {
            if let Some(var) = cap.get(1) {
                let var_name = var.as_str().to_string();
                if !variables.contains(&var_name) {
                    variables.push(var_name);
                }
            }
        }
        
        variables.sort();
        variables
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn memory_guidance_never_names_retired_remote_tools() {
        for profile in [super::PromptProfile::Desktop, super::PromptProfile::Code] {
            let prompt = super::compose_system_prompt(None, profile);
            assert!(prompt.contains("<memory_guidance>"));
            for retired in ["memory_search", "memory_ingest", "HTTP", "http://", "https://"] {
                assert!(!prompt.contains(retired), "{profile:?} contains {retired}");
            }
        }
    }

    #[test]
    fn code_prompt_is_isolated_and_preserves_custom_instructions() {
        let custom = "请保留这段项目指令。\n<identity>My project</identity>";
        let prompt = super::compose_system_prompt(Some(custom), super::PromptProfile::Code);
        assert!(prompt.contains(custom));
        assert!(!prompt.contains("You are powered by the model"));
        assert!(!prompt.contains("# Working environment"));
        assert!(prompt.starts_with(super::CODE_IDENTITY));
        assert!(!prompt.contains("<code_behavior>") && !prompt.contains("</code_behavior>"));
        assert!(!prompt.contains("IMPORTANT: Assist with authorized security testing"));
        assert!(!prompt.contains("deepseek-code is available as a CLI in the terminal"));
        assert_eq!(prompt.matches("<memory_guidance>").count(), 1);
        assert!(!prompt.contains("Reply with OK only"));
        for mode in ["chat", "work", "study", "cowork", "unknown"] {
            let profile = super::PromptProfile::from_workspace_mode(mode);
            let desktop = super::compose_system_prompt(None, profile);
            assert!(!desktop.contains("<code_behavior>"));
            assert!(desktop.contains("<memory_guidance>"));
        }
    }

    #[test]
    fn code_prompt_has_execution_rules_without_duplicated_tool_text() {
        let prompt = super::compose_system_prompt(None, super::PromptProfile::Code);
        assert_eq!(prompt.matches("# Execution and verification").count(), 1);
        assert!(prompt.contains("In a non-interactive run (no user to reply)"));
        assert!(prompt.contains("Do not duplicate work already assigned to a subagent."));
        assert!(prompt.contains("isn't in the project or its dependency cache"));
        assert!(prompt.contains("`~/code/<owner>/<repo>` (fetch if already cloned) and read it there."));
        assert!(prompt.contains("Never clone into the project directory."));
        assert!(prompt.contains("prefer the Python tool over a Bash heredoc"));
        assert_eq!(prompt.matches("# Working efficiently").count(), 1);
        for required in [
            "Stay strictly on task",
            "Batch independent tool calls in one turn",
            "Prefer dedicated read/search tools and omit redundant `cd`",
            "Verify once with a compound command; do not re-run passing checks",
            "Use `MultiEdit` for multiple edits in one file",
            "Never poll with `sleep`",
        ] {
            assert!(prompt.contains(required), "missing efficiency rule: {required}");
        }
        for removed in [
            "historical name of the shell tool",
            "Bash timeout is in milliseconds",
            "AskUserQuestion is for a decision",
            "exhaustive survey",
            "Cowork",
            "Recalled entries",
            "never formatted tables",
            "Shell commands initially start",
        ] {
            assert!(!prompt.contains(removed), "unexpected text: {removed}");
        }
    }

    #[test]
    fn project_facts_join_the_environment_reminder() {
        let workspace = tempfile::tempdir().unwrap();

        let mut single = super::environment_reminder(Some(workspace.path()));
        super::add_project_to_environment(&mut single, "Solo", &["/w/a"]);
        assert!(single.starts_with("<system-reminder>\n# Working environment\n"));
        assert!(single.ends_with("</system-reminder>"));
        assert!(single.contains(" - Project: Solo\n - Primary working directory: "));
        assert!(!single.contains("Project source folders"));

        let mut multi = super::environment_reminder(Some(workspace.path()));
        super::add_project_to_environment(&mut multi, "Multi", &["/w/a", "/w/b"]);
        assert!(multi.contains(" - Project: Multi\n - Primary working directory: "));
        let git = multi.find(" - Is a git repository: ").unwrap();
        let after_git = git + multi[git..].find('\n').unwrap();
        let expected = "\n - Project source folders (the first is the working directory):\n   - /w/a\n   - /w/b\n - Platform: ";
        assert!(multi[after_git..].starts_with(expected), "{}", &multi[after_git..]);

        // The system prompt has no environment; the first user message carries it.
        let system = super::compose_system_prompt_in(None, super::PromptProfile::Code);
        assert!(!system.contains("# Working environment") && !system.contains(" - Project: Multi"));
        assert!(super::split_tail(&system).is_none());
        let start = multi.find("# Working environment").unwrap();
        let end = multi[start..].find("\n\n").map(|offset| start + offset).unwrap_or(multi.len());
        println!("environment sample:\n{}", &multi[start..end]);

        // Text without an environment block is left unchanged.
        let mut bare = String::from("Only this.");
        super::add_project_to_environment(&mut bare, "Custom", &["/a", "/b"]);
        assert_eq!(bare, "Only this.");

        // Without workspace facts the name still stays inside the reminder.
        let mut no_workspace = super::environment_reminder(None);
        super::add_project_to_environment(&mut no_workspace, "NoDir", &[]);
        assert!(no_workspace.ends_with(" - Project: NoDir\n</system-reminder>"), "{no_workspace}");
    }

    #[test]
    fn desktop_prompt_uses_the_shared_identity_and_behavior() {
        let prompt = super::compose_desktop_system_prompt(Some("Respond in Spanish.\n<identity>Project persona</identity>"));
        assert!(prompt.starts_with(super::CODE_IDENTITY));
        assert!(prompt.contains("<behavior>"));
        assert!(prompt.contains("<memory_guidance>"));
        assert!(!prompt.contains("You are powered by the model"));
        assert!(prompt.contains("Respond in Spanish.\n<identity>Project persona</identity>"));
        assert_eq!(prompt.matches("<behavior>").count(), 1);
        assert!(!prompt.contains("<desktop_behavior>"));
    }

    #[test]
    fn desktop_prompt_is_present_without_request_instructions() {
        let prompt = super::compose_desktop_system_prompt(None);
        assert!(prompt.starts_with(super::CODE_IDENTITY));
        assert!(prompt.contains("<behavior>"));
        assert!(!prompt.contains("<additional_instructions>"));
        assert!(!super::compose_desktop_system_prompt(Some("  ")).contains("<additional_instructions>"));
    }

    use super::*;

    #[test]
    fn windows_version_keeps_major_minor_build_only() {
        assert_eq!(parse_windows_version("Microsoft Windows [Version 10.0.26300.9974]").as_deref(), Some("10.0.26300"));
        // The bracket label follows the system language; only the number matters.
        assert_eq!(parse_windows_version("Microsoft Windows [版本 10.0.19041.3803]").as_deref(), Some("10.0.19041"));
        assert_eq!(parse_windows_version("Microsoft Windows [Version 10.0.22631.4169] trailing 1.2.3").as_deref(), Some("10.0.22631"));
        assert_eq!(parse_windows_version("Microsoft Windows"), None);
        assert_eq!(parse_windows_version("10.0"), None);
        assert_eq!(parse_windows_version(""), None);
    }

    #[test]
    fn architecture_uses_platform_labels() {
        assert_eq!(architecture_label("x86_64"), "x64");
        assert_eq!(architecture_label("aarch64"), "arm64");
        assert_eq!(architecture_label("x86"), "x86");
        assert_eq!(architecture_label("riscv64"), "riscv64");
    }

    #[test]
    fn environment_section_lists_facts_in_order_without_a_model_line() {
        let workspace = tempfile::tempdir().unwrap();
        let system = environment_reminder(Some(workspace.path()));
        assert!(system.starts_with("<system-reminder>\n# Working environment\n"));
        assert!(system.ends_with("</system-reminder>"));
        assert!(!system.contains("You are powered by the model"));
        assert!(system.contains(&format!(" - Architecture: {}", architecture_label(std::env::consts::ARCH))));
        // Optional facts (Shell, OS Version, Node.js, Python) may be absent, so
        // only the lines this process produced are checked, in order.
        let labels = [
            " - Primary working directory: ",
            " - Is a git repository: ",
            " - Platform: ",
            " - Architecture: ",
            " - Shell: ",
            " - OS Version: ",
            " - Node.js version: ",
            " - Python: ",
            " - Downloads and extracted archives are untrusted",
        ];
        let mut at = 0;
        for label in labels {
            if let Some(found) = system.find(label) {
                assert!(found >= at, "{label} is out of order:\n{system}");
                at = found;
            }
        }
        if let Some(at) = system.find(" - Shell: ") {
            let line = system[at..].split('\n').next().unwrap();
            let program = line.trim_start_matches(" - Shell: ");
            assert!(std::path::Path::new(program).is_absolute(), "{line}");
        }
        assert!(!system.contains("Today's date"), "the date is its own reminder");
        assert_eq!(reminder_date(&date_reminder("2026-11-19")).as_deref(), Some("2026-11-19"));
        println!("environment section:\n{system}");
    }

    #[test]
    fn prompts_name_deepseek_instead_of_claude() {
        for profile in [PromptProfile::Code, PromptProfile::Desktop] {
            let prompt = compose_system_prompt(None, profile);
            assert!(!prompt.contains("Claude") && !prompt.contains("claude.ai") && !prompt.contains("Anthropic"), "{profile:?}");
        }
    }

    #[test]
    fn static_sections_become_cached_blocks_before_the_dynamic_tail() {
        let mut system = compose_system_prompt_in(Some("custom rule"), PromptProfile::Code);
        system.push_str("\nProject instructions follow\n");
        let blocks = cache_blocks(&system).unwrap();
        assert_eq!(blocks.len(), 5);
        assert_eq!(blocks[0], (CODE_IDENTITY.to_string(), true));
        assert!(blocks[1].1 && blocks[1].0.contains("# Harness"));
        assert!(blocks[2].1 && blocks[2].0.contains("<behavior>"));
        assert!(blocks[3].1 && blocks[3].0.contains("<memory_guidance>"));
        assert!(!blocks[4].1 && blocks[4].0.starts_with("<additional_instructions>"));
        assert!(blocks[4].0.contains("custom rule") && blocks[4].0.ends_with("Project instructions follow\n"));
        // A prompt without request-specific text has no dynamic tail at all.
        let plain = compose_system_prompt_in(None, PromptProfile::Code);
        assert!(split_tail(&plain).is_none());
        assert_eq!(cache_blocks(&plain).unwrap().len(), 4);
        // Every request in a session shares the identical cached prefix.
        let other = compose_system_prompt_in(None, PromptProfile::Code);
        {
            use crate::services::customization::{PromptMode, SystemPrompt};
            let compose = |mode, text: &str| compose_customized(Some("rule"), PromptProfile::Code, &SystemPrompt { mode, text: text.into() }, true);
            assert_eq!(compose(PromptMode::Default, "ignored"), compose_system_prompt_in(Some("rule"), PromptProfile::Code));
            let appended = compose(PromptMode::Append, "EXTRA_RULE");
            assert!(appended.contains(CODE_IDENTITY) && appended.contains("EXTRA_RULE"));
            let replaced = compose(PromptMode::Replace, "ONLY_THIS");
            assert!(replaced.starts_with("ONLY_THIS") && !replaced.contains(CODE_IDENTITY));
            assert!(!replaced.contains("# Working environment") && replaced.contains("rule"));
            assert!(compose(PromptMode::Replace, "  ").contains(CODE_IDENTITY), "an empty replacement keeps the default");
            assert!(default_system_prompt(PromptProfile::Code).starts_with(CODE_IDENTITY));
        }
        assert_eq!(cache_blocks(&other).unwrap()[..4], blocks[..4]);
        let desktop = cache_blocks(&compose_desktop_system_prompt(None)).unwrap();
        assert!(desktop[0].0.starts_with(CODE_IDENTITY) && desktop[2].0.contains("<behavior>"));
        assert!(cache_blocks("Respond in French.").is_none());
    }

    #[test]
    fn memory_switch_drops_the_guidance_section_for_both_profiles() {
        for profile in [PromptProfile::Desktop, PromptProfile::Code] {
            let prompt = compose_system_prompt_with_memory(None, profile, false);
            assert!(!prompt.contains("<memory_guidance>"), "{profile:?}");
            assert!(prompt.starts_with(CODE_IDENTITY));
            assert!(prompt.contains("<behavior>"));
            let blocks = cache_blocks(&prompt).unwrap();
            assert_eq!(blocks.len(), 3);
            assert!(blocks.iter().all(|(_, cached)| *cached));

            let enabled = compose_system_prompt_with_memory(Some("rule"), profile, true);
            assert!(enabled.contains("<memory_guidance>"));
            let blocks = cache_blocks(&enabled).unwrap();
            assert_eq!(blocks.len(), 5);
            assert!(blocks[3].0.contains("<memory_guidance>"));
            assert!(!blocks[4].1, "the request tail stays uncached");
        }
        // Both profiles compose the same sections.
        assert_eq!(
            compose_system_prompt(None, PromptProfile::Code),
            compose_system_prompt(None, PromptProfile::Desktop)
        );
    }

    #[test]
    fn first_message_reminders_keep_the_stable_order() {
        let workspace = tempfile::tempdir().unwrap();
        let mut environment = environment_reminder(Some(workspace.path()));
        add_project_to_environment(&mut environment, "Demo", &[workspace.path().to_str().unwrap()]);
        let blocks = first_message_reminders(SessionReminders {
            user_instructions: Some("<system-reminder>\nuser rules\n</system-reminder>".into()),
            standalone_frontend: true,
            project_instructions: Some("<system-reminder>\nproject rules\n</system-reminder>".into()),
            resident_indexes: vec!["<system-reminder>\nMEMORY.md index marker\n</system-reminder>".into()],
            experience: Some("<system-reminder>\n# Experience\nexperience marker\n</system-reminder>".into()),
            loadable_tools: Some(
                "<system-reminder>\nTools you can load with ToolLoad (pass exact names in the names array):\n- ListDir: List a directory\n</system-reminder>".into(),
            ),
            available_skills: Some(
                "<system-reminder>\nAvailable skills: invoke the Skill tool\n- deploy: Ship it\n</system-reminder>".into(),
            ),
            inline_skills: Some(
                "<system-reminder>\n# Skill: codegraph\ncodegraph explore marker\n</system-reminder>".into(),
            ),
            environment: Some(environment),
            date: Some(date_reminder("2026-11-19")),
            git: Some("<system-reminder>\n# gitStatus\n</system-reminder>".into()),
            skills: Some("<system-reminder>\ninvoked skills\n</system-reminder>".into()),
        });
        assert_eq!(blocks.len(), 12);
        assert!(blocks[0].contains("user rules"));
        assert_eq!(blocks[1], standalone_frontend_reminder());
        assert!(blocks[1].contains("! <command>"), "the CLI's command hint travels with its frontend facts");
        assert!(blocks[2].contains("project rules"));
        assert!(blocks[3].contains("MEMORY.md index marker"));
        assert!(blocks[4].contains("experience marker"));
        assert!(blocks[5].contains("Tools you can load with ToolLoad"));
        assert!(blocks[6].contains("Available skills"));
        assert!(blocks[7].contains("codegraph explore marker"));
        assert!(blocks[8].starts_with("<system-reminder>\n# Working environment\n"));
        assert!(blocks[8].contains(" - Project: Demo\n - Primary working directory: "));
        assert!(blocks[8].contains(" - Platform: "));
        assert!(!blocks[8].contains("Today's date"));
        assert_eq!(blocks[9], date_reminder("2026-11-19"));
        assert_eq!(blocks[10], "<system-reminder>\n# gitStatus\n</system-reminder>");
        assert_eq!(blocks[11], "<system-reminder>\ninvoked skills\n</system-reminder>");
        // The terminal facts are a first-message reminder, never system text.
        let system = compose_system_prompt_in(None, PromptProfile::Code);
        assert!(!system.contains(STANDALONE_FRONTEND));
        assert!(!compose_system_prompt(None, PromptProfile::Desktop).contains(STANDALONE_FRONTEND));
        // The loadable tools and skills lists moved out of the system prompt.
        for absent in [
            "# Tool directory",
            "MCP tools available on demand",
            "Tools you can load with ToolLoad",
            "Available skills:",
            "! <command>",
        ] {
            assert!(!system.contains(absent), "{absent} in:\n{system}");
        }
        // Absent optional parts are simply omitted.
        assert!(first_message_reminders(SessionReminders::default()).is_empty());
        for (index, block) in blocks.iter().enumerate() {
            println!("reminder {index}: {}", block.chars().take(150).collect::<String>());
        }
        let end: String = system.chars().skip(system.chars().count().saturating_sub(160)).collect();
        println!("system prompt end:\n{end}");
    }

    #[test]
    fn always_skills_are_inlined_and_frozen_while_other_states_cost_zero_tokens() {
        let skill = |name: &str, content: &str, always_inline: bool, disable_model_invocation: bool| {
            crate::skills::discovery::SkillCommand {
                name: name.to_owned(),
                description: format!("{name} description"),
                path: std::path::PathBuf::from(format!("/skills/{name}/SKILL.md")),
                content: content.to_owned(),
                user_invocable: true,
                disable_model_invocation,
                listing_name_only: false,
                always_inline,
                context: None,
                shell: None,
                agent: None,
            }
        };
        let mut skills = BTreeMap::new();
        skills.insert(
            "codegraph".to_owned(),
            skill(
                "codegraph",
                "Use CodeGraph.\n\n1. Run `codegraph status` or `codegraph init`.\n2. Run `codegraph explore`, `codegraph node`, `codegraph callers`, `codegraph impact`.\n3. Run `codegraph sync` after a large edit.\n\n$ARGUMENTS",
                true,
                false,
            ),
        );
        skills.insert(
            "style-guide".to_owned(),
            skill("style-guide", "Follow the repository style guide.", true, false),
        );
        skills.insert(
            "regular-user".to_owned(),
            skill("regular-user", "Should not be inlined.", false, false),
        );
        skills.insert(
            "hidden-always".to_owned(),
            skill("hidden-always", "Hidden from model.", true, true),
        );

        let blocks = inline_skill_blocks(&skills);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "codegraph");
        assert_eq!(blocks[1].0, "style-guide");
        assert_eq!(blocks[1].1, "Follow the repository style guide.");
        for command in [
            "codegraph status",
            "codegraph init",
            "codegraph explore",
            "codegraph node",
            "codegraph callers",
            "codegraph impact",
            "codegraph sync",
        ] {
            assert!(blocks[0].1.contains(command), "missing {command}: {}", blocks[0].1);
        }
        assert!(!blocks[0].1.contains("$ARGUMENTS"));

        // No `always_inline` skills -> zero extra text.
        let non_always = BTreeMap::from([(
            "regular-user".to_owned(),
            skill("regular-user", "Should not be inlined.", false, false),
        )]);
        assert!(inline_skill_blocks(&non_always).is_empty());
        assert!(inline_skill_blocks(&BTreeMap::new()).is_empty());

        let root = tempfile::tempdir().unwrap();
        let frozen = inline_skills_for_session(root.path(), "s", &blocks).unwrap();
        assert!(frozen.starts_with("<system-reminder>\n# Skill: codegraph\n"));
        assert!(frozen.contains("# Skill: style-guide\nFollow the repository style guide."));
        assert!(!frozen.contains("regular-user"));
        assert!(!frozen.contains("hidden-always"));
        assert_eq!(frozen.matches("<system-reminder>").count(), 1);
        // Later requests and resumes replay the frozen block.
        assert_eq!(
            inline_skills_for_session(root.path(), "s", &[]).as_deref(),
            Some(frozen.as_str())
        );
        assert!(inline_skills_for_session(root.path(), "other", &[]).is_none());
        assert!(inline_skills_reminder(&[]).is_none());
        assert!(first_message_reminders(SessionReminders {
            inline_skills: Some(frozen.clone()),
            ..Default::default()
        })
        .contains(&frozen));
    }

    #[test]
    fn codegraph_workspace_status_detects_ready_worktree_and_uninitialized() {
        // 1. Direct `.codegraph/codegraph.db` present -> Ready (also from a nested subdir).
        let ready = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ready.path().join(".git")).unwrap();
        std::fs::create_dir_all(ready.path().join(".codegraph")).unwrap();
        std::fs::write(ready.path().join(".codegraph/codegraph.db"), b"sqlite-bytes").unwrap();
        let nested = ready.path().join("crates/core");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            codegraph_workspace_status(ready.path()),
            CodeGraphIndexStatus::Ready(ready.path().to_path_buf())
        );
        assert_eq!(
            codegraph_workspace_status(&nested),
            CodeGraphIndexStatus::Ready(ready.path().to_path_buf())
        );

        // Zero-byte `.codegraph/codegraph.db` inside a git repo is GitUninitialized.
        let empty_db = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(empty_db.path().join(".git")).unwrap();
        std::fs::create_dir_all(empty_db.path().join(".codegraph")).unwrap();
        std::fs::write(empty_db.path().join(".codegraph/codegraph.db"), b"").unwrap();
        assert_eq!(
            codegraph_workspace_status(empty_db.path()),
            CodeGraphIndexStatus::GitUninitialized(empty_db.path().to_path_buf())
        );

        // 2. Git worktree `.git` file pointing to main repo with `.codegraph/codegraph.db`,
        //    while worktree only has `.codegraph/.gitignore` -> WorktreeUninitialized.
        let container = tempfile::tempdir().unwrap();
        let main_repo = container.path().join("main");
        let worktree = container.path().join("wt-feature");
        std::fs::create_dir_all(main_repo.join(".git/worktrees/wt-feature")).unwrap();
        std::fs::create_dir_all(main_repo.join(".codegraph")).unwrap();
        std::fs::write(main_repo.join(".codegraph/codegraph.db"), b"sqlite-bytes").unwrap();
        std::fs::create_dir_all(worktree.join(".codegraph")).unwrap();
        std::fs::write(worktree.join(".codegraph/.gitignore"), b"codegraph.db\n").unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", main_repo.join(".git/worktrees/wt-feature").display()),
        )
        .unwrap();
        assert_eq!(
            codegraph_workspace_status(&worktree),
            CodeGraphIndexStatus::WorktreeUninitialized {
                main_repo: main_repo.clone(),
            }
        );

        // 3. Non-git empty workspace -> NotGitRepo.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            codegraph_workspace_status(empty.path()),
            CodeGraphIndexStatus::NotGitRepo
        );
    }

    #[test]
    fn inline_skill_blocks_for_workspace_appends_status_only_to_codegraph() {
        let skill = |name: &str, content: &str| crate::skills::discovery::SkillCommand {
            name: name.to_owned(),
            description: format!("{name} description"),
            path: std::path::PathBuf::from(format!("/skills/{name}/SKILL.md")),
            content: content.to_owned(),
            user_invocable: true,
            disable_model_invocation: false,
            listing_name_only: false,
            always_inline: true,
            context: None,
            shell: None,
            agent: None,
        };
        let skills = BTreeMap::from([
            ("codegraph".to_owned(), skill("codegraph", "Use CodeGraph.")),
            (
                "style-guide".to_owned(),
                skill("style-guide", "Follow the style guide."),
            ),
        ]);

        // Ready workspace.
        let ready = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ready.path().join(".codegraph")).unwrap();
        std::fs::write(ready.path().join(".codegraph/codegraph.db"), b"db").unwrap();
        let ready_blocks = inline_skill_blocks_for_workspace(&skills, Some(ready.path()));
        assert_eq!(ready_blocks.len(), 2);
        assert_eq!(ready_blocks[0].0, "codegraph");
        assert!(ready_blocks[0].1.starts_with("Use CodeGraph.\n\nWorkspace CodeGraph index: READY"));
        assert!(ready_blocks[0].1.contains("skip `codegraph status` / `codegraph init`"));
        assert_eq!(ready_blocks[1], ("style-guide".to_owned(), "Follow the style guide.".to_owned()));

        // Worktree without its own index while main repo has one.
        let container = tempfile::tempdir().unwrap();
        let main_repo = container.path().join("main");
        let worktree = container.path().join("wt");
        std::fs::create_dir_all(main_repo.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(main_repo.join(".codegraph")).unwrap();
        std::fs::write(main_repo.join(".codegraph/codegraph.db"), b"db").unwrap();
        std::fs::create_dir_all(worktree.join(".codegraph")).unwrap();
        std::fs::write(worktree.join(".codegraph/.gitignore"), b"*\n").unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", main_repo.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        let wt_blocks = inline_skill_blocks_for_workspace(&skills, Some(&worktree));
        assert!(wt_blocks[0]
            .1
            .contains("NOT INITIALIZED in this git worktree"));
        assert!(wt_blocks[0].1.contains("codegraph init -y"));
        assert_eq!(wt_blocks[1], ("style-guide".to_owned(), "Follow the style guide.".to_owned()));

        // Uninitialized regular git repository -> prompts `codegraph init -y`.
        let git_only = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(git_only.path().join(".git")).unwrap();
        let git_blocks = inline_skill_blocks_for_workspace(&skills, Some(git_only.path()));
        assert!(git_blocks[0]
            .1
            .contains("NOT INITIALIZED in this git repository"));
        assert!(git_blocks[0].1.contains("codegraph init -y"));
        assert_eq!(git_blocks[1], ("style-guide".to_owned(), "Follow the style guide.".to_owned()));

        // Non-git empty workspace -> skips `codegraph init`.
        let empty = tempfile::tempdir().unwrap();
        let empty_blocks = inline_skill_blocks_for_workspace(&skills, Some(empty.path()));
        assert!(empty_blocks[0]
            .1
            .contains("not a git repository; skip `codegraph init`"));
        assert!(!empty_blocks[0].1.contains("codegraph init -y"));
        assert_eq!(empty_blocks[1], ("style-guide".to_owned(), "Follow the style guide.".to_owned()));
    }

    #[test]
    fn loadable_tools_show_the_resolved_text_uncut_and_freeze_once() {
        let root = tempfile::tempdir().unwrap();
        let builtin = vec![
            (
                "ListDir".to_owned(),
                "List a directory.\nA second line stays whole.".to_owned(),
            ),
            (
                "WebSearch".to_owned(),
                format!("{}\nsecond line", "x".repeat(200)),
            ),
        ];
        let mcp = vec![("mcp__gh__create_issue".to_owned(), "Create an issue.".to_owned())];
        let reminder = loadable_tools_for_session(root.path(), "s", &builtin, &mcp).unwrap();
        assert!(reminder.starts_with(
            "<system-reminder>\nTools you can load with ToolLoad (pass exact names in the names array):\n"
        ));
        // Every line keeps the whole text, only whitespace is collapsed.
        assert!(reminder.contains("- ListDir: List a directory. A second line stays whole."));
        assert!(reminder.contains("- mcp__gh__create_issue: Create an issue"));
        let web = reminder.lines().find(|line| line.starts_with("- WebSearch:")).unwrap();
        assert_eq!(web, format!("- WebSearch: {} second line", "x".repeat(200)));
        assert!(!reminder.contains('…'), "the listing cuts nothing: {reminder}");
        // A later request reads the frozen file, whatever the current tools say.
        assert_eq!(
            loadable_tools_for_session(root.path(), "s", &[], &[]).as_deref(),
            Some(reminder.as_str())
        );
        assert!(loadable_tools_for_session(root.path(), "other", &[], &[]).is_none());

        // An empty first list is frozen too: a tool or skill discovered later
        // must produce a change reminder, never a new block in the first message.
        let empty_root = tempfile::tempdir().unwrap();
        assert!(loadable_tools_for_session(empty_root.path(), "s", &[], &[]).is_none());
        assert!(
            loadable_tools_for_session(empty_root.path(), "s", &builtin, &mcp).is_none(),
            "a later tool cannot add a list to the frozen first message"
        );
        assert!(skills_for_session(empty_root.path(), "s", &[]).is_none());
        assert!(
            skills_for_session(empty_root.path(), "s", &["- deploy: Ship it".to_owned()]).is_none(),
            "a later skill cannot add a list to the frozen first message"
        );

        let lines = vec!["- deploy: Ship it (context: fork)".to_owned()];
        let skills = skills_for_session(root.path(), "s", &lines).unwrap();
        assert!(skills.starts_with("<system-reminder>\nAvailable skills: invoke the Skill tool"));
        assert!(skills.contains("- deploy: Ship it (context: fork)"));
        assert_eq!(
            skills_for_session(root.path(), "s", &["- changed: New".to_owned()]).as_deref(),
            Some(skills.as_str())
        );

        // The first observation records the set; every later change is reported
        // exactly once against the previous observation.
        let first = vec!["ListDir".to_owned(), "WebSearch".to_owned()];
        assert_eq!(set_changes(root.path(), "s", "loadable-tools", &first), (Vec::new(), Vec::new()));
        let (now, gone) = set_changes(
            root.path(),
            "s",
            "loadable-tools",
            &["ListDir".to_owned(), "Glob".to_owned()],
        );
        assert_eq!(now, vec!["Glob".to_owned()]);
        assert_eq!(gone, vec!["WebSearch".to_owned()]);
        assert_eq!(
            set_changes(root.path(), "s", "loadable-tools", &["ListDir".to_owned(), "Glob".to_owned()]),
            (Vec::new(), Vec::new()),
            "an unchanged set reports nothing on the next request"
        );
        assert_eq!(
            set_changes(root.path(), "s", "skills", &["deploy".to_owned()]),
            (Vec::new(), Vec::new()),
            "each kind tracks its own set"
        );
        assert_eq!(
            changes_reminder(
                now,
                gone,
                vec!["new-skill".to_owned()],
                vec!["old-skill".to_owned()],
                vec!["- .agents/experience/a.md | alpha: symptom".to_owned()],
                vec!["old-entry".to_owned()],
            )
                .unwrap(),
            "<system-reminder>\nNow loadable: Glob\nNo longer available: WebSearch\nNew skills: new-skill\nRemoved skills: old-skill\nNew experience:\n- .agents/experience/a.md | alpha: symptom\nRemoved experience: old-entry\n</system-reminder>"
        );
        assert!(changes_reminder(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new()
        )
        .is_none());
    }

    fn experience_entry(name: &str, description: &str) -> crate::experience::Entry {
        crate::experience::Entry {
            name: name.into(),
            description: description.into(),
            relative_path: format!(".agents/experience/{name}.md"),
        }
    }

    #[test]
    fn experience_freezes_on_first_request() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let first = experience_for_session(
            root.path(),
            "s",
            Some(workspace.path()),
            true,
            &[experience_entry("alpha", "first symptom")],
        )
        .unwrap();
        assert!(first.contains("# Experience\n"));
        assert!(first.contains("- .agents/experience/alpha.md | alpha: first symptom"));
        assert_eq!(first.matches("<system-reminder>").count(), 1);
        // A later scan, even with different entries, replays the frozen block.
        assert_eq!(
            experience_for_session(
                root.path(),
                "s",
                Some(workspace.path()),
                true,
                &[experience_entry("beta", "other symptom")],
            )
            .as_deref(),
            Some(first.as_str())
        );
    }

    #[test]
    fn experience_absent_for_internal_workspace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        assert!(experience_for_session(
            root.path(),
            "s",
            None,
            true,
            &[experience_entry("alpha", "symptom")],
        )
        .is_none());
        // The frozen None keeps the block out of the rest of the session.
        assert!(experience_for_session(
            root.path(),
            "s",
            Some(workspace.path()),
            true,
            &[experience_entry("alpha", "symptom")],
        )
        .is_none());
    }

    #[test]
    fn experience_absent_when_disabled() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        assert!(experience_for_session(
            root.path(),
            "s",
            Some(workspace.path()),
            false,
            &[experience_entry("alpha", "symptom")],
        )
        .is_none());
        // Re-enabling in the same session does not add the block.
        assert!(experience_for_session(
            root.path(),
            "s",
            Some(workspace.path()),
            true,
            &[experience_entry("alpha", "symptom")],
        )
        .is_none());
        // A new session picks up the switch.
        assert!(experience_for_session(
            root.path(),
            "new",
            Some(workspace.path()),
            true,
            &[experience_entry("alpha", "symptom")],
        )
        .is_some());
    }

    #[test]
    fn changes_reminder_reports_new_experience_once() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            set_changes(root.path(), "s", crate::session_files::SET_CHANGE_EXPERIENCE, &["alpha".to_owned()]),
            (Vec::new(), Vec::new())
        );
        let (new, removed) = set_changes(
            root.path(),
            "s",
            crate::session_files::SET_CHANGE_EXPERIENCE,
            &["alpha".to_owned(), "beta".to_owned()],
        );
        assert_eq!(new, vec!["beta".to_owned()]);
        assert!(removed.is_empty());
        let reminder = changes_reminder(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec!["- .agents/experience/beta.md | beta: symptom".to_owned()],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            reminder,
            "<system-reminder>\nNew experience:\n- .agents/experience/beta.md | beta: symptom\n</system-reminder>"
        );
        assert_eq!(
            set_changes(
                root.path(),
                "s",
                crate::session_files::SET_CHANGE_EXPERIENCE,
                &["alpha".to_owned(), "beta".to_owned()],
            ),
            (Vec::new(), Vec::new())
        );
    }

    #[test]
    fn changes_reminder_reports_removed_experience() {
        let root = tempfile::tempdir().unwrap();
        set_changes(
            root.path(),
            "s",
            crate::session_files::SET_CHANGE_EXPERIENCE,
            &["alpha".to_owned(), "beta".to_owned()],
        );
        let (new, removed) = set_changes(
            root.path(),
            "s",
            crate::session_files::SET_CHANGE_EXPERIENCE,
            &["alpha".to_owned()],
        );
        assert!(new.is_empty());
        assert_eq!(removed, vec!["beta".to_owned()]);
        let reminder = changes_reminder(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            removed,
        )
        .unwrap();
        assert_eq!(reminder, "<system-reminder>\nRemoved experience: beta\n</system-reminder>");
    }

    #[test]
    fn one_line_collapses_whitespace_without_cutting() {
        assert_eq!(one_line("First line.\nSecond line."), "First line. Second line.");
        assert_eq!(one_line("  spaced   words  "), "spaced words");
        let long = "a".repeat(120);
        assert_eq!(one_line(&long), long);
        assert_eq!(one_line("é".repeat(120).as_str()).chars().count(), 120);
    }

    #[test]
    fn one_sentence_collapses_whitespace_and_cuts_at_the_limit() {
        assert_eq!(one_sentence("First line.\nSecond line.", 100), "First line");
        assert_eq!(one_sentence("  spaced   words  ", 100), "spaced words");
        let long = "a".repeat(120);
        let cut = one_sentence(&long, 100);
        assert_eq!(cut.chars().count(), 100);
        assert!(cut.ends_with('…'));
        assert_eq!(one_sentence(&"é".repeat(120), 100).chars().count(), 100);
    }

    #[test]
    fn environment_and_date_are_frozen_per_session() {        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let first = environment_for_session(root.path(), "s", Some(workspace.path()), None);
        // A resume asks again, possibly without the workspace; the stored text wins.
        assert_eq!(environment_for_session(root.path(), "s", None, None), first);
        assert!(first.contains(&format!("Primary working directory: {}", workspace.path().display())));

        // The project's name and folders freeze with the environment too.
        let folders = vec!["/w/a".to_string(), "/w/b".to_string()];
        let project = environment_for_session(
            root.path(),
            "p",
            Some(workspace.path()),
            Some(("Frozen", &folders)),
        );
        assert!(project.contains(" - Project: Frozen\n - Primary working directory: "));
        assert!(project.contains(" - /w/a\n   - /w/b"));
        assert_eq!(
            environment_for_session(root.path(), "p", None, Some(("Changed", &folders))),
            project,
            "a later project change never rewrites the first message"
        );

        let frozen = date_for_session(root.path(), "s");
        assert_eq!(date_for_session(root.path(), "s"), frozen);
        assert!(reminder_date(&frozen).is_some());
        std::fs::write(
            root.path().join("cache/environment/s.date.txt"),
            date_reminder("2020-01-01"),
        )
        .unwrap();
        let frozen = date_for_session(root.path(), "s");
        assert_eq!(frozen, date_reminder("2020-01-01"));

        let changed = date_change_reminder(root.path(), "s", &frozen, "2020-01-02").unwrap();
        assert_eq!(changed, "<system-reminder>\nThe date has changed. Today's date is 2020-01-02.\n</system-reminder>");
        assert!(date_change_reminder(root.path(), "s", &frozen, "2020-01-02").is_none(), "announced once per new date");
        assert!(date_change_reminder(root.path(), "s", &frozen, "2020-01-03").is_some(), "a newer date is announced");
        assert!(date_change_reminder(root.path(), "s", &date_reminder("2020-01-03"), "2020-01-03").is_none(), "a fresh date needs no reminder");
        assert_eq!(environment_for_session(root.path(), "s", None, None), first, "a date change never rewrites the environment");
    }

    /// Write every per-session file the two features produce, through the same
    /// helpers the runtime uses.
    fn write_session_files(root: &std::path::Path, session: &str) {
        let workspace = std::path::Path::new("/tmp");
        environment_for_session(root, session, Some(workspace), None);
        date_for_session(root, session);
        date_change_reminder(root, session, &date_reminder("2020-01-01"), "2020-01-02");
        loadable_tools_for_session(
            root,
            session,
            &[("ListDir".to_owned(), "List a directory.".to_owned())],
            &[],
        );
        skills_for_session(root, session, &["- deploy: Ship it".to_owned()]);
        inline_skills_for_session(
            root,
            session,
            &[("codegraph".to_owned(), "codegraph explore".to_owned())],
        );
        experience_for_session(root, session, Some(root), true, &[]);
        set_changes(root, session, "loadable-tools", &["ListDir".to_owned()]);
        set_changes(root, session, "skills", &["deploy".to_owned()]);
        set_changes(
            root,
            session,
            crate::session_files::SET_CHANGE_EXPERIENCE,
            &["alpha".to_owned()],
        );
        std::fs::write(
            crate::session_files::environment_file(
                root,
                session,
                crate::session_files::SUGGESTION_SUFFIX,
            ),
            b"{}",
        )
        .unwrap();
        let snapshot = crate::session_files::mcp_snapshot_file(root, session);
        std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        std::fs::write(snapshot, b"{}").unwrap();
    }

    fn set_modified(path: &std::path::Path, time: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn forget_removes_only_that_sessions_files() {
        let root = tempfile::tempdir().unwrap();
        write_session_files(root.path(), "s");
        write_session_files(root.path(), "other");
        let forgotten = crate::session_files::files_for_session(root.path(), "s");
        let kept = crate::session_files::files_for_session(root.path(), "other");
        for path in &forgotten {
            assert!(path.exists(), "missing {}", path.display());
        }
        forget_session_files(root.path(), "s");
        for path in &forgotten {
            assert!(!path.exists(), "still present: {}", path.display());
        }
        for path in &kept {
            assert!(path.exists(), "other session lost {}", path.display());
        }
    }

    #[test]
    fn forget_rejects_session_ids_that_could_escape_the_directory() {
        let root = tempfile::tempdir().unwrap();
        write_session_files(root.path(), "s");
        let kept = crate::session_files::files_for_session(root.path(), "s");
        forget_session_files(root.path(), "../s");
        forget_session_files(root.path(), "nested/s");
        forget_session_files(root.path(), r"nested\s");
        for path in &kept {
            assert!(path.exists(), "unsafe id removed {}", path.display());
        }
    }

    #[test]
    fn prune_removes_old_files_and_keeps_recent_ones() {
        let root = tempfile::tempdir().unwrap();
        write_session_files(root.path(), "old");
        write_session_files(root.path(), "recent");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60);
        let recent = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60);
        for path in crate::session_files::files_for_session(root.path(), "old") {
            set_modified(&path, old);
        }
        for path in crate::session_files::files_for_session(root.path(), "recent") {
            set_modified(&path, recent);
        }

        prune_session_files(root.path(), SESSION_FILE_MAX_AGE);

        for path in crate::session_files::files_for_session(root.path(), "old") {
            assert!(!path.exists(), "old file kept: {}", path.display());
        }
        for path in crate::session_files::files_for_session(root.path(), "recent") {
            assert!(path.exists(), "recent file pruned: {}", path.display());
        }
    }

    #[test]
    fn skill_invocation_is_declared_in_the_reminder_not_the_system_prompt() {
        let system = compose_system_prompt(None, PromptProfile::Desktop);
        assert!(
            !system.contains("read its instructions with an available tool"),
            "the conflicting clause must not come back: {system}"
        );
        // code.md keeps the slash-command rule and must name the same tool.
        assert!(system.contains("invoke it via Skill"));
        assert!(SKILLS_LEAD_IN.contains("{\"skill\":\"<name>\"}"));
        assert!(SKILLS_LEAD_IN.contains("Skill tool"));
        assert!(!SKILLS_LEAD_IN.contains("ToolLoad"));
        assert!(skills_reminder(&["- demo: Demo".to_owned()])
            .unwrap()
            .contains(SKILLS_LEAD_IN));
    }

    #[test]
    fn test_is_claude_family_model() {
        assert!(is_claude_family_model("claude-sonnet-4-6"));
        assert!(is_claude_family_model("claude-opus-4"));
        assert!(!is_claude_family_model("gpt-4"));
        assert!(!is_claude_family_model("gemini-pro"));
    }

    #[test]
    fn test_resolve_model_for_clawparrot() {
        let result = resolve_requested_model_for_mode(Some("gpt-4"), "clawparrot", false);
        assert!(result.fallback_applied);
        assert_eq!(result.model_id, "claude-sonnet-4-6");
    }

    #[test]
    fn test_resolve_model_for_selfhosted() {
        let result = resolve_requested_model_for_mode(Some("claude-sonnet-4-6"), "selfhosted", true);
        assert!(!result.fallback_applied);
        assert_eq!(result.model_id, "claude-sonnet-4-6");
    }

    #[test]
    fn test_resolve_model_empty_string() {
        let result = resolve_requested_model_for_mode(Some(""), "clawparrot", false);
        assert_eq!(result.model_id, "claude-sonnet-4-6");
    }

    #[test]
    fn test_resolve_model_none() {
        let result = resolve_requested_model_for_mode(None, "clawparrot", false);
        assert_eq!(result.model_id, "claude-sonnet-4-6");
    }

    #[test]
    fn test_build_self_hosted_prompt_no_identity() {
        let result = build_self_hosted_system_prompt("Hello world");
        assert!(result.contains("<identity>"));
        assert!(result.contains("Hello world"));
    }

    #[test]
    fn test_build_self_hosted_prompt_with_identity() {
        let input = "<identity>Old identity</identity>\nHello world";
        let result = build_self_hosted_system_prompt(input);
        assert!(result.contains("deepseek-code desktop app"));
        assert!(!result.contains("Old identity"));
        assert!(result.contains("Hello world"));
    }

    #[test]
    fn test_remove_override_sections() {
        let input = "Before <override_instructions>skip this</override_instructions> After";
        let result = remove_override_sections(input);
        assert_eq!(result, "Before After");
    }

    #[test]
    fn test_build_system_prompt_for_chat() {
        let config = SystemPromptConfig::default();
        let (prompt, resolved) = build_system_prompt_for_chat(
            Some("Custom prompt"),
            &config,
            "claude-sonnet-4-6",
            "selfhosted",
            true,
        );
        assert!(prompt.contains("<identity>"));
        assert!(prompt.contains("Custom prompt"));
        assert!(!resolved.fallback_applied);
    }
}
