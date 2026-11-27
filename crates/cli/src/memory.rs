use deepseek_code_core::db::session_store::GlobalSetting;
use crate::args::MemoryCommands;
use anyhow::{ensure, Result};
use deepseek_code_core::{memory::MemExClient, runtime::RuntimePaths};

const LIMIT: usize = 100;

fn manual_metadata() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([("type".to_owned(), "manual".to_owned())])
}

async fn find(client: &MemExClient, id: &str) -> Result<deepseek_code_core::memory::MemoryItem> {
    client.search("", Some(LIMIT)).await?.into_iter().find(|item| item.id == id)
        .ok_or_else(|| anyhow::anyhow!("Memory not found: {id}"))
}

/// One row of the `/memory` list: the first line, shortened by the picker.
fn title(item: &deepseek_code_core::memory::MemoryItem) -> String {
    item.content.lines().next().unwrap_or_default().to_owned()
}

fn detail(item: &deepseek_code_core::memory::MemoryItem) -> String {
    let date = item.created_at.get(..10).unwrap_or(&item.created_at);
    let origin = if item.metadata.as_ref().and_then(|metadata| metadata.get("type")).is_some_and(|kind| kind == "manual") { "manual" } else { "conversation" };
    format!("{date} \u{b7} {origin} \u{b7} importance {:.1}", item.importance)
}

/// `/memory` long-term entries: browse and search, then view, edit or delete.
pub async fn manage(paths: &RuntimePaths) -> Result<()> {
    use crate::terminal::picker::{details, text_input, Entry, Key, Picked, Picker};
    let client = MemExClient::with_path(paths.data_dir.join("data/memory.sqlite3"));
    let mut selected = 0;
    loop {
        let items = client.search("", Some(LIMIT)).await?;
        let stats = client.stats().await?;
        let subtitle = format!("{} saved \u{b7} shared with the desktop app", stats.total_memories);
        let mut entries = vec![Entry { title: "+ Add a memory".into(), detail: String::new() }];
        entries.extend(items.iter().map(|item| Entry { title: title(item), detail: detail(item) }));
        let picker = Picker { heading: "Long-term memories", subtitle: Some(&subtitle), entries: &entries, search: true, hint: "Type to search \u{b7} Enter to view \u{b7} Ctrl+D to delete \u{b7} Esc to close", selected };
        let index = match picker.run_keys(&[Key::Ctrl('d')])? {
            Picked::Entry(index) => index,
            Picked::Key(Key::Ctrl('d'), Some(index)) if index > 0 => {
                selected = index;
                client.delete(&items[index - 1].id).await?;
                continue;
            }
            Picked::Key(..) => continue,
            Picked::Cancel => return Ok(()),
        };
        selected = index;
        if index == 0 {
            if let Some(content) = text_input("New memory", "")?.filter(|text| !text.trim().is_empty()) {
                client.ingest(&content, Some(0.7), Some(manual_metadata())).await?;
            }
            continue;
        }
        let item = &items[index - 1];
        let mut rows = vec![detail(item), String::new()];
        rows.extend(item.content.lines().map(crate::background_commands::safe_output));
        match details("Memory", &rows, "\u{2190} to go back \u{b7} e to edit \u{b7} x to delete \u{b7} Esc to close", &['e', 'x'])? {
            Some('e') => {
                if let Some(content) = text_input("Edit memory", &item.content)?.filter(|text| !text.trim().is_empty()) {
                    client.update(&item.id, &content, item.importance).await?;
                }
            }
            Some('x') => { client.delete(&item.id).await?; }
            Some('<') => {}
            _ => return Ok(()),
        }
    }
}

