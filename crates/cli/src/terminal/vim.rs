//! Vim-style editing for the prompt (`/vim`), as in the reference CLI: Esc
//! enters normal mode, where motions and edits act on the prompt text.

/// What the editor should do after a normal-mode key.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Stay,
    Insert,
    Submit,
    HistoryPrevious,
    HistoryNext,
}

/// Normal-mode state between keys, e.g. the first `d` of `dd`.
#[derive(Default)]
pub struct Normal {
    pending: Option<char>,
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn next_word(chars: &[char], cursor: usize) -> usize {
    let mut index = cursor;
    let start_word = chars.get(index).is_some_and(|ch| is_word(*ch));
    while index < chars.len() && !chars[index].is_whitespace() && is_word(chars[index]) == start_word {
        index += 1;
    }
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    index.min(chars.len().saturating_sub(1))
}

fn previous_word(chars: &[char], cursor: usize) -> usize {
    let mut index = cursor;
    while index > 0 && chars[index - 1].is_whitespace() {
        index -= 1;
    }
    if index == 0 {
        return 0;
    }
    let word = is_word(chars[index - 1]);
    while index > 0 && !chars[index - 1].is_whitespace() && is_word(chars[index - 1]) == word {
        index -= 1;
    }
    index
}

fn word_end(chars: &[char], cursor: usize) -> usize {
    let mut index = cursor + 1;
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    if index >= chars.len() {
        return chars.len().saturating_sub(1);
    }
    let word = is_word(chars[index]);
    while index + 1 < chars.len() && !chars[index + 1].is_whitespace() && is_word(chars[index + 1]) == word {
        index += 1;
    }
    index
}

impl Normal {
    /// Applies one normal-mode key to the prompt text and cursor.
    pub fn key(&mut self, key: char, chars: &mut Vec<char>, cursor: &mut usize) -> Outcome {
        let last = chars.len().saturating_sub(1);
        if let Some(operator) = self.pending.take() {
            match (operator, key) {
                ('d', 'd') => {
                    chars.clear();
                    *cursor = 0;
                }
                ('c', 'c') => {
                    chars.clear();
                    *cursor = 0;
                    return Outcome::Insert;
                }
                ('d', 'w') | ('c', 'w') => {
                    let end = if *cursor < chars.len() { next_word(chars, *cursor) } else { *cursor };
                    let end = if end == last && chars.get(end).is_some_and(|ch| !ch.is_whitespace()) { chars.len() } else { end };
                    chars.drain(*cursor..end.max(*cursor));
                    if operator == 'c' {
                        return Outcome::Insert;
                    }
                }
                _ => {}
            }
            *cursor = (*cursor).min(chars.len().saturating_sub(1));
            return Outcome::Stay;
        }
        match key {
            'h' => *cursor = cursor.saturating_sub(1),
            'l' => *cursor = (*cursor + 1).min(last),
            '0' | '^' => *cursor = 0,
            '$' => *cursor = last,
            'w' => *cursor = next_word(chars, *cursor),
            'b' => *cursor = previous_word(chars, *cursor),
            'e' => *cursor = word_end(chars, *cursor),
            'x' if *cursor < chars.len() => {
                chars.remove(*cursor);
                *cursor = (*cursor).min(chars.len().saturating_sub(1));
            }
            'X' if *cursor > 0 => {
                chars.remove(*cursor - 1);
                *cursor -= 1;
            }
            'D' => {
                chars.truncate(*cursor);
                *cursor = cursor.saturating_sub(1).min(chars.len().saturating_sub(1));
            }
            'C' => {
                chars.truncate(*cursor);
                return Outcome::Insert;
            }
            'd' | 'c' => self.pending = Some(key),
            'i' => return Outcome::Insert,
            'a' => {
                *cursor = (*cursor + 1).min(chars.len());
                return Outcome::Insert;
            }
            'I' => {
                *cursor = 0;
                return Outcome::Insert;
            }
            'A' => {
                *cursor = chars.len();
                return Outcome::Insert;
            }
            'k' => return Outcome::HistoryPrevious,
            'j' => return Outcome::HistoryNext,
            _ => {}
        }
        Outcome::Stay
    }
}

/// Leaving insert mode puts the cursor on the last typed character.
pub fn leave_insert(cursor: &mut usize) {
    *cursor = cursor.saturating_sub(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, cursor: usize, keys: &str) -> (String, usize, Outcome) {
        let mut chars: Vec<char> = text.chars().collect();
        let mut cursor = cursor;
        let mut normal = Normal::default();
        let mut outcome = Outcome::Stay;
        for key in keys.chars() {
            outcome = normal.key(key, &mut chars, &mut cursor);
        }
        (chars.into_iter().collect(), cursor, outcome)
    }

    #[test]
    fn motions_and_edits_follow_vim() {
        assert_eq!(run("fix the bug", 0, "w").1, 4);
        assert_eq!(run("fix the bug", 10, "b").1, 8);
        assert_eq!(run("fix the bug", 0, "e").1, 2);
        assert_eq!(run("fix the bug", 0, "$").1, 10);
        assert_eq!(run("fix the bug", 0, "dw").0, "the bug");
        assert_eq!(run("fix the bug", 4, "D").0, "fix ");
        assert_eq!(run("fix the bug", 0, "x").0, "ix the bug");
        assert_eq!(run("fix the bug", 3, "dd"), (String::new(), 0, Outcome::Stay));
        assert_eq!(run("fix the bug", 4, "cw"), ("fix bug".into(), 4, Outcome::Insert));
        assert_eq!(run("abc", 1, "A"), ("abc".into(), 3, Outcome::Insert));
        assert_eq!(run("abc", 1, "k").2, Outcome::HistoryPrevious);
    }
}

/// Whether `/vim` is on: `"editorMode": "vim"` in `<data dir>/settings.json`.
pub fn enabled(data_dir: &std::path::Path) -> bool {
    std::fs::read_to_string(data_dir.join("settings.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|settings| settings["editorMode"] == "vim")
}

/// Saves the editor mode, keeping every other setting.
pub fn set_enabled(data_dir: &std::path::Path, on: bool) -> anyhow::Result<()> {
    let path = data_dir.join("settings.json");
    let mut settings: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(settings.is_object(), "settings.json must contain a JSON object");
    settings["editorMode"] = serde_json::json!(if on { "vim" } else { "normal" });
    std::fs::write(path, serde_json::to_string_pretty(&settings)?)?;
    Ok(())
}
