//! AskUserQuestion as a menu, as in the reference CLI: arrow keys move
//! between options, the focused option's preview is shown above the
//! question, and the last entry takes a typed answer instead.
use super::change_view::{self, Line, Style};
use serde_json::Value;

/// The free-text entry after the options.
pub const OTHER: &str = "Type something else";

pub struct Menu {
    pub question: String,
    /// Labels with descriptions, then [`OTHER`].
    pub options: Vec<String>,
    /// Preview lines per entry; empty where an option has none.
    pub previews: Vec<Vec<Line>>,
    /// multiSelect: options are checked with space and sent together.
    pub multi: bool,
    labels: Vec<String>,
}

/// A menu for a question with options; multiSelect questions get checkboxes.
pub fn menu(question: &str, input: &Value) -> Option<Menu> {
    let options = input["options"].as_array().filter(|options| !options.is_empty())?;
    let labels: Vec<String> = options.iter().map(|option| option["label"].as_str().or_else(|| option.as_str()).unwrap_or_default().to_owned()).collect();
    let mut entries: Vec<String> = options.iter().zip(&labels).map(|(option, label)| match option["description"].as_str().filter(|text| !text.trim().is_empty()) {
        Some(description) => format!("{label} \u{2014} {description}"),
        None => label.clone(),
    }).collect();
    entries.push(OTHER.into());
    let mut previews: Vec<Vec<Line>> = options.iter().map(|option| match option["preview"].as_str().filter(|text| !text.trim().is_empty()) {
        Some(preview) => preview.lines().map(|line| change_view::styled(&format!("  \u{2502} {}", expand_tabs(line)), Style::Body)).collect(),
        None => Vec::new(),
    }).collect();
    previews.push(Vec::new());
    // The call's own question; `question` may be a frontend's full text prompt.
    let question = input["question"].as_str().unwrap_or(question);
    let question = match input["header"].as_str().filter(|header| !header.trim().is_empty()) {
        Some(header) => format!("[{header}] {question}"),
        None => question.to_owned(),
    };
    Some(Menu { question, options: entries, previews, multi: input["multiSelect"] == true, labels })
}

/// Tabs as a terminal shows them, to the next multiple of eight columns, so
/// a tab-indented preview keeps its shape.
fn expand_tabs(line: &str) -> String {
    let mut out = String::new();
    for ch in line.chars() {
        if ch == '\t' {
            let width = unicode_width::UnicodeWidthStr::width(out.as_str());
            out.push_str(&" ".repeat(8 - width % 8));
        } else {
            out.push(ch);
        }
    }
    out
}

impl Menu {
    /// The answer for the model from the menu's reply: the chosen label, or
    /// the typed text after the last entry.
    pub fn answer(&self, reply: &str) -> String {
        if reply == "esc" {
            return "The user dismissed the question without answering.".into();
        }
        let (number, typed) = reply.split_once('\n').unwrap_or((reply, ""));
        if let Some(numbers) = number.strip_prefix("m:") {
            // The checked labels, then any typed answer.
            let mut picks: Vec<String> = numbers.split(',').filter_map(|n| n.trim().parse::<usize>().ok()).filter_map(|n| n.checked_sub(1)).filter_map(|i| self.labels.get(i).cloned()).collect();
            if !typed.trim().is_empty() { picks.push(typed.trim().to_owned()); }
            return if picks.is_empty() { "The user submitted no choices.".into() } else { picks.join(", ") };
        }
        match number.trim().parse::<usize>().ok().and_then(|n| n.checked_sub(1)) {
            Some(index) if index < self.labels.len() => self.labels[index].clone(),
            Some(_) if !typed.trim().is_empty() => typed.trim().to_owned(),
            _ => reply.trim().to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_select_questions_become_menus_with_previews() {
        let input = serde_json::json!({"header":"Indent","options":[{"label":"Tabs","description":"One tab per level","preview":"fn a() {\n\tb();\n}"},{"label":"Spaces"}]});
        let menu = menu("Which indentation?", &input).unwrap();
        assert_eq!(menu.question, "[Indent] Which indentation?");
        assert_eq!(menu.options, ["Tabs \u{2014} One tab per level", "Spaces", OTHER]);
        assert_eq!(change_view::plain(&menu.previews[0]).lines().nth(1), Some("  \u{2502}         b();"));
        // A frontend's full prompt is not repeated in the menu's question.
        let with_question = serde_json::json!({"question":"Tabs or spaces?","options":[{"label":"Tabs"}]});
        assert_eq!(super::menu("[H] Tabs or spaces?\n1. Tabs", &with_question).unwrap().question, "Tabs or spaces?");
        assert!(menu.previews[1].is_empty() && menu.previews[2].is_empty());
        assert_eq!(menu.answer("2"), "Spaces");
        assert_eq!(menu.answer("3\nUse whatever the file uses"), "Use whatever the file uses");
        assert!(menu.answer("esc").contains("dismissed"));
        let multi = super::menu("Q?", &serde_json::json!({"multiSelect":true,"options":[{"label":"A"},{"label":"B"},{"label":"C"}]})).unwrap();
        assert!(multi.multi && multi.options.len() == 4);
        assert_eq!(multi.answer("m:1,3"), "A, C");
        assert_eq!(multi.answer("m:2\nalso D"), "B, also D");
        assert!(multi.answer("m:").contains("no choices"));
    }
}
