//! Images in a prompt, as in the reference CLI: image files dragged into the
//! terminal or mentioned by path, and images pasted from the clipboard with
//! Ctrl+V (saved under the data directory). Pasted images, and pasted or
//! dragged image paths, show in the prompt as `[Image #N]`.
use anyhow::Result;
use claude_core::runtime::{image_block_from_file, image_media_type};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const MAX_IMAGES: usize = 5;

/// The images pasted this session; `[Image #N]` names the Nth.
static PASTED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Remembers a pasted image and returns its placeholder, `[Image #N]`.
pub fn attach(path: PathBuf) -> String {
    let mut pasted = PASTED.lock().unwrap_or_else(|error| error.into_inner());
    let number = match pasted.iter().position(|known| *known == path) {
        Some(index) => index + 1,
        None => {
            pasted.push(path);
            pasted.len()
        }
    };
    format!("[Image #{number}]")
}

/// The image file a paste consists of, as a terminal drops a dragged file:
/// a path, possibly quoted or with backslash-escaped spaces.
pub fn pasted_path(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    let text = text
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .or_else(|| text.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')))
        .map(str::to_owned)
        .unwrap_or_else(|| text.replace("\\ ", " "));
    let text = text.strip_prefix("file://").unwrap_or(&text);
    let path = Path::new(text);
    (text.lines().count() == 1 && path.is_absolute() && image_media_type(path).is_some() && path.is_file()).then(|| path.to_path_buf())
}

/// The pasted images a prompt names with `[Image #N]`, in order.
fn placeholders(prompt: &str) -> Vec<PathBuf> {
    let pasted = PASTED.lock().unwrap_or_else(|error| error.into_inner());
    prompt
        .split("[Image #")
        .skip(1)
        .filter_map(|rest| rest.split_once(']')?.0.parse::<usize>().ok())
        .filter_map(|number| pasted.get(number.checked_sub(1)?).cloned())
        .collect()
}

/// Image paths written in the prompt: plain, quoted or after @.
fn image_paths(workspace: &Path, prompt: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for path in placeholders(prompt) {
        if path.is_file() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    for word in prompt.split_whitespace() {
        let word = word
            .trim_start_matches(|ch| matches!(ch, '"' | '\'' | '@' | '('))
            .trim_end_matches(|ch| matches!(ch, '"' | '\'' | ',' | ';' | ')' | '?' | '!'));
        let candidate = Path::new(word);
        if image_media_type(candidate).is_none() {
            continue;
        }
        let path = if candidate.is_absolute() { candidate.to_path_buf() } else { workspace.join(candidate) };
        if path.is_file() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths.truncate(MAX_IMAGES);
    paths
}

/// User content with the prompt text and its images, when it names any.
pub fn content(workspace: &Path, prompt: &str) -> Result<Option<Value>> {
    let paths = image_paths(workspace, prompt);
    if paths.is_empty() {
        return Ok(None);
    }
    let text = crate::mentions::expand(workspace, prompt).unwrap_or_else(|| prompt.to_owned());
    let mut blocks = vec![json!({"type": "text", "text": text})];
    for path in paths {
        blocks.push(image_block_from_file(&path)?);
    }
    Ok(Some(Value::Array(blocks)))
}

fn run(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    (output.status.success() && !output.stdout.is_empty()).then_some(output.stdout)
}

/// Saves a clipboard image as PNG under `<data dir>/pasted-images`.
pub fn paste_clipboard() -> Result<Option<PathBuf>> {
    let dir = claude_core::runtime::resolve_default_data_dir()?.join("pasted-images");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.png", uuid::Uuid::new_v4()));
    let path_text = path.to_string_lossy().into_owned();
    let bytes = if cfg!(windows) {
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; $i=[Windows.Forms.Clipboard]::GetImage(); if ($i) {{ $i.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png) }}",
            path_text.replace('\'', "''")
        );
        run("powershell", &["-NoProfile", "-STA", "-Command", &script]);
        std::fs::read(&path).ok()
    } else if cfg!(target_os = "macos") {
        run("pngpaste", &["-"]).or_else(|| {
            let script = format!(
                "set f to open for access POSIX file \"{path_text}\" with write permission\nwrite (the clipboard as «class PNGf») to f\nclose access f"
            );
            run("osascript", &["-e", &script]);
            std::fs::read(&path).ok()
        })
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        run("wl-paste", &["--type", "image/png"])
    } else {
        run("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"])
    };
    match bytes.filter(|bytes| bytes.starts_with(b"\x89PNG")) {
        Some(bytes) => {
            std::fs::write(&path, bytes)?;
            Ok(Some(path))
        }
        None => {
            let _ = std::fs::remove_file(&path);
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_paths_are_found_in_plain_quoted_and_mentioned_forms() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), b"x").unwrap();
        std::fs::write(dir.path().join("b.JPG"), b"x").unwrap();
        let absolute = dir.path().join("a.png");
        let prompt = format!("compare @a.png with '{}' and \"b.JPG\", not c.png or notes.txt", absolute.display());
        assert_eq!(image_paths(dir.path(), &prompt), [dir.path().join("a.png"), dir.path().join("b.JPG")]);
    }

    #[test]
    fn pasted_images_are_named_by_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("my shot.png");
        std::fs::write(&image, b"x").unwrap();
        let escaped = image.display().to_string().replace(' ', "\\ ");
        assert_eq!(pasted_path(&escaped).as_deref(), Some(image.as_path()));
        assert_eq!(pasted_path(&format!("'{}'", image.display())).as_deref(), Some(image.as_path()));
        assert_eq!(pasted_path("just some text"), None);
        let placeholder = attach(image.clone());
        assert_eq!(attach(image.clone()), placeholder, "the same image keeps its number");
        assert_eq!(image_paths(dir.path(), &format!("what is in {placeholder}?")), [image]);
        assert!(content(dir.path(), "no images here").unwrap().is_none());
    }
}
