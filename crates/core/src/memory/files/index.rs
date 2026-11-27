use super::store::Entry;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub struct IndexLimits {
    pub lines: usize,
    pub bytes: usize,
    pub tokens: usize,
}
impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            lines: 200,
            bytes: 25 * 1024,
            tokens: 4096,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IndexContext {
    pub text: String,
    pub included: usize,
    pub omitted: usize,
    pub truncated: bool,
    /// False only when even the omission marker cannot fit (e.g. zero budget).
    pub marker_included: bool,
}

impl IndexContext {
    /// Caller may pass the actual model tokenizer. The default byte counter is
    /// conservative for UTF-8; never assume chars/4 for Chinese text.
    pub fn from_index(
        index: &str,
        path: &str,
        limits: IndexLimits,
        count_tokens: impl Fn(&str) -> usize,
    ) -> Self {
        let rows: Vec<_> = index.lines().collect();
        let fits = |text: &str| {
            text.lines().count() <= limits.lines
                && text.len() <= limits.bytes
                && count_tokens(text) <= limits.tokens
        };
        if fits(index) {
            return Self {
                text: index.into(),
                included: rows.len(),
                omitted: 0,
                truncated: false,
                marker_included: false,
            };
        }
        // No tokenizer monotonicity assumptions: test each complete prefix.
        let mut prefix = String::new();
        let mut best = Self {
            text: String::new(),
            included: 0,
            omitted: rows.len(),
            truncated: true,
            marker_included: false,
        };
        let path = path.replace(['\r', '\n'], " ");
        for included in 0..rows.len() {
            let candidate = format!("{prefix}[Memory index truncated: {} entries omitted; full index: {path}; limits: {} lines / {} bytes / {} tokens.]\n", rows.len() - included, limits.lines, limits.bytes, limits.tokens);
            if fits(&candidate) {
                best = Self {
                    text: candidate,
                    included,
                    omitted: rows.len() - included,
                    truncated: true,
                    marker_included: true,
                };
            }
            prefix.push_str(rows[included]);
            prefix.push('\n');
            if prefix.len() > limits.bytes || included + 1 > limits.lines {
                break;
            }
        }
        best
    }
}

fn escape(text: &str) -> String {
    let mut output = String::new();
    for c in text.chars() {
        if "\\[]*_`<>!".contains(c) {
            output.push('\\');
        }
        output.push(c);
    }
    output
}

fn pointer(entry: &Entry) -> String {
    format!(
        "- [{}]({}) — {}",
        escape(&entry.document.name),
        entry.filename,
        escape(&entry.document.description)
    )
}

/// Only canonical catalog rows are managed by explicit repair. All other text,
/// including arbitrary Markdown links and original line endings, stays authored.
pub(super) fn repair(entries: &[Entry], previous: &str) -> String {
    let mut remaining: BTreeMap<_, _> = entries.iter().map(|e| (e.filename.as_str(), e)).collect();
    let mut result = String::new();
    for line in previous.split_inclusive('\n') {
        let row = line.trim_end_matches(['\r', '\n']);
        let file = row
            .strip_prefix("- [")
            .and_then(|label| {
                let (end, _) = label.match_indices("](").find(|(i, _)| {
                    label[..*i]
                        .bytes()
                        .rev()
                        .take_while(|&b| b == b'\\')
                        .count()
                        % 2
                        == 0
                })?;
                label[end + 2..].split_once(") — ").map(|(file, _)| file)
            })
            .filter(|file| super::memory_filename(file).is_ok());
        match file {
            Some(file) => {
                if let Some(entry) = remaining.remove(file) {
                    result.push_str(&pointer(entry));
                    result.push_str(&line[row.len()..]);
                }
            }
            None => result.push_str(line),
        }
    }
    let mut missing: Vec<_> = remaining.into_values().collect();
    missing.sort_by_key(|e| (super::format::name_key(&e.document.name), &e.filename));
    for entry in missing {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&pointer(entry));
        result.push('\n');
    }
    result
}
