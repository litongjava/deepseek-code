use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{io::Write, path::Path};

pub(super) fn save(directory: &Path, encoded: &str) -> Result<(String, &'static str)> {
    let bytes = STANDARD
        .decode(encoded.trim())
        .context("Invalid screenshot Base64")?;
    let (extension, mime) = if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        ("jpg", "image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ("png", "image/png")
    } else {
        anyhow::bail!("Screenshot is not JPEG or PNG");
    };
    std::fs::create_dir_all(directory).context("Cannot create screenshot directory")?;
    let path = directory.join(format!("screenshot-{}.{}", uuid::Uuid::new_v4(), extension));
    ensure!(path.is_absolute(), "Screenshot directory must be absolute");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(&bytes).context("Cannot save screenshot")?;
    let canonical = path.canonicalize()?;
    let receipts = directory.join(".generated");
    std::fs::create_dir_all(&receipts)?;
    let receipt = serde_json::json!({"path":canonical,"sha256":STANDARD.encode(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref())});
    std::fs::write(
        receipts.join(path.file_name().unwrap()),
        serde_json::to_vec(&receipt)?,
    )?;
    Ok((path.to_string_lossy().into_owned(), mime))
}

/// Only unchanged files emitted by the screenshot tool receive the Read exception.
pub(crate) fn readable(path: &Path) -> bool {
    crate::runtime::resolve_default_data_dir()
        .ok()
        .is_some_and(|root| verified(&root.join("screenshots"), path).unwrap_or(false))
}

fn verified(directory: &Path, path: &Path) -> Result<bool> {
    let canonical = path.canonicalize()?;
    let root = directory.canonicalize()?;
    if canonical.parent() != Some(root.as_path()) || !canonical.is_file() {
        return Ok(false);
    }
    let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(
        root.join(".generated").join(canonical.file_name().unwrap()),
    )?)?;
    if receipt["path"] != serde_json::to_value(&canonical)? {
        return Ok(false);
    }
    let bytes = std::fs::read(&canonical)?;
    Ok(receipt["sha256"]
        == STANDARD.encode(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_generated_unchanged_files_receive_read_access() {
        let root = tempfile::tempdir().unwrap();
        let (path, _) = save(root.path(), &STANDARD.encode(b"\xff\xd8\xffimage")).unwrap();
        assert!(verified(root.path(), Path::new(&path)).unwrap());
        let other = root.path().join("manual.jpg");
        std::fs::copy(&path, &other).unwrap();
        assert!(!verified(root.path(), &other).unwrap_or(false));
        std::fs::write(&path, b"replacement").unwrap();
        assert!(!verified(root.path(), Path::new(&path)).unwrap());
    }

    #[test]
    fn saves_original_bytes_with_matching_extension_and_unique_path() {
        let root = tempfile::tempdir().unwrap();
        for (bytes, extension, mime) in [
            (&b"\xff\xd8\xffexample"[..], "jpg", "image/jpeg"),
            (&b"\x89PNG\r\n\x1a\nexample"[..], "png", "image/png"),
        ] {
            let encoded = STANDARD.encode(bytes);
            let (path, actual_mime) = save(root.path(), &encoded).unwrap();
            assert_eq!(actual_mime, mime);
            assert_eq!(Path::new(&path).extension().unwrap(), extension);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_ne!(save(root.path(), &encoded).unwrap().0, path);
        }
    }

    #[test]
    fn rejects_invalid_data_and_unwritable_destination() {
        let root = tempfile::tempdir().unwrap();
        assert!(save(root.path(), "not base64").is_err());
        assert!(save(root.path(), "").is_err());
        let file = root.path().join("file");
        std::fs::write(&file, b"occupied").unwrap();
        assert!(save(&file, &STANDARD.encode(b"\xff\xd8\xfftest")).is_err());
    }
}
