//! Session-scoped plan documents. Approval is handled by the host, never inferred
//! from the existence of a document or from model-supplied permissions.
use anyhow::{ensure, Context, Result};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const MAX_PLAN_BYTES: usize = 256 * 1024;

pub fn path(root: &Path, session: &str) -> Result<PathBuf> {
    ensure!(
        !session.is_empty() && session.len() <= 100,
        "Invalid plan session ID"
    );
    let key: String = session.bytes().map(|byte| format!("{byte:02x}")).collect();
    // The data root is selected by the host, not supplied in tool input. Resolve
    // trusted aliases first (including macOS /var and /tmp), then reject links
    // below that root and pin the resulting real directory during IO.
    let directory = root
        .canonicalize()
        .context("Plan data directory is unavailable")?
        .join("plans");
    for ancestor in directory.ancestors() {
        check_node(ancestor, true)?;
    }
    let path = directory.join(format!("{key}.md"));
    check_node(&path, false)?;
    Ok(path)
}

fn check_node(path: &Path, directory: bool) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        !metadata.file_type().is_symlink(),
        "Plan path cannot contain symbolic links"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "Plan path cannot contain reparse points"
        );
    }
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "Invalid plan path type"
    );
    if !directory {
        ensure!(
            metadata.len() <= MAX_PLAN_BYTES as u64,
            "Plan exceeds 256 KiB"
        );
    }
    Ok(())
}

/// Pin Windows directory ancestors against rename/reparse replacement until IO finishes.
/// Model-supplied paths never select this directory: it derives from trusted runtime state.
#[cfg(not(unix))]
fn guard_directory(
    root: &Path,
    session: &str,
    create: bool,
) -> Result<(PathBuf, Vec<std::fs::File>)> {
    let path = path(root, session)?;
    let directory = path.parent().context("Missing plan directory")?;
    let mut guards = Vec::new();
    for ancestor in directory.ancestors().collect::<Vec<_>>().into_iter().rev() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        check_node(ancestor, true)?;
        if !ancestor.exists() {
            if create {
                std::fs::create_dir(ancestor)?;
            } else {
                continue;
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let handle = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(3)
                .custom_flags(0x02000000 | 0x00200000)
                .open(ancestor)?;
            use std::os::windows::fs::MetadataExt;
            ensure!(
                handle.metadata()?.file_attributes() & 0x400 == 0,
                "Plan directory cannot be a reparse point"
            );
            guards.push(handle);
        }
    }
    check_node(&path, false)?;
    Ok((path, guards))
}

#[cfg(unix)]
fn guard_directory(
    root: &Path,
    session: &str,
    create: bool,
) -> Result<(PathBuf, Vec<std::fs::File>)> {
    use rustix::fs::{mkdirat, open, openat, Mode, OFlags};
    let path = path(root, session)?;
    ensure!(path.is_absolute(), "Plan directory must be absolute");
    let mut directory: std::fs::File = open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?
    .into();
    for component in path
        .parent()
        .context("Missing plan directory")?
        .components()
    {
        let name = match component {
            std::path::Component::RootDir => continue,
            std::path::Component::Normal(name) => name,
            _ => anyhow::bail!("Invalid plan directory component"),
        };
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let opened = match openat(&directory, name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) if create => {
                match mkdirat(&directory, name, Mode::from_raw_mode(0o700)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                openat(&directory, name, flags, Mode::empty())?
            }
            Err(rustix::io::Errno::NOENT) => return Ok((path, Vec::new())),
            Err(error) => return Err(error.into()),
        };
        directory = opened.into();
    }
    Ok((path, vec![directory]))
}

pub fn read(root: &Path, session: &str) -> Result<Option<String>> {
    let (path, guards) = guard_directory(root, session, false)?;
    #[cfg(unix)]
    let file: std::fs::File = {
        use rustix::fs::{openat, Mode, OFlags};
        let Some(directory) = guards.last() else {
            return Ok(None);
        };
        match openat(
            directory,
            path.file_name().context("Missing plan filename")?,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd.into(),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    };
    #[cfg(not(unix))]
    let file = {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000).share_mode(1);
        }
        match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            file.metadata()?.file_attributes() & 0x400 == 0,
            "Plan cannot be a reparse point"
        );
    }
    ensure!(file.metadata()?.is_file(), "Plan must be a regular file");
    let mut content = String::new();
    file.take(MAX_PLAN_BYTES as u64 + 1)
        .read_to_string(&mut content)?;
    ensure!(content.len() <= MAX_PLAN_BYTES, "Plan exceeds 256 KiB");
    drop(guards);
    Ok(Some(content))
}

