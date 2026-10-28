//! The terminal window title, as the reference CLI sets it: the product name
//! at start, then a few words on the session's topic, cleared on exit.
use std::io::{IsTerminal, Write};
use std::sync::Mutex;

/// The topic named for a session, waiting to be shown: (session, title).
static TOPIC: Mutex<Option<(String, String)>> = Mutex::new(None);
/// Sessions whose topic has been asked for, so each is named once.
static NAMED: Mutex<Vec<String>> = Mutex::new(Vec::new());

const PROMPT: &str = "Name the topic of the user's request in 2 to 5 words, in the request's language. Reply with only the title: no quotes, no punctuation at the end.";

pub fn set(text: &str) {
    let mut out = std::io::stdout();
    if out.is_terminal() && !crate::terminal::screen_reader() {
        let clean: String = text.chars().filter(|ch| !ch.is_control()).collect();
        let _ = write!(out, "\x1b]0;{clean}\x07");
        let _ = out.flush();
    }
}

pub fn start() {
    set("\u{2733} DeepSeek Code");
}

pub fn clear() {
    set("");
}

/// Shows the session's topic once it has been named.
pub fn apply(session: &str) {
    let topic = TOPIC.lock().unwrap_or_else(|error| error.into_inner()).take();
    if let Some((owner, title)) = topic {
        if owner == session {
            set(&format!("\u{2733} {title}"));
        }
    }
}

/// A title from the model's reply: one short line.
fn tidy(reply: &str) -> Option<String> {
    let line = reply.lines().map(str::trim).find(|line| !line.is_empty())?;
    let decoration = |ch: char| matches!(ch, '"' | '\'' | '`' | '*' | '#') || ch.is_whitespace();
    let line = line.trim_start_matches(decoration).trim_end_matches(|ch: char| decoration(ch) || matches!(ch, '.' | '。' | '!' | '！'));
    (!line.is_empty()).then(|| line.chars().take(50).collect())
}

/// Names the session's topic from its first prompt, in the background.
pub fn name_topic(session: &crate::session::Session, prompt: &str) {
    if !std::io::stdout().is_terminal() || prompt.trim().is_empty() || prompt.trim_start().starts_with('/') {
        return;
    }
    {
        let mut named = NAMED.lock().unwrap_or_else(|error| error.into_inner());
        if named.contains(&session.id) {
            return;
        }
        named.push(session.id.clone());
    }
    let engine = session.side_engine();
    let (id, provider, model) = (session.id.clone(), session.provider_id.clone(), session.model.clone());
    let prompt: String = prompt.chars().take(2000).collect();
    tokio::spawn(async move {
        if let Ok(Some(title)) = engine.generate_text(&provider, &model, PROMPT, &prompt).await.map(|reply| tidy(&reply)) {
            *TOPIC.lock().unwrap_or_else(|error| error.into_inner()) = Some((id, title));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_one_short_line_without_decoration() {
        assert_eq!(tidy("\n\"Attached image color.\"\nmore").as_deref(), Some("Attached image color"));
        assert_eq!(tidy("**修复登录问题**。").as_deref(), Some("修复登录问题"));
        assert_eq!(tidy("   ").as_deref(), None);
        assert_eq!(tidy(&"x".repeat(80)).map(|title| title.len()), Some(50));
    }
}
