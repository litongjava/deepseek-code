//! User overrides for the interactive prompt, using Claude's keybindings layout.
use anyhow::{bail, ensure, Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Value};
use std::{
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Default)]
pub struct Keybindings {
    bindings: Vec<Binding>,
}

#[derive(Clone)]
struct Binding {
    chat: bool,
    keys: Vec<Key>,
    action: Option<KeyEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    code: KeyCode,
    modifiers: KeyModifiers,
}

#[derive(Default)]
pub struct Pending {
    keys: Vec<Key>,
    since: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    Key(KeyEvent),
    Consumed,
}

pub fn supported_actions() -> &'static [&'static str] {
    &[
        "chat:submit",
        "chat:cycleMode",
        "chat:newline",
        "chat:cancel",
        "app:interrupt",
        "app:exit",
        "app:toggleTranscript",
        "app:redraw",
        "history:previous",
        "history:next",
        "history:search",
        "input:lineStart",
        "input:lineEnd",
        "input:backwardChar",
        "input:forwardChar",
        "input:deleteBackward",
        "input:deleteForward",
        "input:killToStart",
        "input:killToEnd",
    ]
}

fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

pub fn is_cycle_permission(key: KeyEvent) -> bool {
    (key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT)))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn action(name: &str) -> Result<KeyEvent> {
    use KeyCode::*;
    let none = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;
    Ok(match name {
        "chat:cycleMode" => event(Tab, KeyModifiers::SHIFT),
        "chat:submit" => event(Enter, none),
        "chat:newline" => event(Enter, KeyModifiers::ALT),
        "chat:cancel" | "app:interrupt" => event(Char('c'), ctrl),
        "app:exit" => event(Char('d'), ctrl),
        "app:toggleTranscript" => event(Char('o'), ctrl),
        "app:redraw" => event(Char('l'), ctrl),
        "history:previous" => event(Up, none),
        "history:next" => event(Down, none),
        "history:search" => event(Char('r'), ctrl),
        "input:lineStart" => event(Home, none),
        "input:lineEnd" => event(End, none),
        "input:backwardChar" => event(Left, none),
        "input:forwardChar" => event(Right, none),
        "input:deleteBackward" => event(Backspace, none),
        "input:deleteForward" => event(Delete, none),
        "input:killToStart" => event(Char('u'), ctrl),
        "input:killToEnd" => event(Char('k'), ctrl),
        _ => bail!(
            "Unsupported keybinding action '{name}'. Supported actions: {}",
            supported_actions().join(", ")
        ),
    })
}

fn parse_key(value: &str) -> Result<Key> {
    let lower = value.to_ascii_lowercase();
    let parts: Vec<_> = lower.split('+').collect();
    let mut modifiers = KeyModifiers::NONE;
    for modifier in &parts[..parts.len().saturating_sub(1)] {
        modifiers |= match *modifier {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            "cmd" | "super" | "win" => KeyModifiers::SUPER,
            _ => bail!("Unknown key modifier '{modifier}' in '{value}'"),
        };
    }
    let name = parts.last().copied().unwrap_or("");
    let code = match name {
        "enter" | "return" => KeyCode::Enter,
        "escape" | "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        _ if name.starts_with('f')
            && name[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) =>
        {
            KeyCode::F(name[1..].parse().unwrap())
        }
        _ if name.chars().count() == 1 => KeyCode::Char(name.chars().next().unwrap()),
        _ => bail!("Unknown key '{value}'"),
    };
    Ok(Key { code, modifiers })
}

fn normalized(key: KeyEvent) -> Key {
    let code = match key.code {
        KeyCode::Char(ch) => KeyCode::Char(ch.to_ascii_lowercase()),
        KeyCode::BackTab => KeyCode::Tab,
        other => other,
    };
    let mut modifiers = key.modifiers;
    if key.code == KeyCode::BackTab {
        modifiers |= KeyModifiers::SHIFT;
    }
    Key { code, modifiers }
}

