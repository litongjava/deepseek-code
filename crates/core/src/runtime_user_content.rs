//! Canonical multimodal user input shared by CLI persistence and model history.
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

pub fn normalize_user_content(content: &Value) -> Result<Value> {
    if let Some(text) = content.as_str() {
        ensure!(!text.trim().is_empty(), "User message is empty");
        return Ok(content.clone());
    }
    let blocks = content
        .as_array()
        .context("User content must be text or a block array")?;
    ensure!(
        !blocks.is_empty() && blocks.len() <= 64,
        "User content requires 1..64 blocks"
    );
    ensure!(
        serde_json::to_vec(content)?.len() <= 8 * 1024 * 1024,
        "User content exceeds 8 MiB"
    );
    let mut result = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str().context("Text block needs text")?;
                ensure!(
                    block
                        .as_object()
                        .unwrap()
                        .keys()
                        .all(|k| matches!(k.as_str(), "type" | "text")),
                    "Unsupported text block metadata"
                );
                result.push(json!({"type":"text","text":text}));
            }
            Some("image") | Some("image_url") => {
                let source_key = if block["type"] == "image" {
                    "source"
                } else {
                    "image_url"
                };
                ensure!(
                    block.as_object().unwrap().keys().all(|k| k == "type"
                        || k == source_key
                        || matches!(k.as_str(), "fileName" | "size")),
                    "Unsupported image block metadata"
                );
                let (mime, data) = if block["type"] == "image" {
                    let source = &block["source"];
                    ensure!(source["type"]=="base64","Only embedded base64 images are supported; remote image URLs are not fetched");
                    ensure!(
                        source
                            .as_object()
                            .context("image source must be an object")?
                            .keys()
                            .all(|k| matches!(k.as_str(), "type" | "media_type" | "data")),
                        "Unsupported image source metadata"
                    );
                    (
                        source["media_type"]
                            .as_str()
                            .context("Image needs media_type")?,
                        source["data"].as_str().context("Image needs base64 data")?,
                    )
                } else {
                    let image = &block["image_url"];
                    ensure!(
                        image
                            .as_object()
                            .context("image_url must be an object")?
                            .keys()
                            .all(|k| k == "url"),
                        "Image detail/extra image_url fields are not supported"
                    );
                    image["url"]
                        .as_str()
                        .and_then(|s| s.strip_prefix("data:"))
                        .and_then(|s| s.split_once(";base64,"))
                        .context("image_url must contain an embedded base64 data URL")?
                };
                ensure!(
                    matches!(
                        mime,
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    ),
                    "Unsupported image media type"
                );
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .context("Invalid image base64")?;
                ensure!(
                    !decoded.is_empty() && decoded.len() <= 5 * 1024 * 1024,
                    "Image must contain 1 byte to 5 MiB"
                );
                let valid = match mime {
                    "image/png" => decoded.starts_with(b"\x89PNG\r\n\x1a\n"),
                    "image/jpeg" => decoded.starts_with(&[0xff, 0xd8, 0xff]),
                    "image/gif" => decoded.starts_with(b"GIF87a") || decoded.starts_with(b"GIF89a"),
                    "image/webp" => {
                        decoded.starts_with(b"RIFF") && decoded.get(8..12) == Some(b"WEBP")
                    }
                    _ => false,
                };
                ensure!(valid, "Image bytes do not match declared media type");
                let mut normalized = json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}")}});
                if let Some(name) = block.get("fileName") {
                    let name = name.as_str().context("Image fileName must be text")?;
                    ensure!(
                        !name.is_empty() && name.len() <= 1024,
                        "Invalid image fileName"
                    );
                    normalized["fileName"] = json!(name);
                }
                if let Some(size) = block.get("size") {
                    ensure!(
                        size.as_u64() == Some(decoded.len() as u64),
                        "Image size does not match its bytes"
                    );
                    normalized["size"] = json!(decoded.len());
                }
                result.push(normalized);
            }
            Some("attachment") => {
                attachment_text(block)?;
                result.push(block.clone());
            }
            other => bail!("Unsupported user content block: {other:?}"),
        }
    }
    ensure!(
        result.iter().any(
            |b| matches!(b["type"].as_str(), Some("image_url" | "attachment"))
                || b["text"].as_str().is_some_and(|t| !t.trim().is_empty())
        ),
        "User message is empty"
    );
    Ok(Value::Array(result))
}

