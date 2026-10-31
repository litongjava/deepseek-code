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
    Ok((path.to_string_lossy().into_owned(), mime))
}

#[cfg(test)]
mod tests {
    use super::*;

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