impl Keybindings {
    pub fn parse(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .context("Invalid keybindings JSON")?;
        let blocks = value["bindings"]
            .as_array()
            .context("keybindings.json must contain a bindings array")?;
        ensure!(blocks.len() <= 32, "Too many keybinding context blocks");
        let mut result = Self::default();
        for block in blocks {
            let chat = match block["context"].as_str() {
                Some("Chat") => true,
                Some("Global") => false,
                Some(context) => {
                    bail!("Unsupported keybinding context '{context}'; supported: Global, Chat")
                }
                None => bail!("Keybinding block needs a context"),
            };
            for (shortcut, value) in block["bindings"]
                .as_object()
                .context("Context bindings must be an object")?
            {
                let keys: Vec<_> = shortcut
                    .split_whitespace()
                    .map(parse_key)
                    .collect::<Result<_>>()?;
                ensure!(
                    (1..=2).contains(&keys.len()),
                    "Shortcut '{shortcut}' must contain one or two keystrokes"
                );
                ensure!(
                    !keys.iter().any(|key| key.modifiers == KeyModifiers::CONTROL
                        && matches!(key.code, KeyCode::Char('c' | 'd'))),
                    "Ctrl+C and Ctrl+D are reserved and cannot be rebound"
                );
                let mapped = if value.is_null() {
                    None
                } else {
                    Some(action(
                        value
                            .as_str()
                            .context("Keybinding action must be a string or null")?,
                    )?)
                };
                if let Some(previous) = result
                    .bindings
                    .iter_mut()
                    .find(|binding| binding.chat == chat && binding.keys == keys)
                {
                    previous.action = mapped;
                } else {
                    result.bindings.push(Binding {
                        chat,
                        keys,
                        action: mapped,
                    });
                }
                ensure!(result.bindings.len() <= 256, "Too many custom keybindings");
            }
        }
        Ok(result)
    }

    pub fn resolve(&self, key: KeyEvent, pending: &mut Pending) -> Resolution {
        let incoming = normalized(key);
        // Reserved interrupt/exit keys always work, including halfway through a chord.
        if incoming.modifiers == KeyModifiers::CONTROL
            && matches!(incoming.code, KeyCode::Char('c' | 'd'))
        {
            *pending = Pending::default();
            return Resolution::Key(key);
        }
        if pending
            .since
            .is_some_and(|since| since.elapsed() > Duration::from_secs(1))
        {
            *pending = Pending::default();
        }
        pending.keys.push(incoming);
        // Focused Chat overrides Global, and later bindings override earlier ones.
        for chat in [true, false] {
            if let Some(binding) = self
                .bindings
                .iter()
                .rev()
                .find(|binding| binding.chat == chat && binding.keys == pending.keys)
            {
                *pending = Pending::default();
                return binding
                    .action
                    .map(Resolution::Key)
                    .unwrap_or(Resolution::Consumed);
            }
            if self
                .bindings
                .iter()
                .any(|binding| binding.chat == chat && binding.keys.starts_with(&pending.keys))
            {
                pending.since = Some(Instant::now());
                return Resolution::Consumed;
            }
        }
        let was_chord = pending.keys.len() > 1;
        *pending = Pending::default();
        if was_chord {
            self.resolve(key, pending)
        } else {
            Resolution::Key(key)
        }
    }
}

pub fn load(path: &Path) -> Result<Keybindings> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            ensure!(text.len() <= 256 * 1024, "keybindings.json exceeds 256 KiB");
            Keybindings::parse(&text).with_context(|| format!("Cannot load {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Keybindings::default()),
        Err(error) => Err(error).with_context(|| format!("Cannot read {}", path.display())),
    }
}

