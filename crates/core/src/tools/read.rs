//! Read, aligned with the reference CLI: text with line numbers, images as
//! model image content, PDFs by page range and notebooks as cells.
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::Path;

/// Text over this size needs offset/limit or allow_large.
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_PDF_PAGES: usize = 20;
const PDF_PAGES_REQUIRED_OVER: usize = 10;

pub fn definition() -> super::ToolDefinition {
    super::ToolDefinition {
        name: "Read".into(),
        description: "Reads a file from the local filesystem.\n\n- `file_path` must be an absolute path.\n- Reads up to 2000 lines by default.\n- When you already know which part of the file you need, only read that part. This can be important for larger files.\n- Results are returned using cat -n format, with line numbers starting at 1\n- Reads images (PNG, JPG, GIF, WebP) and presents them visually when the selected model accepts images. Reads PDFs as text via the `pages` parameter (e.g. \"1-5\", max 20 pages/request; required for PDFs over 10 pages). Reads Jupyter notebooks (.ipynb) as cells with outputs.\n- Reading a directory, a missing file, or an empty file returns an error or system reminder rather than content.\n- Do NOT re-read a file you just edited to verify — Edit/Write would have errored if the change failed.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": {"description": "The absolute path to the file to read", "type": "string"},
                "offset": {"description": "The line number to start reading from. Only provide if the file is too large to read at once", "type": "integer", "minimum": 0},
                "limit": {"description": "The number of lines to read. Only provide if the file is too large to read at once.", "type": "integer", "exclusiveMinimum": 0},
                "pages": {"description": "Page range for PDF files (e.g., \"1-5\", \"3\", \"10-20\"). Only applicable to PDF files. Maximum 20 pages per request.", "type": "string"},
                "allow_large": {"description": "Set to true to read a text file, or a line range of one, that is over the usual size limits, up to what still fits in your context. Only use this when you genuinely need all of it or the user asked for the whole file; otherwise read it in parts with offset and limit, or search it.", "type": "boolean"}
            },
            "required": ["file_path"],
            "additionalProperties": false
        }),
    }
}

pub fn execute(input: Value, cwd: &str) -> Result<Value> {
    let file_path = input["file_path"].as_str().ok_or_else(|| anyhow!("file_path required"))?;
    if let Some(bytes) = crate::skills::bundled_resources::read(Path::new(file_path)) {
        return text(std::str::from_utf8(bytes)?, &input);
    }
    let path = if super::screenshot_file::readable(Path::new(file_path)) {
        Path::new(file_path).canonicalize()?.to_string_lossy().into_owned()
    } else {
        super::resolve_path(file_path, cwd)?
    };
    let path = Path::new(&path);
    if !path.exists() {
        return Ok(json!({"content": format!("File not found: {}", path.display()), "is_error": true}));
    }
    if path.is_dir() {
        return Ok(json!({"content": format!("{} is a directory, not a file. Use Bash `ls` or Glob to list it.", path.display()), "is_error": true}));
    }
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if let Some(media_type) = image_media_type(&extension) {
        return image(path, media_type);
    }
    if extension == "pdf" {
        return pdf(path, input["pages"].as_str());
    }
    anyhow::ensure!(input.get("pages").map_or(true, Value::is_null), "pages applies only to PDF files");
    let content = std::fs::read_to_string(path).map_err(|error| anyhow!("Cannot read {} as UTF-8 text: {error}", path.display()))?;
    if extension == "ipynb" {
        if let Ok(notebook) = serde_json::from_str::<Value>(&content) {
            return Ok(json!({"content": notebook_cells(&notebook), "cells": notebook["cells"].as_array().map_or(0, Vec::len)}));
        }
    }
    text(&content, &input)
}

fn text(content: &str, input: &Value) -> Result<Value> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    if content.is_empty() {
        return Ok(json!({"content": "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>", "lines": 0, "truncated": false}));
    }
    // Line numbers are 1-based; 0 is accepted as the first line like the reference.
    let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = input["limit"].as_u64().unwrap_or(2000) as usize;
    anyhow::ensure!(limit > 0, "limit must be positive");
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    if offset > total_lines {
        return Ok(json!({"content": format!("<system-reminder>Warning: the file has {total_lines} lines; offset {offset} is past the end.</system-reminder>"), "lines": total_lines, "truncated": false}));
    }
    let selected: Vec<String> = lines.iter().enumerate().skip(offset - 1).take(limit).map(|(i, line)| format!("{:>6}\t{}", i + 1, line)).collect();
    let rendered = selected.join("\n");
    if rendered.len() > MAX_TEXT_BYTES && input["allow_large"].as_bool() != Some(true) {
        bail!("File content ({} KB) exceeds the {} KB read limit. Read it in parts with offset and limit, search it with Grep, or pass allow_large=true if you genuinely need all of it.", rendered.len() / 1024, MAX_TEXT_BYTES / 1024);
    }
    Ok(json!({"content": rendered, "lines": total_lines, "truncated": offset - 1 + selected.len() < total_lines}))
}

