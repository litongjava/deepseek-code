//! Images in a prompt, as in the reference CLI: image files dragged into the
//! terminal or mentioned by path, and images pasted from the clipboard with
//! Ctrl+V (saved under the data directory). Pasted images, and pasted or
//! dragged image paths, show in the prompt as `[Image #N]`.
use anyhow::Result;
use deepseek_code_core::runtime::{image_block_from_file, image_media_type};
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

/// Ctrl+V pastes a clipboard image; Alt+V too, for terminals that keep
/// Ctrl+V for their own text paste.
pub fn is_image_paste_key(key: crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    matches!(key.code, KeyCode::Char('v' | 'V'))
        && (key.modifiers == KeyModifiers::CONTROL || key.modifiers == KeyModifiers::ALT)
}

/// What Ctrl+V takes from the clipboard.
pub enum Pasted {
    /// The `[Image #N]` placeholder of a clipboard image.
    Image(String),
    /// Clipboard text, when the terminal passed Ctrl+V on instead of pasting.
    Text(String),
}

/// The clipboard's image, or else its text.
pub fn paste() -> Option<Pasted> {
    if let Ok(Some(path)) = paste_clipboard() {
        return Some(Pasted::Image(format!("{} ", attach(path))));
    }
    #[cfg(windows)]
    let text = native_clipboard::text();
    #[cfg(not(windows))]
    let text: Option<String> = None;
    text.filter(|text| !text.is_empty()).map(|text| Pasted::Text(text.replace("\r\n", "\n")))
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

/// Shares this session's pasted images with the process that delivers a
/// queued message, so its `[Image #N]` attaches the image.
pub fn share(session: &str, text: &str) {
    if placeholder_numbers(text).is_empty() {
        return;
    }
    let pasted = PASTED.lock().unwrap_or_else(|error| error.into_inner()).clone();
    if let Ok(data_dir) = deepseek_code_core::runtime::resolve_default_data_dir() {
        let _ = deepseek_code_core::services::input_queue::save_pasted_images(&data_dir, session, &pasted);
    }
}

/// The numbers of the pasted images a prompt names with `[Image #N]`.
pub fn placeholder_numbers(prompt: &str) -> Vec<usize> {
    let pasted = PASTED.lock().unwrap_or_else(|error| error.into_inner());
    let mut numbers: Vec<usize> = Vec::new();
    for number in prompt.split("[Image #").skip(1).filter_map(|rest| rest.split_once(']')?.0.parse::<usize>().ok()) {
        if (1..=pasted.len()).contains(&number) && !numbers.contains(&number) {
            numbers.push(number);
        }
    }
    numbers
}

/// The pasted images a prompt names with `[Image #N]`, in order.
fn placeholders(prompt: &str) -> Vec<PathBuf> {
    let numbers = placeholder_numbers(prompt);
    let pasted = PASTED.lock().unwrap_or_else(|error| error.into_inner());
    numbers.into_iter().map(|number| pasted[number - 1].clone()).collect()
}

/// Groups quoted path tokens without interpreting shell escapes or expansions.
fn image_path_words(prompt: &str) -> impl Iterator<Item = &str> {
    let mut remaining = prompt;
    std::iter::from_fn(move || {
        remaining = remaining.trim_start();
        if remaining.is_empty() {
            return None;
        }
        let mut end = remaining.find(char::is_whitespace).unwrap_or(remaining.len());
        let start = remaining.trim_start_matches(|ch| matches!(ch, '@' | '('));
        if let Some(quote @ ('"' | '\'')) = start.chars().next() {
            let prefix = remaining.len() - start.len() + quote.len_utf8();
            if let Some(close) = remaining[prefix..].find(quote) {
                let quoted_end = prefix + close + quote.len_utf8();
                let suffix = &remaining[quoted_end..];
                let suffix_end = suffix.find(char::is_whitespace).unwrap_or(suffix.len());
                if suffix[..suffix_end].chars().all(|ch| matches!(ch, ',' | ';' | ')' | '?' | '!')) {
                    end = quoted_end + suffix_end;
                }
            }
        }
        let word = &remaining[..end];
        remaining = &remaining[end..];
        Some(word)
    })
}

/// Image paths written in the prompt: plain, quoted or after @.
fn image_paths(workspace: &Path, prompt: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for path in placeholders(prompt) {
        if path.is_file() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    for word in image_path_words(prompt) {
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

#[cfg(not(windows))]
fn run(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    (output.status.success() && !output.stdout.is_empty()).then_some(output.stdout)
}

/// Saves a clipboard image as PNG under `<data dir>/pasted-images`, or returns
/// the image file copied in a file manager.
pub fn paste_clipboard() -> Result<Option<PathBuf>> {
    let dir = deepseek_code_core::runtime::resolve_default_data_dir()?.join("pasted-images");
    let path = dir.join(format!("{}.png", uuid::Uuid::new_v4()));
    #[cfg(windows)]
    let bytes = match native_clipboard::image() {
        Some(native_clipboard::Image::File(file)) => return Ok(Some(file)),
        Some(native_clipboard::Image::Png(bytes)) => Some(bytes),
        None => None,
    };
    #[cfg(not(windows))]
    let bytes = {
        std::fs::create_dir_all(&dir)?;
        let path_text = path.to_string_lossy().into_owned();
        if cfg!(target_os = "macos") {
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
        }
    };
    match bytes.filter(|bytes| bytes.starts_with(b"\x89PNG")) {
        Some(bytes) => {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&path, bytes)?;
            Ok(Some(path))
        }
        None => {
            let _ = std::fs::remove_file(&path);
            Ok(None)
        }
    }
}

/// The Windows clipboard read in-process: the `PNG` format browsers and
/// screenshot tools publish, device-independent bitmaps, and image files
/// copied in Explorer.
#[cfg(windows)]
pub(crate) mod native_clipboard {
    use std::path::PathBuf;

    pub enum Image {
        Png(Vec<u8>),
        File(PathBuf),
    }

    pub fn image() -> Option<Image> {
        let mut clipboard = arboard::Clipboard::new().ok()?;
        if let Ok(image) = clipboard.get_image() {
            return encode_png(image.width, image.height, &image.bytes).map(Image::Png);
        }
        let files = clipboard.get().file_list().ok()?;
        files
            .into_iter()
            .find(|file| deepseek_code_core::runtime::image_media_type(file).is_some() && file.is_file())
            .map(Image::File)
    }

    pub fn text() -> Option<String> {
        arboard::Clipboard::new().ok()?.get_text().ok()
    }

    pub fn set_text(text: &str) -> bool {
        arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)).is_ok()
    }

    /// RGBA pixels as a PNG file.
    pub fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
        if width == 0 || height == 0 || rgba.len() != width.checked_mul(height)?.checked_mul(4)? {
            return None;
        }
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, u32::try_from(width).ok()?, u32::try_from(height).ok()?);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
        writer.finish().ok()?;
        Some(bytes)
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
    fn image_paths_find_single_quoted_chinese_path_with_spaces_and_text() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("中文 图片项目");
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("红色 red.png");
        std::fs::write(&path, b"x").unwrap();
        for quote in ['"', '\''] {
            let prompt = format!("INTEROP_SINGLE 请保留这张图片。 {quote}{}{quote} 然后描述图片", path.display());
            assert_eq!(image_paths(dir.path(), &prompt), [path.clone()]);
            let blocks = content(dir.path(), &prompt).unwrap().unwrap();
            let blocks = blocks.as_array().unwrap();
            assert_eq!(blocks.len(), 2);
            assert_eq!(blocks[0], json!({"type": "text", "text": prompt}));
            assert_eq!(blocks[1], image_block_from_file(&path).unwrap());
        }
    }

    #[test]
    fn image_paths_find_multiple_quoted_mentions_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let names = ["红色 red.png", "绿色 green.JPG", "蓝色 blue.webp"];
        let paths: Vec<_> = names.iter().map(|name| dir.path().join(name)).collect();
        for path in &paths {
            std::fs::write(path, b"x").unwrap();
        }
        let prompt = format!("比较 @\"{}\", 与 (@'{}'); 然后 \"{}\"! 再看 @\"{}\"?", names[0], names[1], paths[2].display(), names[0]);
        assert_eq!(image_paths(dir.path(), &prompt), paths);
    }

    #[test]
    fn image_path_words_preserve_literal_windows_and_unix_paths() {
        let prompt = r#"看 @"C:\中文 图片\红色 red.png" 和 '/tmp/中文 图片/green $HOME $(echo x) `id`.png'"#;
        assert_eq!(
            image_path_words(prompt).collect::<Vec<_>>(),
            ["看", r#"@"C:\中文 图片\红色 red.png""#, "和", "'/tmp/中文 图片/green $HOME $(echo x) `id`.png'"]
        );
        assert_eq!(image_path_words("don't change ordinary text").collect::<Vec<_>>(), ["don't", "change", "ordinary", "text"]);
    }

    #[test]
    fn image_paths_ignore_urls_and_arbitrary_quoted_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), b"x").unwrap();
        for prompt in [
            "ordinary text without images",
            "https://example.com/a.png http://example.com/a.png",
            "\"https://example.com/a.png\"",
            "say \"please describe a.png\" or 'do not attach a.png'",
            "\"missing image.png\" and @\"notes with spaces.txt\"",
            "prefix\"a.png\"",
        ] {
            assert!(image_paths(dir.path(), prompt).is_empty(), "{prompt}");
            assert!(content(dir.path(), prompt).unwrap().is_none(), "{prompt}");
        }
    }

    #[test]
    fn image_paths_unmatched_quotes_fall_back_to_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        std::fs::write(&path, b"x").unwrap();
        for prompt in ["看 \"missing image.png 然后 @a.png", "看 @'missing image.png 然后 a.png", "\"a.png", "'a.png\""] {
            assert_eq!(image_path_words(prompt).collect::<Vec<_>>(), prompt.split_whitespace().collect::<Vec<_>>());
            assert_eq!(image_paths(dir.path(), prompt), [path.clone()]);
        }
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