pub fn ensure_file(path: &Path) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let template = json!({"$schema":"https://www.schemastore.org/claude-code-keybindings.json","bindings":[{"context":"Global","bindings":{"ctrl+o":"app:toggleTranscript","ctrl+r":"history:search"}},{"context":"Chat","bindings":{"enter":"chat:submit","shift+tab":"chat:cycleMode","alt+enter":"chat:newline","up":"history:previous","down":"history:next"}}]});
    writeln!(file, "{}", serde_json::to_string_pretty(&template)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_cycle_normalizes_backtab_and_supports_remap_and_disable() {
        assert!(is_cycle_permission(event(KeyCode::BackTab, KeyModifiers::NONE)));
        assert!(is_cycle_permission(event(KeyCode::Tab, KeyModifiers::SHIFT)));
        assert!(!is_cycle_permission(event(KeyCode::Tab, KeyModifiers::NONE)));
        let bindings = Keybindings::parse(r#"{"bindings":[{"context":"Chat","bindings":{"shift+tab":null,"f2":"chat:cycleMode"}}]}"#).unwrap();
        for key in [event(KeyCode::BackTab, KeyModifiers::NONE), event(KeyCode::Tab, KeyModifiers::SHIFT)] {
            assert_eq!(bindings.resolve(key, &mut Pending::default()), Resolution::Consumed);
        }
        let Resolution::Key(key) = bindings.resolve(event(KeyCode::F(2), KeyModifiers::NONE), &mut Pending::default()) else { panic!("cycle shortcut not mapped") };
        assert!(is_cycle_permission(key));
    }

    #[test]
    fn context_override_null_and_chords_work() {
        let bindings=Keybindings::parse(r#"{"bindings":[{"context":"Global","bindings":{"ctrl+j":"chat:newline","ctrl+o":null}},{"context":"Chat","bindings":{"ctrl+j":"chat:submit","ctrl+x ctrl+k":"input:lineEnd"}}]}"#).unwrap();
        let mut pending = Pending::default();
        assert_eq!(
            bindings.resolve(
                event(KeyCode::Char('j'), KeyModifiers::CONTROL),
                &mut pending
            ),
            Resolution::Key(event(KeyCode::Enter, KeyModifiers::NONE))
        );
        assert_eq!(
            bindings.resolve(
                event(KeyCode::Char('o'), KeyModifiers::CONTROL),
                &mut pending
            ),
            Resolution::Consumed
        );
        assert_eq!(
            bindings.resolve(
                event(KeyCode::Char('x'), KeyModifiers::CONTROL),
                &mut pending
            ),
            Resolution::Consumed
        );
        assert_eq!(
            bindings.resolve(
                event(KeyCode::Char('k'), KeyModifiers::CONTROL),
                &mut pending
            ),
            Resolution::Key(event(KeyCode::End, KeyModifiers::NONE))
        );
    }
    #[test]
    fn invalid_config_and_reserved_shortcuts_report_errors() {
        for text in [
            "broken",
            r#"{"bindings":[{"context":"Other","bindings":{}}]}"#,
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+j":"unknown"}}]}"#,
            r#"{"bindings":[{"context":"Global","bindings":{"ctrl+c":null}}]}"#,
        ] {
            assert!(Keybindings::parse(text).is_err());
        }
    }
    #[test]
    fn unmatched_chord_reprocesses_key_and_interrupt_is_preserved() {
        let bindings=Keybindings::parse(r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+x ctrl+k":"input:lineEnd","f2":"chat:submit"}}]}"#).unwrap();
        let mut pending = Pending::default();
        bindings.resolve(
            event(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut pending,
        );
        assert_eq!(
            bindings.resolve(event(KeyCode::F(2), KeyModifiers::NONE), &mut pending),
            Resolution::Key(event(KeyCode::Enter, KeyModifiers::NONE))
        );
        bindings.resolve(
            event(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut pending,
        );
        assert_eq!(
            bindings.resolve(
                event(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &mut pending
            ),
            Resolution::Key(event(KeyCode::Char('c'), KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn template_roundtrips_and_does_not_overwrite_customizations() {
        let dir = tempfile::tempdir().unwrap();
        let path = deepseek_code_core::data_layout::DataLayout::from_root(&dir.path())
            .config_file("keybindings.json");
        ensure_file(&path).unwrap();
        load(&path).unwrap();
        let custom = r#"{"bindings":[{"context":"Chat","bindings":{"f2":"chat:submit"}}]}"#;
        std::fs::write(&path, custom).unwrap();
        ensure_file(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), custom);
        let bindings = load(&path).unwrap();
        assert_eq!(
            bindings.resolve(
                event(KeyCode::F(2), KeyModifiers::NONE),
                &mut Pending::default()
            ),
            Resolution::Key(event(KeyCode::Enter, KeyModifiers::NONE))
        );
        std::fs::write(&path, "invalid").unwrap();
        assert!(format!("{:#}", load(&path).err().unwrap()).contains("Invalid keybindings JSON"));
    }

    #[test]
    fn expired_chord_does_not_capture_later_text() {
        let bindings = Keybindings::parse(
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+x k":"input:lineEnd"}}]}"#,
        )
        .unwrap();
        let mut pending = Pending::default();
        bindings.resolve(
            event(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut pending,
        );
        pending.since = Some(Instant::now() - Duration::from_secs(2));
        let key = event(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(bindings.resolve(key, &mut pending), Resolution::Key(key));
    }
}