pub fn save(root: &Path, session: &str, content: &str) -> Result<PathBuf> {
    ensure!(!content.trim().is_empty(), "Plan cannot be empty");
    ensure!(content.len() <= MAX_PLAN_BYTES, "Plan exceeds 256 KiB");
    let (path, guards) = guard_directory(root, session, true)?;
    #[cfg(unix)]
    {
        use rustix::fs::{openat, renameat, statat, unlinkat, AtFlags, FileType, Mode, OFlags};
        let directory = guards.last().context("Missing plan directory handle")?;
        let name = path.file_name().context("Missing plan filename")?;
        let temporary_name = format!(".plan-{}.tmp", uuid::Uuid::new_v4());
        let mut temporary: std::fs::File = openat(
            directory,
            temporary_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?
        .into();
        let result: Result<()> = (|| {
            temporary.write_all(content.as_bytes())?;
            temporary.sync_all()?;
            match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => ensure!(
                    FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile,
                    "Plan must be a regular file"
                ),
                Err(rustix::io::Errno::NOENT) => {}
                Err(error) => return Err(error.into()),
            }
            renameat(directory, temporary_name.as_str(), directory, name)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(directory, temporary_name.as_str(), AtFlags::empty());
        }
        result?;
    }
    #[cfg(not(unix))]
    {
        let directory = path.parent().context("Missing plan directory")?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(content.as_bytes())?;
        temporary.as_file().sync_all()?;
        check_node(&path, false)?;
        temporary.persist(&path).map_err(|error| error.error)?;
    }
    drop(guards);
    Ok(path)
}

/// Exact advertised absolute path only: no aliases, traversal, alternate session or tool.
pub fn is_document_tool(root: &Path, session: &str, tool: &str, input: &serde_json::Value) -> bool {
    if !matches!(tool, "Read" | "Write" | "Edit") {
        return false;
    }
    let Some(candidate) = input["file_path"].as_str() else {
        return false;
    };
    if candidate
        .split(['/', '\\'])
        .any(|part| matches!(part, "." | ".."))
    {
        return false;
    }
    let candidate = Path::new(candidate);
    candidate.is_absolute() && path(root, session).is_ok_and(|expected| candidate == expected)
}

pub fn edit_document(
    root: &Path,
    session: &str,
    tool: &str,
    input: &serde_json::Value,
) -> Result<serde_json::Value> {
    ensure!(
        matches!(tool, "Write" | "Edit") && is_document_tool(root, session, tool, input),
        "Only the current session plan file may be edited"
    );
    let previous = read(root, session)?;
    let content = if tool == "Write" {
        input["content"]
            .as_str()
            .context("content required")?
            .to_owned()
    } else {
        let old = input["old_string"]
            .as_str()
            .context("old_string required")?;
        let new = input["new_string"]
            .as_str()
            .context("new_string required")?;
        ensure!(!old.is_empty(), "old_string cannot be empty");
        let content = previous.as_deref().context("Plan file does not exist")?;
        let matches = content.matches(old).count();
        let all = input["replace_all"].as_bool().unwrap_or(false);
        ensure!(
            matches > 0 && (all || matches == 1),
            "old_string must match exactly once unless replace_all=true"
        );
        let replacements = if all { matches } else { 1 };
        let resulting_size = content
            .len()
            .checked_sub(replacements.saturating_mul(old.len()))
            .and_then(|remaining| {
                new.len()
                    .checked_mul(replacements)
                    .and_then(|added| remaining.checked_add(added))
            });
        ensure!(
            resulting_size.is_some_and(|size| size <= MAX_PLAN_BYTES),
            "Plan exceeds 256 KiB"
        );
        if all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        }
    };
    let path = save(root, session, &content)?;
    let change = crate::tools::file_change::completed_text(
        &path,
        Some(previous.unwrap_or_default()),
        read(root, session).ok().flatten(),
    );
    Ok(
        serde_json::json!({"success":true,"planFilePath":path,"bytes_written":content.len(),"file_change":change,"content":"Session plan saved. Call ExitPlanMode to request approval."}),
    )
}