pub async fn run(
    cli: &crate::args::Cli,
    paths: &RuntimePaths,
    action: &MemoryCommands,
) -> Result<()> {
    let client = MemExClient::with_path(paths.data_dir.join("data/memory.sqlite3"));
    match action {
        MemoryCommands::Files {
            session,
            project,
            action,
        } => {
            use crate::args::FileMemoryCommands;
            use deepseek_code_core::services::memory_files::{FileMemory, Selection};
            let context = crate::services::context(paths)?;
            let memory = if session.is_some() || project.is_some() {
                FileMemory::select(
                    &context,
                    &Selection {
                        session_id: session.clone(),
                        project_id: project.clone(),
                    },
                )?
            } else {
                FileMemory::workspace(
                    &context,
                    &cli.path.clone().unwrap_or(std::env::current_dir()?),
                )?
            };
            let result = match action {
                FileMemoryCommands::Repair { confirm } => {
                    memory.repair_catalog(confirm.as_deref())?
                }
                FileMemoryCommands::List { query } => {
                    memory.listing(query.as_deref().unwrap_or_default())?
                }
                FileMemoryCommands::Put {
                    name,
                    source,
                    version,
                } => memory.put(name, &std::fs::read_to_string(source)?, version.as_deref())?,
                FileMemoryCommands::Delete { name, version } => memory.delete(name, version)?,
            };
            crate::services::print(&result)
        }

        MemoryCommands::Dream | MemoryCommands::AutoDream => {
            let selected = crate::config::select(cli, paths, None, None)?;
            let generator = deepseek_code_core::services::generation::Generator::new(paths, selected.providers, &selected.provider_id, &selected.model)?;
            let context = crate::services::context(paths)?;
            tokio::select! {
                result = deepseek_code_core::services::dream::run(&context, &generator, false) => crate::services::print(&result?),
                _ = crate::signals::interrupted() => anyhow::bail!("Dream cancelled"),
            }
        }
        MemoryCommands::DreamStatus => {
            let context = crate::services::context(paths)?;
            crate::services::print(&serde_json::json!({"memory":client.dream_status().await?, "config":context.global_setting::<deepseek_code_core::services::dream::DreamConfig>(GlobalSetting::MachinePreference("auto-dream"))?.unwrap_or_default(), "services":deepseek_code_core::services::lifecycle::status(&context)?}))
        }
        MemoryCommands::DreamConfig { enabled, min_hours, min_memories } => {
            let selected = crate::config::select(cli, paths, None, None)?;
            let config = deepseek_code_core::services::dream::DreamConfig { enabled:*enabled, provider:selected.provider_id, model:selected.model, min_hours:*min_hours, min_memories:*min_memories };
            config.validate()?;
            deepseek_code_core::services::generation::Generator::saved(paths, &config.provider, &config.model)?;
            crate::services::context(paths)?.save_global_setting(GlobalSetting::MachinePreference("auto-dream"), &config)?;
            crate::services::print(&config)
        }
        MemoryCommands::Status => crate::services::print(&client.stats().await?),
        MemoryCommands::List { query, limit } => {
            crate::services::print(&client.search(query.as_deref().unwrap_or(""), Some(*limit)).await?)
        }
        MemoryCommands::Add { content, importance } => {
            client.ingest(content, Some(*importance), Some(manual_metadata())).await?;
            crate::services::print(&serde_json::json!({"saved": true}))
        }
        MemoryCommands::Update { id, content, importance } => {
            let importance = match importance {
                Some(importance) => *importance,
                None => find(&client, id).await?.importance,
            };
            ensure!(client.update(id, content, importance).await?, "Memory not found: {id}");
            crate::services::print(&serde_json::json!({"updated": id}))
        }
        MemoryCommands::Delete { id } => {
            ensure!(client.delete(id).await?, "Memory not found: {id}");
            crate::services::print(&serde_json::json!({"deleted": id}))
        }
        MemoryCommands::Clear { yes } => {
            ensure!(
                *yes,
                "Pass --yes to clear personal memory; conversations and team memory are retained"
            );
            client.clear().await?;
            crate::services::print(&serde_json::json!({"cleared": true}))
        }
        MemoryCommands::Export { output } => {
            let archive = client.export_archive().await?;
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            file.write_all(&serde_json::to_vec_pretty(&archive)?)?;
            file.sync_all()?;
            crate::services::print(
                &serde_json::json!({"exported": archive.memories.len(), "output": output}),
            )
        }
        MemoryCommands::Import { input, preview } => {
            let value = serde_json::from_slice(&std::fs::read(input)?)?;
            crate::services::print(&client.import_archive(value, *preview).await?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli() -> crate::args::Cli {
        crate::args::Cli::parse_from(["dcl", "--print"])
    }

    #[tokio::test]
    async fn entries_can_be_added_listed_updated_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().to_path_buf()).unwrap();
        let client = MemExClient::with_path(paths.data_dir.join("data/memory.sqlite3"));
        let cli = cli();
        run(&cli, &paths, &MemoryCommands::Add { content: "Use PostgreSQL".into(), importance: 0.8 }).await.unwrap();
        let item = client.search("", Some(10)).await.unwrap().remove(0);
        assert_eq!(item.metadata.as_ref().and_then(|metadata| metadata.get("type")).map(String::as_str), Some("manual"));
        assert!(detail(&item).contains("manual \u{b7} importance 0.8"));

        run(&cli, &paths, &MemoryCommands::Update { id: item.id.clone(), content: "Use SQLite".into(), importance: None }).await.unwrap();
        let updated = client.search("SQLite", Some(10)).await.unwrap().remove(0);
        assert_eq!((updated.id.as_str(), updated.importance), (item.id.as_str(), 0.8));

        run(&cli, &paths, &MemoryCommands::Delete { id: item.id.clone() }).await.unwrap();
        assert!(client.search("", Some(10)).await.unwrap().is_empty());
        for missing in [
            MemoryCommands::Delete { id: item.id.clone() },
            MemoryCommands::Update { id: item.id.clone(), content: "x".into(), importance: Some(0.5) },
            MemoryCommands::Update { id: item.id.clone(), content: "x".into(), importance: None },
        ] {
            assert!(run(&cli, &paths, &missing).await.unwrap_err().to_string().contains("Memory not found"));
        }
    }

    #[tokio::test]
    async fn file_memory_commands_enforce_scope_and_cas() {
        use crate::args::FileMemoryCommands;
        use deepseek_code_core::services::memory_files::FileMemory;
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let mut cli = cli();
        cli.path = Some(workspace.clone());
        let source = root.path().join("draft.md");
        std::fs::write(&source, "---\nname: 中文 CLI\ndescription: Command fixture\nmetadata:\n  type: user\n---\nOriginal fact\n").unwrap();
        let files = |action| MemoryCommands::Files {
            session: None,
            project: None,
            action,
        };
        run(
            &cli,
            &paths,
            &files(FileMemoryCommands::Put {
                name: "cli.md".into(),
                source: source.clone(),
                version: None,
            }),
        )
        .await
        .unwrap();
        let context = crate::services::context(&paths).unwrap();
        let memory = FileMemory::workspace(&context, &workspace).unwrap();
        let first = memory
            .store(false)
            .unwrap()
            .read("cli.md")
            .unwrap()
            .unwrap();
        assert!(first.document.body.contains("Original fact"));
        assert!(run(
            &cli,
            &paths,
            &files(FileMemoryCommands::Put {
                name: "cli.md".into(),
                source: source.clone(),
                version: None
            })
        )
        .await
        .is_err());
        std::fs::write(
            &source,
            String::from_utf8(first.bytes.clone())
                .unwrap()
                .replace("Original fact", "Updated fact"),
        )
        .unwrap();
        run(
            &cli,
            &paths,
            &files(FileMemoryCommands::Put {
                name: "cli.md".into(),
                source,
                version: Some(first.hash.clone()),
            }),
        )
        .await
        .unwrap();
        assert!(run(
            &cli,
            &paths,
            &files(FileMemoryCommands::Delete {
                name: "cli.md".into(),
                version: first.hash
            })
        )
        .await
        .is_err());
        let updated = memory
            .store(false)
            .unwrap()
            .read("cli.md")
            .unwrap()
            .unwrap();
        assert!(updated.document.body.contains("Updated fact"));
        run(
            &cli,
            &paths,
            &files(FileMemoryCommands::List {
                query: Some("中文".into()),
            }),
        )
        .await
        .unwrap();
        run(
            &cli,
            &paths,
            &files(FileMemoryCommands::Delete {
                name: "cli.md".into(),
                version: updated.hash,
            }),
        )
        .await
        .unwrap();
        assert!(memory
            .store(false)
            .unwrap()
            .read("cli.md")
            .unwrap()
            .is_none());
        assert!(!paths.data_dir.join("data/memory.sqlite3").exists());
        assert!(crate::args::Cli::try_parse_from([
            "dcl",
            "memory",
            "files",
            "--session",
            "s",
            "--project",
            "p",
            "list"
        ])
        .is_err());
        assert!(
            crate::args::Cli::try_parse_from(["dcl", "memory", "files", "delete", "cli.md"])
                .is_err()
        );
    }

    #[tokio::test]
    async fn invalid_edits_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let client = MemExClient::with_path(dir.path().join("data/memory.sqlite3"));
        client.ingest("keep me", Some(0.7), None).await.unwrap();
        let id = client.search("", Some(1)).await.unwrap().remove(0).id;
        assert!(client.update(&id, "   ", 0.5).await.is_err());
        assert!(client.update(&id, "text", 1.5).await.is_err());
        assert_eq!(client.search("", Some(1)).await.unwrap()[0].content, "keep me");
    }
}