fn image_media_type(extension: &str) -> Option<&'static str> {
    match extension {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// The tool loop turns `image` into model image content when the model accepts it.
fn image(path: &Path, media_type: &str) -> Result<Value> {
    let bytes = std::fs::read(path)?;
    anyhow::ensure!(bytes.len() <= MAX_IMAGE_BYTES, "Image is {} MB; the limit is {} MB", bytes.len() / 1024 / 1024, MAX_IMAGE_BYTES / 1024 / 1024);
    use base64::Engine;
    Ok(json!({
        "content": format!("Image file {} ({media_type}, {} bytes)", path.display(), bytes.len()),
        "image": {"media_type": media_type, "data": base64::engine::general_purpose::STANDARD.encode(&bytes)}
    }))
}

fn pdf(path: &Path, pages: Option<&str>) -> Result<Value> {
    let bytes = std::fs::read(path)?;
    let texts = pdf_extract::extract_text_from_mem_by_pages(&bytes).map_err(|error| anyhow!("Cannot extract PDF text from {}: {error}", path.display()))?;
    let total = texts.len();
    let (first, last) = match pages {
        Some(range) => parse_pages(range, total)?,
        None => {
            anyhow::ensure!(total <= PDF_PAGES_REQUIRED_OVER, "This PDF has {total} pages; pass pages (for example \"1-5\", at most {MAX_PDF_PAGES} pages per request).");
            (1, total.max(1))
        }
    };
    let content = (first..=last.min(total)).map(|page| format!("--- Page {page} ---\n{}", texts[page - 1].trim_end())).collect::<Vec<_>>().join("\n\n");
    Ok(json!({"content": content, "pages": total}))
}

fn parse_pages(range: &str, total: usize) -> Result<(usize, usize)> {
    let parse = |value: &str| value.trim().parse::<usize>().map_err(|_| anyhow!("Invalid pages value: {range}"));
    let (first, last) = match range.split_once('-') {
        Some((first, last)) => (parse(first)?, parse(last)?),
        None => { let page = parse(range)?; (page, page) }
    };
    anyhow::ensure!(first >= 1 && first <= last, "Invalid pages range: {range}");
    anyhow::ensure!(last - first < MAX_PDF_PAGES, "At most {MAX_PDF_PAGES} pages per request");
    anyhow::ensure!(first <= total, "The PDF has {total} pages");
    Ok((first, last))
}

fn source(value: &Value) -> String {
    match value {
        Value::Array(lines) => lines.iter().filter_map(Value::as_str).collect(),
        Value::String(text) => text.clone(),
        _ => String::new(),
    }
}

fn notebook_cells(notebook: &Value) -> String {
    let mut out = Vec::new();
    for (index, cell) in notebook["cells"].as_array().into_iter().flatten().enumerate() {
        let id = cell["id"].as_str().map(str::to_owned).unwrap_or_else(|| format!("cell-{index}"));
        let mut text = format!("<cell id=\"{id}\" type=\"{}\">\n{}\n</cell>", cell["cell_type"].as_str().unwrap_or("code"), source(&cell["source"]));
        for output in cell["outputs"].as_array().into_iter().flatten() {
            let body = if output["text"].is_null() { source(&output["data"]["text/plain"]) } else { source(&output["text"]) };
            let body = if body.is_empty() && !output["data"].is_null() { "[non-text output]".to_owned() } else { body };
            if !body.is_empty() {
                text.push_str(&format!("\n<output>\n{}\n</output>", body.trim_end()));
            }
        }
        out.push(text);
    }
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ranges_notebooks_images_and_reports_limits() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let out = execute(json!({"file_path":"a.txt","offset":2,"limit":1}), cwd).unwrap();
        assert_eq!(out["content"], "     2\ttwo");
        assert_eq!(out["truncated"], true);
        assert_eq!(execute(json!({"file_path":"a.txt","offset":0}), cwd).unwrap()["content"], "     1\tone\n     2\ttwo\n     3\tthree");

        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        assert!(execute(json!({"file_path":"empty.txt"}), cwd).unwrap()["content"].as_str().unwrap().contains("empty"));
        assert_eq!(execute(json!({"file_path":"."}), cwd).unwrap()["is_error"], true);

        let big = "x".repeat(1000) + "\n";
        std::fs::write(dir.path().join("big.txt"), big.repeat(400)).unwrap();
        assert!(execute(json!({"file_path":"big.txt"}), cwd).unwrap_err().to_string().contains("allow_large"));
        assert_eq!(execute(json!({"file_path":"big.txt","allow_large":true}), cwd).unwrap()["lines"], 400);

        std::fs::write(dir.path().join("n.ipynb"), r#"{"cells":[{"id":"c1","cell_type":"code","source":["print(1)\n"],"outputs":[{"output_type":"stream","text":["1\n"]}]}]}"#).unwrap();
        let notebook = execute(json!({"file_path":"n.ipynb"}), cwd).unwrap();
        assert!(notebook["content"].as_str().unwrap().contains("<cell id=\"c1\" type=\"code\">\nprint(1)\n\n</cell>\n<output>\n1\n</output>"));

        std::fs::write(dir.path().join("p.png"), b"\x89PNG").unwrap();
        let image = execute(json!({"file_path":"p.png"}), cwd).unwrap();
        assert_eq!(image["image"]["media_type"], "image/png");
        assert_eq!(image["image"]["data"], "iVBORw==");
        assert!(execute(json!({"file_path":"a.txt","pages":"1"}), cwd).is_err());
    }

    #[test]
    fn page_ranges_are_bounded() {
        assert_eq!(parse_pages("3", 5).unwrap(), (3, 3));
        assert_eq!(parse_pages("2-4", 5).unwrap(), (2, 4));
        assert!(parse_pages("1-21", 30).is_err());
        assert!(parse_pages("6", 5).is_err());
        assert!(parse_pages("4-2", 5).is_err());
    }
}
