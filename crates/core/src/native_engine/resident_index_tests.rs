//! Provider-body coverage of every scope and each durable window writer.
use super::*;

#[tokio::test]
async fn authored_project_index_survives_listing_topic_crud_and_compaction() -> Result<()> {
    use crate::services::{compaction::GeneratedSummary, memory_files::FileMemory};
    use std::fs;

    let root = tempfile::tempdir()?;
    let ws = root.path().join("project");
    fs::create_dir(&ws)?;
    let ctx = crate::services::session_messages::context(&root.path().join("data"))?;
    let shard =
        ctx.db
            .create_code_session(&ws, "model", "provider", &uuid::Uuid::new_v4().to_string())?;
    let project = FileMemory::session(&ctx, shard.id())?;
    let tool = |name: &str, input: Value| -> Result<Value> {
        let filename = project.target(name, &input, ws.to_str().unwrap())?.unwrap();
        project.tool(name, &input, &filename, false)
    };
    let index = project.directory.join("MEMORY.md");
    let marker = "AUTHORED-PROJECT-CHECKLIST";
    let authored = format!("# Checklist\r\n{marker}\r\n");
    tool("Write", json!({"file_path":index,"content":authored}))?;
    let check_disk = || assert_eq!(fs::read(&index).unwrap(), authored.as_bytes());
    let check_request = |bytes: Vec<u8>| {
        let request: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap().matches(marker).count(),
            1
        );
        let first_user = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "user")
            .unwrap();
        assert!(first_user["content"].to_string().contains(marker));
        assert!(!request.to_string().contains("MEMORY.md changed."));
    };
    check_disk();
    shard.admit_prompt("before listing", None, "typed")?;
    check_request(memory_request_bytes(&shard, "openai").await);
    let generation = project.store(true)?.snapshot()?.generation;
    let listed = project.listing("")?;
    assert_eq!(listed["index"], authored);
    assert_eq!(listed["generation"], generation);
    check_disk();
    shard.admit_prompt("after listing", None, "typed")?;
    check_request(memory_request_bytes(&shard, "openai").await);

    let topic = project.directory.join("sibling.md");
    tool(
        "Write",
        json!({"file_path":topic,"content":"---\nname: Sibling\ndescription: Sibling topic\nmetadata:\n  type: user\n---\nold body\n"}),
    )?;
    check_disk();
    tool(
        "Edit",
        json!({"file_path":topic,"old_string":"old body","new_string":"new body"}),
    )?;
    check_disk();
    tool(
        "ApplyPatch",
        json!({"patch":format!("*** Begin Patch\n*** Delete File: {}\n*** End Patch", topic.display())}),
    )?;
    assert!(!topic.exists());
    check_disk();
    shard.admit_prompt("after deletion", None, "typed")?;
    check_request(memory_request_bytes(&shard, "openai").await);

    let prepared = shard.repository().prepare_compaction(None)?;
    shard.repository().commit_compaction(
        &prepared,
        &GeneratedSummary {
            text: "summary".into(),
            native: None,
        },
        None,
    )?;
    shard.admit_prompt("after compact", None, "typed")?;
    check_request(memory_request_bytes(&shard, "openai").await);
    Ok(())
}

#[test]
fn memory_fork_copies_and_deletes_only_its_own_directory() -> Result<()> {
    use crate::db::session_store::Branch;
    use std::fs;

    let root = tempfile::tempdir()?;
    let ws = root.path().join("project");
    fs::create_dir(&ws)?;
    let ctx = crate::services::session_messages::context(&root.path().join("data"))?;
    let parent =
        ctx.db
            .create_code_session(&ws, "model", "provider", &uuid::Uuid::new_v4().to_string())?;
    let parent_dir = parent.memory_directory();
    fs::create_dir_all(parent_dir.join("nested"))?;
    fs::write(parent_dir.join("MEMORY.md"), "index")?;
    fs::write(parent_dir.join("nested/topic.md"), "original")?;
    let fork = parent.branch("fork", Branch::Full)?;
    let fork_dir = fork.memory_directory();
    assert_ne!(parent_dir, fork_dir);
    assert_eq!(fs::read(fork_dir.join("MEMORY.md"))?, b"index");
    fs::write(parent_dir.join("nested/topic.md"), "edited")?;
    assert_eq!(fs::read(fork_dir.join("nested/topic.md"))?, b"original");
    fork.delete()?;
    assert!(!fork_dir.exists());
    assert_eq!(fs::read(parent_dir.join("nested/topic.md"))?, b"edited");
    Ok(())
}