/// Validate self-contained attachments and derive provider text from their saved bytes.
/// No path or upload reference supplied by a client is opened here.
pub(crate) fn attachment_text(block: &Value) -> Result<String> {
    ensure!(
        block
            .as_object()
            .context("Attachment must be an object")?
            .keys()
            .all(|key| matches!(key.as_str(), "type" | "fileName" | "size" | "source")),
        "Unsupported attachment metadata"
    );
    let name = block["fileName"]
        .as_str()
        .context("Attachment needs fileName")?;
    ensure!(
        !name.is_empty() && name.len() <= 1024 && !name.chars().any(char::is_control),
        "Invalid attachment fileName"
    );
    let source = &block["source"];
    ensure!(
        source["type"] == "base64"
            && source
                .as_object()
                .context("Attachment source must be an object")?
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "media_type" | "data")),
        "Only embedded base64 attachments are supported"
    );
    let mime = source["media_type"]
        .as_str()
        .context("Attachment needs media_type")?;
    ensure!(
        mime.len() <= 255 && !mime.chars().any(char::is_control),
        "Invalid attachment media type"
    );
    let data = source["data"]
        .as_str()
        .context("Attachment needs base64 data")?;
    ensure!(data.len() <= 7 * 1024 * 1024, "Attachment exceeds 5 MiB");
    let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
    ensure!(bytes.len() <= 5 * 1024 * 1024, "Attachment exceeds 5 MiB");
    ensure!(
        block["size"].as_u64() == Some(bytes.len() as u64),
        "Attachment size does not match its bytes"
    );
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let text = if mime == "application/pdf" || ext == "pdf" {
        ensure!(bytes.starts_with(b"%PDF-"), "Attachment is not a valid PDF");
        let text = pdf_extract::extract_text_from_mem(&bytes).context("Cannot extract PDF text")?;
        ensure!(
            !text.trim().is_empty(),
            "PDF has no extractable text; attach page images instead"
        );
        text
    } else {
        ensure!(
            mime.starts_with("text/")
                || matches!(
                    ext.as_str(),
                    "txt"
                        | "md"
                        | "csv"
                        | "json"
                        | "xml"
                        | "yaml"
                        | "yml"
                        | "js"
                        | "jsx"
                        | "ts"
                        | "tsx"
                        | "py"
                        | "java"
                        | "cpp"
                        | "c"
                        | "h"
                        | "cs"
                        | "go"
                        | "rs"
                        | "rb"
                        | "php"
                        | "swift"
                        | "kt"
                        | "scala"
                        | "html"
                        | "css"
                        | "scss"
                        | "less"
                        | "sql"
                        | "sh"
                        | "bash"
                        | "vue"
                        | "svelte"
                        | "lua"
                        | "r"
                        | "m"
                        | "pl"
                        | "ex"
                        | "exs"
                ),
            "Queued files support UTF-8 text/code and text PDFs; this file format is not supported"
        );
        let text = String::from_utf8(bytes).context("Text attachment must use UTF-8")?;
        ensure!(!text.contains('\0'), "Text attachment contains binary data");
        text
    };
    ensure!(
        text.len() <= 1024 * 1024,
        "Extracted attachment text exceeds 1 MiB"
    );
    Ok(format!("Attached file: {name}\n\n{text}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attachments_retain_bytes_and_reject_invalid_or_unsupported_content() {
        let text = "中文 notes\ncode = 42;";
        let block = json!({"type":"attachment","fileName":"notes.txt","size":text.len(),
            "source":{"type":"base64","media_type":"text/plain","data":base64::engine::general_purpose::STANDARD.encode(text)}});
        assert_eq!(
            normalize_user_content(&json!([block])).unwrap(),
            json!([block])
        );
        assert_eq!(
            attachment_text(&block).unwrap(),
            format!("Attached file: notes.txt\n\n{text}")
        );
        for (field, value) in [
            ("size", json!(1)),
            ("fileName", json!("bad\nname")),
            ("path", json!("local.txt")),
        ] {
            let mut invalid = block.clone();
            invalid[field] = value;
            assert!(normalize_user_content(&json!([invalid])).is_err());
        }
        for (name, mime, bytes) in [
            ("bad.txt", "text/plain", vec![0xff]),
            ("binary.txt", "text/plain", vec![0]),
            ("bad.pdf", "application/pdf", b"not a pdf".to_vec()),
            (
                "office.docx",
                "application/octet-stream",
                b"PK binary".to_vec(),
            ),
        ] {
            let invalid = json!({"type":"attachment","fileName":name,"size":bytes.len(),
                "source":{"type":"base64","media_type":mime,"data":base64::engine::general_purpose::STANDARD.encode(bytes)}});
            assert!(normalize_user_content(&json!([invalid])).is_err());
        }
    }
    #[test]
    fn pdf_attachment_extracts_saved_document_text() {
        let bytes = include_bytes!("../tests/fixtures/queue-text.pdf");
        let block = json!({"type":"attachment","fileName":"queue-text.pdf","size":bytes.len(),
            "source":{"type":"base64","media_type":"application/pdf","data":base64::engine::general_purpose::STANDARD.encode(bytes)}});
        assert!(attachment_text(&block).unwrap().contains("PDF_QUEUE_CODE_7391"));
        assert_eq!(normalize_user_content(&json!([block])).unwrap(), json!([block]));
    }
    #[test]
    fn invalid_images_and_unhandled_content_are_rejected_without_dropping_fields() {
        for value in [
            json!([{"type":"image","source":{"type":"url","url":"https://example.invalid/p.png"}}]),
            json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"not-base64!"}}]),
            json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}]),
            json!([{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo=","detail":"high"}}]),
            json!([{"type":"text","text":"hello","citations":[{}]}]),
            json!([{"type":"document","source":{}}]),
            json!([]),
        ] {
            assert!(normalize_user_content(&value).is_err(), "{value}");
        }
        assert_eq!(
            normalize_user_content(&json!("你好")).unwrap(),
            json!("你好")
        );
    }

    #[test]
    fn image_display_metadata_survives_normalization_and_is_validated() {
        let content = json!([{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="},"fileName":"截图.png","size":8}]);
        assert_eq!(normalize_user_content(&content).unwrap(), content);
        for (field, value) in [
            ("fileName", json!(12)),
            ("size", json!(9)),
            ("path", json!("local.png")),
        ] {
            let mut invalid = content.clone();
            invalid[0][field] = value;
            assert!(normalize_user_content(&invalid).is_err());
        }
    }
}

/// Largest image file attached from a path or the clipboard.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// The media type of a supported image file, from its extension.
pub fn image_media_type(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// An embedded base64 image block for a PNG, JPEG, GIF or WebP file.
pub fn image_block_from_file(path: &std::path::Path) -> Result<Value> {
    let media_type =
        image_media_type(path).context("Only PNG, JPEG, GIF and WebP images can be attached")?;
    let size = std::fs::metadata(path)?.len();
    ensure!(
        size <= MAX_IMAGE_BYTES,
        "Image {} is larger than 5 MB",
        path.display()
    );
    let data = base64::engine::general_purpose::STANDARD.encode(std::fs::read(path)?);
    Ok(
        serde_json::json!({"type":"image","source":{"type":"base64","media_type":media_type,"data":data}}),
    )
}

#[cfg(test)]
mod image_file_tests {
    use super::*;

    #[test]
    fn image_files_become_normalized_base64_blocks() {
        let dir = tempfile::tempdir().unwrap();
        // A real 1x1 PNG; normalization checks the image bytes.
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY9sAAAAASUVORK5CYII=";
        let png = dir.path().join("shot.PNG");
        std::fs::write(
            &png,
            base64::engine::general_purpose::STANDARD
                .decode(PNG)
                .unwrap(),
        )
        .unwrap();
        let block = image_block_from_file(&png).unwrap();
        assert_eq!(block["source"]["media_type"], "image/png");
        assert_eq!(block["source"]["data"], PNG);
        assert!(
            normalize_user_content(&serde_json::json!([{"type":"text","text":"see"}, block]))
                .is_ok()
        );
        assert!(image_block_from_file(&dir.path().join("notes.txt")).is_err());
    }
}
