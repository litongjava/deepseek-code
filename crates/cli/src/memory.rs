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
    let client = MemExClient::with_path(paths.data_dir.join("memory.sqlite3"), None);
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

pub async fn run(cli: &crate::args::Cli, paths: &RuntimePaths, action: &MemoryCommands) -> Result<()> {
    let client = MemExClient::with_path(paths.data_dir.join("memory.sqlite3"), None);
    match action {
        MemoryCommands::Dream | MemoryCommands::AutoDream => {
            let selected = crate::config::select(cli, paths, None, None)?;
            let generator = deepseek_code_core::services::generation::Generator::new(paths, selected.providers, &selected.provider_id, &selected.model)?;
            let context = crate::services::context(paths)?;
            tokio::select! {
                result = deepseek_code_core::services::dream::run(&context, &generator, false) => crate::services::print(&result?),
                _ = tokio::signal::ctrl_c() => anyhow::bail!("Dream cancelled"),
            }
        }
        MemoryCommands::DreamStatus => {
            let context = crate::services::context(paths)?;
            crate::services::print(&serde_json::json!({"memory":client.dream_status().await?, "config":context.setting::<deepseek_code_core::services::dream::DreamConfig>("auto-dream")?.unwrap_or_default(), "services":deepseek_code_core::services::lifecycle::status(&context)?}))
        }
        MemoryCommands::DreamConfig { enabled, min_hours, min_memories } => {
            let selected = crate::config::select(cli, paths, None, None)?;
            let config = deepseek_code_core::services::dream::DreamConfig { enabled:*enabled, provider:selected.provider_id, model:selected.model, min_hours:*min_hours, min_memories:*min_memories };
            config.validate()?;
            deepseek_code_core::services::generation::Generator::saved(paths, &config.provider, &config.model)?;
            crate::services::context(paths)?.save_setting("auto-dream", &config)?;
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
        let client = MemExClient::with_path(paths.data_dir.join("memory.sqlite3"), None);
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
    async fn invalid_edits_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let client = MemExClient::with_path(dir.path().join("memory.sqlite3"), None);
        client.ingest("keep me", Some(0.7), None).await.unwrap();
        let id = client.search("", Some(1)).await.unwrap().remove(0).id;
        assert!(client.update(&id, "   ", 0.5).await.is_err());
        assert!(client.update(&id, "text", 1.5).await.is_err());
        assert_eq!(client.search("", Some(1)).await.unwrap()[0].content, "keep me");
    }
}