pub fn definitions() -> Vec<crate::tools::ToolDefinition> {
    use serde_json::json;
    vec![
        crate::tools::ToolDefinition {
            name: "EnterPlanMode".into(),
            description: "Use this tool proactively when you're about to start a non-trivial implementation task. Getting user sign-off on your approach before writing code prevents wasted effort and ensures alignment. This tool transitions you into plan mode where you can explore the codebase and design an implementation approach for user approval.\n\n## When to Use This Tool\n\n**Prefer using EnterPlanMode** for implementation tasks unless they're simple. Use it when ANY of these conditions apply:\n\n1. **New Feature Implementation**: Adding meaningful new functionality\n   - Example: \"Add a logout button\" - where should it go? What should happen on click?\n   - Example: \"Add form validation\" - what rules? What error messages?\n\n2. **Multiple Valid Approaches**: The task can be solved in several different ways\n   - Example: \"Add caching to the API\" - could use Redis, in-memory, file-based, etc.\n   - Example: \"Improve performance\" - many optimization strategies possible\n\n3. **Code Modifications**: Changes that affect existing behavior or structure\n   - Example: \"Update the login flow\" - what exactly should change?\n   - Example: \"Refactor this component\" - what's the target architecture?\n\n4. **Architectural Decisions**: The task requires choosing between patterns or technologies\n   - Example: \"Add real-time updates\" - WebSockets vs SSE vs polling\n   - Example: \"Implement state management\" - Redux vs Context vs custom solution\n\n5. **Multi-File Changes**: The task will likely touch more than 2-3 files\n   - Example: \"Refactor the authentication system\"\n   - Example: \"Add a new API endpoint with tests\"\n\n6. **Unclear Requirements**: You need to explore before understanding the full scope\n   - Example: \"Make the app faster\" - need to profile and identify bottlenecks\n   - Example: \"Fix the bug in checkout\" - need to investigate root cause\n\n7. **User Preferences Matter**: The implementation could reasonably go multiple ways\n   - If you would use AskUserQuestion to clarify the approach, use EnterPlanMode instead\n   - Plan mode lets you explore first, then present options with context\n\n## When NOT to Use This Tool\n\nOnly skip EnterPlanMode for simple tasks:\n- Single-line or few-line fixes (typos, obvious bugs, small tweaks)\n- Adding a single function with clear requirements\n- Tasks where the user has given very specific, detailed instructions\n- Pure research/exploration tasks (use the Agent tool instead)\n\n## What Happens in Plan Mode\n\nIn plan mode, you'll:\n1. Thoroughly explore the codebase using Glob, Grep, and Read\n2. Understand existing patterns and architecture\n3. Design an implementation approach\n4. Present your plan to the user for approval\n5. Use AskUserQuestion if you need to clarify approaches\n6. Exit plan mode with ExitPlanMode when ready to implement\n\n## Examples\n\n### GOOD - Use EnterPlanMode:\nUser: \"Add user authentication to the app\"\n- Requires architectural decisions (session vs JWT, where to store tokens, middleware structure)\n\nUser: \"Optimize the database queries\"\n- Multiple approaches possible, need to profile first, significant impact\n\nUser: \"Implement dark mode\"\n- Architectural decision on theme system, affects many components\n\nUser: \"Add a delete button to the user profile\"\n- Seems simple but involves: where to place it, confirmation dialog, API call, error handling, state updates\n\nUser: \"Update the error handling in the API\"\n- Affects multiple files, user should approve the approach\n\n### BAD - Don't use EnterPlanMode:\nUser: \"Fix the typo in the README\"\n- Straightforward, no planning needed\n\nUser: \"Add a console.log to debug this function\"\n- Simple, obvious implementation\n\nUser: \"What files handle routing?\"\n- Research task, not implementation planning\n\n## Important Notes\n\n- Entering plan mode returns the session plan file path; write the plan there with Write/Edit. Project writes stay blocked until the user approves the plan\n- If unsure whether to use it, err on the side of planning - it's better to get alignment upfront than to redo work\n- Users appreciate being consulted before significant changes are made to their codebase".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        },
        // `plan` is still accepted from older transcripts but no longer advertised:
        // like the reference CLI, the plan is read from the session plan file.
        crate::tools::ToolDefinition {
            name: "ExitPlanMode".into(),
            description: "Use this tool when you are in plan mode and have finished writing your plan to the plan file and are ready for user approval.\n\n## How This Tool Works\n- You should have already written your plan to the plan file path that EnterPlanMode returned\n- This tool does NOT take the plan content as a parameter - it will read the plan from the file you wrote\n- This tool simply signals that you're done planning and ready for the user to review and approve\n- The user will see the contents of your plan file when they review it\n\n## When to Use This Tool\nIMPORTANT: Only use this tool when the task requires planning the implementation steps of a task that requires writing code. For research tasks where you're gathering information, searching files, reading files or in general trying to understand the codebase - do NOT use this tool.\n\n## Before Using This Tool\nEnsure your plan is complete and unambiguous:\n- If you have unresolved questions about requirements or approach, use AskUserQuestion first (in earlier phases)\n- Once your plan is finalized, use THIS tool to request approval\n\n**Important:** Do NOT use AskUserQuestion to ask \"Is this plan okay?\" or \"Should I proceed?\" - that's exactly what THIS tool does. ExitPlanMode inherently requests user approval of your plan.\n\n## Examples\n\n1. Initial task: \"Search for and understand the implementation of vim mode in the codebase\" - Do not use the exit plan mode tool because you are not planning the implementation steps of a task.\n2. Initial task: \"Help me implement yank mode for vim\" - Use the exit plan mode tool after you have finished planning the implementation steps of the task.\n3. Initial task: \"Add a new feature to handle user authentication\" - If unsure about auth method (OAuth, JWT, etc.), use AskUserQuestion first, then use exit plan mode tool after clarifying the approach.".into(),
            input_schema: json!({"type":"object","properties":{"allowedPrompts":{"description":"Semantic descriptions of Bash actions the plan needs, recorded for the user's review; they never grant arbitrary Bash.","type":"array","maxItems":32,"items":{"type":"object","properties":{"tool":{"description":"The tool this prompt applies to","type":"string","enum":["Bash"]},"prompt":{"description":"Semantic description of the action, e.g. \"run tests\", \"install dependencies\"","type":"string"}},"required":["tool","prompt"],"additionalProperties":false}}},"additionalProperties":{}}),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_documents_are_durable_isolated_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        assert!(read(root.path(), "one").unwrap().is_none());
        save(root.path(), "one", "# Plan\n检查 UTF-8").unwrap();
        assert_eq!(
            read(root.path(), "one").unwrap().unwrap(),
            "# Plan\n检查 UTF-8"
        );
        assert!(read(root.path(), "two").unwrap().is_none());
        save(root.path(), "one", "# Revised").unwrap();
        assert_eq!(read(root.path(), "one").unwrap().unwrap(), "# Revised");
        assert!(save(root.path(), "one", " ").is_err());
        assert!(save(root.path(), "one", &"x".repeat(MAX_PLAN_BYTES + 1)).is_err());
        assert!(path(root.path(), "../escape")
            .unwrap()
            .starts_with(root.path().canonicalize().unwrap().join("plans")));
    }
    #[test]
    fn document_tools_are_exact_scoped_bounded_and_atomic() {
        use serde_json::json;
        let root = tempfile::tempdir().unwrap();
        let target = path(root.path(), "one").unwrap();
        let write = json!({"file_path":target,"content":"# Initial plan\nTest twice twice"});
        assert!(is_document_tool(root.path(), "one", "Write", &write));
        assert!(!is_document_tool(root.path(), "two", "Write", &write));
        assert!(!is_document_tool(root.path(), "one", "Bash", &write));
        for name in [
            target.file_name().unwrap().to_string_lossy().into_owned(),
            format!("{}:other", target.display()),
            format!(
                "{}/../plans/{}",
                target.parent().unwrap().display(),
                target.file_name().unwrap().to_string_lossy()
            ),
            root.path()
                .join("project.rs")
                .to_string_lossy()
                .into_owned(),
        ] {
            assert!(
                !is_document_tool(root.path(), "one", "Write", &json!({"file_path":name})),
                "{name}"
            );
        }
        let written = edit_document(root.path(), "one", "Write", &write).unwrap();
        assert_eq!(written["file_change"]["verified"], true);
        assert_eq!(written["file_change"]["added"], 2);
        assert_eq!(written["file_change"]["removed"], 0);
        assert!(edit_document(
            root.path(),
            "one",
            "Edit",
            &json!({"file_path":target,"old_string":"twice","new_string":"once"})
        )
        .is_err());
        let edited = edit_document(root.path(), "one", "Edit", &json!({"file_path":target,"old_string":"twice","new_string":"once","replace_all":true})).unwrap();
        assert_eq!(edited["file_change"]["added"], 1);
        assert_eq!(edited["file_change"]["removed"], 1);
        assert!(edited["file_change"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["kind"] == "removed" && row["text"] == "Test twice twice" }));
        assert_eq!(
            read(root.path(), "one").unwrap().unwrap(),
            "# Initial plan\nTest once once"
        );
        assert!(edit_document(
            root.path(),
            "one",
            "Write",
            &json!({"file_path":target,"content":"x".repeat(MAX_PLAN_BYTES+1)})
        )
        .is_err());
        assert_eq!(
            read(root.path(), "one").unwrap().unwrap(),
            "# Initial plan\nTest once once"
        );
        // Replacing a hardlinked file must not mutate its other name.
        let other = root.path().join("unrelated.md");
        std::fs::hard_link(&target, &other).unwrap();
        save(root.path(), "one", "# Replacement").unwrap();
        assert_eq!(
            std::fs::read_to_string(other).unwrap(),
            "# Initial plan\nTest once once"
        );
    }

    #[test]
    fn linked_plan_and_plan_directory_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let original = outside.path().join("original.md");
        std::fs::write(&original, "untouched").unwrap();
        let target = path(root.path(), "one").unwrap();
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&original, &target);
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&original, &target);
        if linked.is_ok() {
            assert!(read(root.path(), "one").is_err());
            assert!(save(root.path(), "one", "bad").is_err());
            std::fs::remove_file(&target).unwrap();
        } else {
            eprintln!("File symlink creation unavailable; directory link check still runs");
        }
        std::fs::remove_dir(target.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), target.parent().unwrap()).unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(target.parent().unwrap())
                .arg(outside.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(read(root.path(), "one").is_err());
        assert!(save(root.path(), "one", "bad").is_err());
        assert_eq!(std::fs::read_to_string(original).unwrap(), "untouched");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn directory_guard_prevents_parent_replacement_during_io() {
        let root = tempfile::tempdir().unwrap();
        let (target, guards) = guard_directory(root.path(), "one", true).unwrap();
        assert!(!guards.is_empty());
        assert!(std::fs::rename(target.parent().unwrap(), root.path().join("moved")).is_err());
        drop(guards);
        std::fs::rename(target.parent().unwrap(), root.path().join("moved")).unwrap();
    }
    #[test]
    fn trusted_runtime_root_alias_resolves_but_plan_directory_alias_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let actual = tempfile::tempdir().unwrap();
        let alias = temp.path().join("data-alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(actual.path(), &alias).unwrap();
        #[cfg(windows)]
        {
            let result = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&alias)
                .arg(actual.path())
                .output()
                .unwrap();
            assert!(result.status.success());
        }
        let saved = save(&alias, "session", "# Plan").unwrap();
        assert!(saved.starts_with(actual.path().canonicalize().unwrap()));
        assert_eq!(
            read(actual.path(), "session").unwrap().as_deref(),
            Some("# Plan")
        );
        assert_eq!(
            path(&alias, "session").unwrap(),
            path(actual.path(), "session").unwrap()
        );
    }
}