/// Session-bound file memory panel; each selected row carries its CAS version.
pub async fn manage_files(paths: &RuntimePaths, session: &str) -> Result<()> {
    use crate::terminal::picker::{details, text_input, Entry, Picked, Picker};
    let context = crate::services::context(paths)?;
    let memory =
        deepseek_code_core::services::memory_files::FileMemory::session(&context, session)?;
    loop {
        let listing = memory.listing("")?;
        let items = listing["items"].as_array().unwrap();
        let mut entries = vec![Entry {
            title: "+ Add a Markdown memory".into(),
            detail: String::new(),
        }];
        entries.extend(items.iter().map(|item| Entry {
            title: item["name"].as_str().unwrap_or_default().into(),
            detail: item["description"].as_str().unwrap_or_default().into(),
        }));
        let scope = format!("Project scope {}", memory.scope.key());
        let picker = Picker {
            heading: "File memories",
            subtitle: Some(&scope),
            entries: &entries,
            search: true,
            hint: "Enter to view · Esc to close",
            selected: 0,
        };
        let Picked::Entry(index) = picker.run_keys(&[])? else {
            return Ok(());
        };
        if index == 0 {
            let Some(filename) = text_input("Filename (lowercase .md)", "")? else {
                continue;
            };
            edit_memory_draft(
                &memory,
                &filename,
                "---\nname: \ndescription: \nmetadata:\n  type: user\n---\n",
                None,
            )
            .await?;
            continue;
        }
        let item = &items[index - 1];
        let rows = item["content"]
            .as_str()
            .unwrap_or_default()
            .lines()
            .map(crate::background_commands::safe_output)
            .collect::<Vec<_>>();
        match details(
            "Memory",
            &rows,
            "e to edit · x to delete · Esc to close",
            &['e', 'x'],
        )? {
            Some('e') => {
                edit_memory_draft(
                    &memory,
                    item["id"].as_str().unwrap(),
                    item["content"].as_str().unwrap_or_default(),
                    item["version"].as_str(),
                )
                .await?;
            }
            Some('x') => {
                if text_input("Type yes to delete this memory", "")?.as_deref() == Some("yes") {
                    memory.delete(
                        item["id"].as_str().unwrap(),
                        item["version"].as_str().unwrap(),
                    )?;
                }
            }
            Some('<') => {}
            _ => return Ok(()),
        }
    }
}

async fn edit_memory_draft(
    memory: &deepseek_code_core::services::memory_files::FileMemory,
    filename: &str,
    content: &str,
    version: Option<&str>,
) -> Result<()> {
    let draft = tempfile::Builder::new()
        .prefix("dcl-memory-")
        .suffix(".md")
        .tempfile()?;
    std::fs::write(draft.path(), content)?;
    let result = async {
        crate::display_commands::edit_file(draft.path(), content).await?;
        let content = std::fs::read_to_string(draft.path())?;
        memory.put(filename, &content, version)?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        let (_, path) = draft.keep()?;
        return Err(error.context(format!(
            "Draft preserved at {}; read the latest version before submitting again",
            path.display()
        )));
    }
    Ok(())
}
