//! Messages the user sends while a turn runs. As in the reference CLI, a
//! running turn reads them after its next tool results; whatever is still
//! queued when the turn ends becomes the next prompt.
use crate::db::DbManager;
use anyhow::Result;

pub fn push(db: &DbManager, session: &str, text: &str) -> Result<()> {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO session_input_queue (session_id, content, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![session, text, chrono::Utc::now().to_rfc3339()],
        )
    })??;
    Ok(())
}

/// Takes every queued message, oldest first; each is delivered exactly once.
pub fn take(db: &DbManager, session: &str) -> Result<Vec<String>> {
    db.with_conn(|conn| -> Result<Vec<String>> {
        let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let texts = {
            let mut statement = tx.prepare("SELECT content FROM session_input_queue WHERE session_id = ?1 ORDER BY id")?;
            let rows = statement.query_map([session], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute("DELETE FROM session_input_queue WHERE session_id = ?1", [session])?;
        tx.commit()?;
        Ok(texts)
    })?
}

/// Takes the queued messages `wanted` selects, oldest first, and leaves
/// the rest queued.
pub fn take_where(db: &DbManager, session: &str, wanted: impl Fn(&str) -> bool) -> Result<Vec<String>> {
    db.with_conn(|conn| -> Result<Vec<String>> {
        let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let rows = {
            let mut statement = tx.prepare("SELECT id, content FROM session_input_queue WHERE session_id = ?1 ORDER BY id")?;
            let rows = statement.query_map([session], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut texts = Vec::new();
        for (id, text) in rows.into_iter().filter(|(_, text)| wanted(text)) {
            tx.execute("DELETE FROM session_input_queue WHERE id = ?1", [id])?;
            texts.push(text);
        }
        tx.commit()?;
        Ok(texts)
    })?
}

/// Whether a message `wanted` selects is queued.
pub fn any(db: &DbManager, session: &str, wanted: impl Fn(&str) -> bool) -> Result<bool> {
    db.with_conn(|conn| -> Result<bool> {
        let mut statement = conn.prepare("SELECT content FROM session_input_queue WHERE session_id = ?1")?;
        let rows = statement.query_map([session], |row| row.get::<_, String>(0))?;
        for text in rows {
            if wanted(&text?) {
                return Ok(true);
            }
        }
        Ok(false)
    })?
}

fn pasted_images_file(data_dir: &std::path::Path, session: &str) -> Option<std::path::PathBuf> {
    session
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        .then(|| data_dir.join("pasted-images").join(format!("{session}.json")))
}

/// Records the images pasted in a session's frontend, so a queued message's
/// `[Image #N]` can be resolved by whichever process delivers it.
pub fn save_pasted_images(data_dir: &std::path::Path, session: &str, paths: &[std::path::PathBuf]) -> Result<()> {
    let file = pasted_images_file(data_dir, session).ok_or_else(|| anyhow::anyhow!("Invalid session id"))?;
    std::fs::create_dir_all(file.parent().expect("pasted-images directory"))?;
    std::fs::write(file, serde_json::to_vec(paths)?)?;
    Ok(())
}

/// The pasted images a queued message names with `[Image #N]`, in order.
pub fn pasted_images(data_dir: &std::path::Path, session: &str, text: &str) -> Vec<std::path::PathBuf> {
    let Some(file) = pasted_images_file(data_dir, session) else {
        return Vec::new();
    };
    let Ok(paths) = std::fs::read(file).map_err(anyhow::Error::from).and_then(|bytes| Ok(serde_json::from_slice::<Vec<std::path::PathBuf>>(&bytes)?)) else {
        return Vec::new();
    };
    let mut found: Vec<std::path::PathBuf> = Vec::new();
    for number in text.split("[Image #").skip(1).filter_map(|rest| rest.split_once(']')?.0.parse::<usize>().ok()) {
        if let Some(path) = number.checked_sub(1).and_then(|index| paths.get(index)) {
            if path.is_file() && !found.contains(path) {
                found.push(path.clone());
            }
        }
    }
    found
}

/// A queued message as content blocks: its text, then each image it names.
/// The image's path is left out: with it, models tend to inspect the file
/// with tools instead of looking at the image.
pub fn content_with_images(data_dir: &std::path::Path, session: &str, text: &str) -> Option<Vec<serde_json::Value>> {
    let paths = pasted_images(data_dir, session, text);
    if paths.is_empty() {
        return None;
    }
    let mut blocks = vec![serde_json::json!({"type": "text", "text": text})];
    for path in paths {
        if let Ok(block) = crate::runtime::image_block_from_file(&path) {
            blocks.push(block);
        }
    }
    (blocks.len() > 1).then_some(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_messages_are_taken_once_in_order_per_session() {
        let temp = tempfile::tempdir().unwrap();
        let db = DbManager::new(temp.path().join("db.sqlite")).unwrap();
        db.init().unwrap();
        push(&db, "a", "first").unwrap();
        push(&db, "b", "other").unwrap();
        push(&db, "a", "second").unwrap();
        assert_eq!(take(&db, "a").unwrap(), ["first", "second"]);
        assert!(take(&db, "a").unwrap().is_empty());
        assert_eq!(take(&db, "b").unwrap(), ["other"]);
        push(&db, "a", "typed").unwrap();
        push(&db, "a", "<note>").unwrap();
        let note = |text: &str| text.starts_with('<');
        assert!(any(&db, "a", note).unwrap());
        assert_eq!(take_where(&db, "a", |text| !note(text)).unwrap(), ["typed"]);
        assert_eq!(take(&db, "a").unwrap(), ["<note>"]);
    }

    #[test]
    fn queued_messages_resolve_pasted_image_placeholders() {
        let temp = tempfile::tempdir().unwrap();
        let png = temp.path().join("shot.png");
        std::fs::write(&png, base64::Engine::decode(&base64::engine::general_purpose::STANDARD, "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY9sAAAAASUVORK5CYII=").unwrap()).unwrap();
        save_pasted_images(temp.path(), "s-1", &[temp.path().join("gone.png"), png.clone()]).unwrap();
        assert!(content_with_images(temp.path(), "s-1", "no images").is_none());
        assert!(content_with_images(temp.path(), "s-1", "missing [Image #1]").is_none());
        let blocks = content_with_images(temp.path(), "s-1", "look [Image #2]").unwrap();
        assert_eq!(blocks[0]["text"], "look [Image #2]");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks.len(), 2);
        assert!(save_pasted_images(temp.path(), "../x", &[]).is_err());
    }
}