#[tokio::test]
async fn memory_resident_scopes_across_boundaries_and_resume() -> Result<()> {
    use crate::db::session_store::{settings::MEMORY_AGENT, Stores};
    use crate::services::{
        compaction::{GeneratedSummary, Part},
        memory_files::FileMemory,
    };
    use std::fs;
    const RESUME: &str = "DCL_RESIDENT_MATRIX_ROOT";
    if let Some(root) = std::env::var_os(RESUME) {
        let root = std::path::PathBuf::from(root);
        let session = Stores::open(&root.join("data"))?
            .open_session(&std::env::var("DCL_MATRIX_SESSION")?)?;
        let bytes = memory_request_bytes(&session, &std::env::var("DCL_MATRIX_PROTOCOL")?).await;
        assert_eq!(bytes, fs::read(root.join("expected"))?);
        return Ok(());
    }

    for child in [false, true] {
        for kind in [
            "summary",
            "native",
            "queued-summary",
            "queued-native",
            "live-summary",
            "live-native",
            "partial-up-to",
            "partial-from",
            "plan",
            "overflow",
        ] {
            let root = tempfile::tempdir()?;
            let data = root.path().join("data");
            let ws = root.path().join("project");
            fs::create_dir(&ws)?;
            let ctx = crate::services::session_messages::context(&data)?;
            let parent = ctx.db.create_code_session(
                &ws,
                "model",
                "provider",
                &uuid::Uuid::new_v4().to_string(),
            )?;
            let mut shard = if child {
                fs::create_dir_all(parent.memory_directory())?;
                fs::write(
                    parent.memory_directory().join("MEMORY.md"),
                    "PARENT-ONLY-INDEX",
                )?;
                let child = parent.derive_owned_child("child-session", &ws)?;
                assert!(!child.memory_directory().join("MEMORY.md").exists());
                child.write(|w| w.set_setting(MEMORY_AGENT, &"reviewer".into()))?;
                child
            } else {
                parent.clone()
            };
            let project = FileMemory::session(&ctx, shard.id())?;
            let agent = project.directory.parent().unwrap().join("agents/reviewer");
            let team = root.path().join("team");
            crate::data_layout::write_config_file(
                crate::data_layout::DataLayout::from_root(&data).config_file("settings.json"),
                json!({"memory":{"team_dir":team}}).to_string(),
            )?;
            let dirs = [
                ("global", data.join("data/memory/global")),
                ("team", team),
                ("project", project.directory.clone()),
                ("subagent", agent),
                ("session", shard.memory_directory()),
            ];
            for (scope, directory) in &dirs {
                fs::create_dir_all(directory)?;
                let mut first = format!("FIRST-{scope}!\n");
                // Exercise both raw limits in every scope, before XML escaping.
                if kind == "summary" {
                    first.push_str(&"<&>\n".repeat(199));
                    first.push_str("UNSEEN-INDEX-TAIL");
                } else if kind == "native" {
                    first.push_str(&"é".repeat(12800));
                    first.push_str("UNSEEN-INDEX-TAIL");
                }
                fs::write(directory.join("MEMORY.md"), first)?;
                fs::write(directory.join("topic.md"), "HIDDEN-MEMORY-FILE-BODY")?;
            }
            let visible = dirs
                .iter()
                .filter(|(scope, _)| child || *scope != "subagent")
                .collect::<Vec<_>>();
            let protocol = if kind.contains("native") {
                "responses"
            } else if child {
                "anthropic"
            } else {
                "openai"
            };
            shard.admit_prompt("topic", None, if child { "delegation" } else { "typed" })?;
            let first = memory_request_bytes(&shard, protocol).await;
            let wire = std::str::from_utf8(&first)?;
            let mut offset = 0;
            for (scope, dir) in &visible {
                let at = wire.find(&format!("FIRST-{scope}!")).unwrap();
                let memory_path = dir.join("MEMORY.md");
                let memory_path = memory_path.to_string_lossy();
                let unescaped_wire = wire.replace("\\\\", "\\");
                let wire_path = unescaped_wire
                    .strip_prefix(r"\\?\")
                    .unwrap_or(&unescaped_wire)
                    .replace('\\', "/")
                    .replace("//?/", "");
                let memory_path = memory_path
                    .strip_prefix(r"\\?\")
                    .unwrap_or(&memory_path)
                    .replace('\\', "/");
                assert!(
                    at > offset && wire_path.contains(&memory_path),
                    "{kind} {scope}"
                );
                offset = at;
            }
            assert_eq!(wire.matches("FIRST-").count(), visible.len());
            if matches!(kind, "summary" | "native") {
                assert_eq!(wire.matches("[MEMORY.md truncated;").count(), visible.len());
                assert!(!wire.contains("UNSEEN-INDEX-TAIL"));
            }
            assert!(
                !wire.contains("HIDDEN-MEMORY-FILE-BODY") && !wire.contains("PARENT-ONLY-INDEX")
            );
            let mut kept = String::new();
            for (scope, dir) in &visible {
                if kind == "summary" {
                    let input = json!({"file_path":dir.join("MEMORY.md"), "content":format!("CHANGE-{scope}!")});
                    for tool in ["Read", "Write"] {
                        let filename = project.target(tool, &input, ws.to_str().unwrap())?.unwrap();
                        project.tool(tool, &input, &filename, false)?;
                    }
                } else {
                    fs::write(dir.join("MEMORY.md"), format!("CHANGE-{scope}!"))?;
                }
                kept = shard.admit_prompt("next topic", None, "typed")?;
                let bytes = memory_request_bytes(&shard, protocol).await;
                let wire = std::str::from_utf8(&bytes)?;
                assert_eq!(wire.matches("FIRST-").count(), visible.len());
                let last = wire.rsplit_once("MEMORY.md changed.").unwrap().1;
                assert!(last.contains(&format!("CHANGE-{scope}!")));
                assert_eq!(last.matches("CHANGE-").count(), 1);
                assert!(!last.contains("FIRST-") && !wire.contains("HIDDEN-MEMORY-FILE-BODY"));
            }
            for (scope, dir) in &visible {
                fs::write(dir.join("MEMORY.md"), format!("BOUNDARY-{scope}!"))?;
            }
            let summary = GeneratedSummary {text:"summary".into(),native:kind.contains("native").then(||json!({"api":"openai-responses","compaction":true,"items":[{"type":"compaction","encrypted_content":"opaque-test"}]}))};
            match kind {
                "plan" => {
                    shard.approve_plan("approved")?;
                }
                "partial-from" | "partial-up-to" => {
                    let part = if kind == "partial-from" {
                        Part::From
                    } else {
                        Part::UpTo
                    };
                    let prepared = shard.prepare_summary_part(&kept, part)?;
                    let (branch, _) = crate::services::compaction::commit_part(
                        &ctx.db,
                        shard.id(),
                        &kept,
                        part,
                        prepared,
                        "summary",
                    )?;
                    shard = ctx.db.open_session(&branch)?;
                }
                "queued-summary" | "queued-native" => {
                    shard.write(|w| w.request_compaction("keep facts", 50, 100))?;
                    let owner = shard.delivery_owner()?;
                    let claim = owner.claim_compaction()?.unwrap();
                    owner.finish_compaction(&claim, &summary)?;
                }
                "live-summary" | "live-native" => {
                    let cut = shard.read(|r| {
                        let (messages, _) = r.model_history(&ApiFormat::OpenAI)?;
                        r.compaction_window(&messages, None, None, &ApiFormat::OpenAI)
                    })?;
                    shard.commit_compaction_window(&cut, &summary)?;
                }
                _ => {
                    let prepared = shard
                        .repository()
                        .prepare_compaction((kind == "overflow").then_some(kept.as_str()))?;
                    shard
                        .repository()
                        .commit_compaction(&prepared, &summary, None)?;
                }
            }
            shard.admit_prompt("unchanged", None, "typed")?;
            let bytes = memory_request_bytes(&shard, protocol).await;
            let wire = std::str::from_utf8(&bytes)?;
            for (scope, _) in &visible {
                assert_eq!(
                    wire.matches(&format!("BOUNDARY-{scope}!")).count(),
                    1,
                    "{kind} {scope}"
                );
            }
            assert!(
                !wire.contains("CHANGE-")
                    && !wire.contains("FIRST-")
                    && !wire.contains("HIDDEN-MEMORY-FILE-BODY")
                    && !wire.contains("MEMORY.md changed."),
                "{kind}"
            );
            let unescaped_wire = wire.replace("\\\\", "\\");
            let memory_dir = shard.memory_directory();
            let memory_dir = memory_dir.to_string_lossy();
            assert!(
                unescaped_wire.contains(memory_dir.as_ref())
                    || unescaped_wire.contains(&memory_dir.replace('\\', "/"))
            );
            assert_eq!(bytes, memory_request_bytes(&shard, protocol).await);
            fs::write(root.path().join("expected"), bytes)?;
            let resume = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    concat!(
                        module_path!(),
                        "::memory_resident_scopes_across_boundaries_and_resume"
                    )
                    .split_once("::")
                    .unwrap()
                    .1,
                ])
                .env(RESUME, root.path())
                .env("DCL_MATRIX_PROTOCOL", protocol)
                .env("DCL_MATRIX_SESSION", shard.id())
                .output()?;
            assert!(
                resume.status.success()
                    && String::from_utf8_lossy(&resume.stdout).contains("1 passed"),
                "{kind} {}",
                String::from_utf8_lossy(&resume.stderr)
            );
            fs::write(dirs[0].1.join("MEMORY.md"), "AFTER-global!")?;
            fs::write(shard.memory_directory().join("MEMORY.md"), "AFTER-session!")?;
            shard.admit_prompt("new change", None, "typed")?;
            let bytes = memory_request_bytes(&shard, protocol).await;
            let wire = std::str::from_utf8(&bytes)?;
            assert_eq!(wire.matches("MEMORY.md changed.").count(), 1, "{kind}");
            let (_, reminder) = wire.split_once("MEMORY.md changed.").unwrap();
            assert_eq!(reminder.matches("AFTER-global!").count(), 1);
            assert_eq!(reminder.matches("AFTER-session!").count(), 1);
            assert!(!reminder.contains("BOUNDARY-"));
        }
    }
    Ok(())
}
