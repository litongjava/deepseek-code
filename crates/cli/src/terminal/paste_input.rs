//! Windows Console consumes paste delimiters unless VT input is enabled. Intercept
//! those raw records before crossterm (which drops VK=0 escape characters).
//! Ordinary input still uses crossterm's keyboard/mouse/resize interpretation.
use crossterm::event::Event;
use std::{io, time::Duration};

#[cfg(not(windows))]
pub struct RawReader;
#[cfg(not(windows))]
impl RawReader {
    pub fn new() -> io::Result<Self> {
        Ok(Self)
    }
    pub fn next(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if crossterm::event::poll(timeout)? {
            crossterm::event::read().map(|event| Some(backspace_from_ctrl_h(event)))
        } else {
            Ok(None)
        }
    }
}

/// Many terminals (Xshell, PuTTY variants, some tmux setups) send `^H` for
/// Backspace and `^?` for Ctrl+Backspace; crossterm reads `^H` as Ctrl+H.
/// As on Windows, both bytes delete backwards.
#[cfg(not(windows))]
fn backspace_from_ctrl_h(event: Event) -> Event {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    match event {
        Event::Key(key) if key.code == KeyCode::Char('h') && key.modifiers == KeyModifiers::CONTROL => {
            Event::Key(KeyEvent::new_with_kind(KeyCode::Backspace, KeyModifiers::NONE, key.kind))
        }
        event => event,
    }
}

#[cfg(any(windows, test))]
#[derive(Default)]
struct PasteDecoder {
    prefix: String,
    body: Option<String>,
    ready: std::collections::VecDeque<Event>,
}
#[cfg(any(windows, test))]
impl PasteDecoder {
    fn push(&mut self, ch: char) {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        const START: &str = "\x1b[200~";
        const END: &str = "\x1b[201~";
        if let Some(body) = &mut self.body {
            body.push(ch);
            if body.ends_with(END) {
                body.truncate(body.len() - END.len());
                let text = body.replace("\r\n", "\n").replace('\r', "\n");
                self.body = None;
                self.ready.push_back(Event::Paste(text));
            }
            return;
        }
        self.prefix.push(ch);
        if self.prefix == START {
            self.prefix.clear();
            self.body = Some(String::new());
        } else if self.prefix.starts_with("\x1b[") || self.prefix.starts_with("\x1bO") {
            if self.prefix.len() > 2 && ('@'..='~').contains(&ch) {
                if let Some(event) = decode_sequence(&self.prefix) {
                    self.ready.push_back(event);
                }
                self.prefix.clear();
            } else if self.prefix.len() > 64 {
                self.prefix.clear();
            }
        } else if !START.starts_with(&self.prefix) {
            if self.prefix.starts_with('\x1b') && self.prefix.chars().count() == 2 {
                self.ready.push_back(Event::Key(KeyEvent::new(
                    match ch {
                        '\r' | '\n' => KeyCode::Enter,
                        '\x7f' | '\x08' => KeyCode::Backspace,
                        '\t' => KeyCode::Tab,
                        ch => KeyCode::Char(ch),
                    },
                    KeyModifiers::ALT,
                )));
                self.prefix.clear();
                return;
            }
            for ch in self.prefix.drain(..) {
                let mut modifiers = KeyModifiers::NONE;
                let code = match ch {
                    '\x1b' => KeyCode::Esc,
                    '\r' | '\n' => KeyCode::Enter,
                    '\t' => KeyCode::Tab,
                    '\x08' | '\x7f' => KeyCode::Backspace,
                    '\0' => {
                        modifiers = KeyModifiers::CONTROL;
                        KeyCode::Char(' ')
                    }
                    '\x01'..='\x1a' => {
                        modifiers = KeyModifiers::CONTROL;
                        KeyCode::Char((b'a' + ch as u8 - 1) as char)
                    }
                    // Ctrl+\, Ctrl+], Ctrl+^ and Ctrl+_ (undo).
                    '\x1c'..='\x1f' => {
                        modifiers = KeyModifiers::CONTROL;
                        KeyCode::Char(['\\', ']', '^', '_'][ch as usize - 0x1c])
                    }
                    ch => KeyCode::Char(ch),
                };
                self.ready
                    .push_back(Event::Key(KeyEvent::new(code, modifiers)));
            }
        }
    }
    fn capturing(&self) -> bool {
        self.body.is_some() || !self.prefix.is_empty()
    }
}

#[cfg(any(windows, test))]
fn decode_sequence(text: &str) -> Option<Event> {
    use crossterm::event::{
        KeyCode as K, KeyEvent, KeyModifiers as M, MouseButton as B, MouseEvent,
        MouseEventKind as MK,
    };
    let final_char = text.chars().last()?;
    let params = &text[2..text.len() - 1];
    if let Some(mouse) = params.strip_prefix('<') {
        if !matches!(final_char, 'M' | 'm') {
            return None;
        }
        let p = mouse
            .split(';')
            .map(str::parse::<u16>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        if p.len() != 3 {
            return None;
        }
        let mut modifiers = M::NONE;
        if p[0] & 4 != 0 {
            modifiers |= M::SHIFT;
        }
        if p[0] & 8 != 0 {
            modifiers |= M::ALT;
        }
        if p[0] & 16 != 0 {
            modifiers |= M::CONTROL;
        }
        let button = match p[0] & 3 {
            0 => B::Left,
            1 => B::Middle,
            _ => B::Right,
        };
        let kind = if p[0] & 64 != 0 {
            match p[0] & 3 {
                0 => MK::ScrollUp,
                1 => MK::ScrollDown,
                2 => MK::ScrollLeft,
                _ => MK::ScrollRight,
            }
        } else if final_char == 'm' {
            MK::Up(button)
        } else if p[0] & 32 != 0 {
            if p[0] & 3 == 3 {
                MK::Moved
            } else {
                MK::Drag(button)
            }
        } else {
            MK::Down(button)
        };
        return Some(Event::Mouse(MouseEvent {
            kind,
            column: p[1].saturating_sub(1),
            row: p[2].saturating_sub(1),
            modifiers,
        }));
    }
    if params.is_empty() && final_char == 'I' {
        return Some(Event::FocusGained);
    }
    if params.is_empty() && final_char == 'O' {
        return Some(Event::FocusLost);
    }
    let p = params
        .split(';')
        .map(|p| p.parse::<u16>().unwrap_or(1))
        .collect::<Vec<_>>();
    let bits = p.get(1).copied().unwrap_or(1).saturating_sub(1);
    let mut modifiers = M::NONE;
    if bits & 1 != 0 {
        modifiers |= M::SHIFT;
    }
    if bits & 2 != 0 {
        modifiers |= M::ALT;
    }
    if bits & 4 != 0 {
        modifiers |= M::CONTROL;
    }
    let code = match final_char {
        'A' => K::Up,
        'B' => K::Down,
        'C' => K::Right,
        'D' => K::Left,
        'H' => K::Home,
        'F' => K::End,
        'Z' => {
            modifiers |= M::SHIFT;
            K::BackTab
        }
        'P' => K::F(1),
        'Q' => K::F(2),
        'R' => K::F(3),
        'S' => K::F(4),
        'u' => match p[0] {
            13 => K::Enter,
            9 => K::Tab,
            27 => K::Esc,
            127 => K::Backspace,
            value => K::Char(char::from_u32(value as u32)?),
        },
        '~' => match p[0] {
            1 | 7 => K::Home,
            2 => K::Insert,
            3 => K::Delete,
            4 | 8 => K::End,
            5 => K::PageUp,
            6 => K::PageDown,
            11..=15 => K::F((p[0] - 10) as u8),
            17..=21 => K::F((p[0] - 11) as u8),
            23..=24 => K::F((p[0] - 12) as u8),
            _ => return None,
        },
        _ => return None,
    };
    Some(Event::Key(KeyEvent::new(code, modifiers)))
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{ffi::c_void, sync::Mutex, time::Instant};
    type Handle = *mut c_void;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyRecord {
        down: i32,
        repeat: u16,
        virtual_key: u16,
        scan: u16,
        unicode: u16,
        modifiers: u32,
    }
    #[repr(C)]
    union RecordData {
        key: KeyRecord,
        padding: [u32; 4],
    }
    #[repr(C)]
    struct Record {
        kind: u16,
        data: RecordData,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
        fn PeekConsoleInputW(handle: Handle, records: *mut Record, len: u32, read: *mut u32)
            -> i32;
        fn ReadConsoleInputW(handle: Handle, records: *mut Record, len: u32, read: *mut u32)
            -> i32;
        fn WaitForSingleObject(handle: Handle, timeout: u32) -> u32;
    }
    #[derive(Default)]
    struct State {
        decoder: PasteDecoder,
        surrogate: Option<u16>,
        escape_since: Option<Instant>,
    }
    // Shared across idle/live ownership transfers; no second OS reader/thread.
    static STATE: Mutex<Option<State>> = Mutex::new(None);
    pub struct RawReader {
        handle: Handle,
        previous_vt: bool,
    }
    impl RawReader {
        pub fn new() -> io::Result<Self> {
            unsafe {
                let handle = GetStdHandle(-10i32 as u32);
                let mut mode = 0;
                if GetConsoleMode(handle, &mut mode) == 0
                    || SetConsoleMode(handle, mode | 0x200) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(Self {
                    handle,
                    previous_vt: mode & 0x200 != 0,
                })
            }
        }
        pub fn next(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
            // Crossterm mouse capture replaces the Windows input flags. Restore VT
            // before reading so ConPTY preserves bracketed-paste delimiters.
            unsafe {
                let mut mode = 0;
                if GetConsoleMode(self.handle, &mut mode) == 0 || SetConsoleMode(self.handle, mode | 0x200) == 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            let deadline = Instant::now() + timeout;
            let mut shared = STATE.lock().unwrap_or_else(|e| e.into_inner());
            let state = shared.get_or_insert_with(State::default);
            loop {
                if let Some(event) = state.decoder.ready.pop_front() {
                    return Ok(Some(event));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                unsafe {
                    match WaitForSingleObject(
                        self.handle,
                        remaining.as_millis().min(u32::MAX as u128) as u32,
                    ) {
                        0 => {}
                        258 => {
                            if state.decoder.prefix == "\x1b"
                                && state
                                    .escape_since
                                    .is_some_and(|at| at.elapsed() >= Duration::from_millis(50))
                            {
                                state.decoder.prefix.clear();
                                state.escape_since = None;
                                return Ok(Some(Event::Key(crossterm::event::KeyEvent::new(
                                    crossterm::event::KeyCode::Esc,
                                    crossterm::event::KeyModifiers::NONE,
                                ))));
                            }
                            return Ok(None);
                        }
                        _ => return Err(io::Error::last_os_error()),
                    }
                    let mut record = Record {
                        kind: 0,
                        data: RecordData { padding: [0; 4] },
                    };
                    let mut count = 0;
                    if PeekConsoleInputW(self.handle, &mut record, 1, &mut count) == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if count == 0 {
                        continue;
                    }
                    let key = if record.kind == 1 {
                        Some(record.data.key)
                    } else {
                        None
                    };
                    // Modifier-only records would make crossterm read ahead and
                    // swallow the following raw VT escape, so consume them here.
                    let modifier = key.is_some_and(|key| {
                        matches!(key.virtual_key, 0x10 | 0x11 | 0x12) && key.unicode == 0
                    });
                    let raw =
                        key.is_some_and(|key| key.virtual_key == 0 || state.decoder.capturing());
                    let ignored = !matches!(record.kind, 1 | 2 | 4 | 16);
                    if raw || modifier || ignored {
                        if ReadConsoleInputW(self.handle, &mut record, 1, &mut count) == 0 {
                            return Err(io::Error::last_os_error());
                        }
                        if raw {
                            if let Some(key) = key.filter(|key| {
                                (key.down != 0 || key.virtual_key == 0x12)
                                    && (key.unicode != 0 || key.virtual_key == 0)
                            }) {
                                for _ in 0..key.repeat.max(1) {
                                    if (0xd800..=0xdbff).contains(&key.unicode) {
                                        state.surrogate = Some(key.unicode);
                                        continue;
                                    }
                                    let ch = if let Some(high) = state.surrogate.take() {
                                        std::char::decode_utf16([high, key.unicode])
                                            .next()
                                            .and_then(Result::ok)
                                    } else {
                                        char::from_u32(key.unicode as u32)
                                    };
                                    if let Some(ch) = ch {
                                        state.decoder.push(ch);
                                        state.escape_since = if state.decoder.prefix == "\x1b" {
                                            Some(Instant::now())
                                        } else {
                                            None
                                        };
                                    }
                                }
                            }
                        }
                    } else {
                        // crossterm reads a single console record. Never run a
                        // second crossterm poller while this reader owns input.
                        if crossterm::event::poll(Duration::ZERO)? {
                            return crossterm::event::read().map(Some);
                        }
                    }
                }
                if Instant::now() >= deadline {
                    return Ok(None);
                }
            }
        }
    }
    impl Drop for RawReader {
        fn drop(&mut self) {
            unsafe {
                let mut mode = 0;
                if GetConsoleMode(self.handle, &mut mode) != 0 {
                    let mode = if self.previous_vt {
                        mode | 0x200
                    } else {
                        mode & !0x200
                    };
                    SetConsoleMode(self.handle, mode);
                }
            }
        }
    }
}
#[cfg(windows)]
use windows::RawReader;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vt_keys_and_mouse_keep_their_meaning_outside_paste() {
        use crossterm::event::{KeyCode, KeyModifiers, MouseEventKind};
        assert!(
            matches!(decode_sequence("\x1b[Z"),Some(Event::Key(k)) if k.code==KeyCode::BackTab && k.modifiers.contains(KeyModifiers::SHIFT))
        );
        assert!(
            matches!(decode_sequence("\x1b[1;5D"),Some(Event::Key(k)) if k.code==KeyCode::Left && k.modifiers.contains(KeyModifiers::CONTROL))
        );
        assert!(
            matches!(decode_sequence("\x1b[13;2u"),Some(Event::Key(k)) if k.code==KeyCode::Enter && k.modifiers.contains(KeyModifiers::SHIFT))
        );
        assert!(
            matches!(decode_sequence("\x1b[<64;5;6M"),Some(Event::Mouse(m)) if m.kind==MouseEventKind::ScrollUp && m.column==4 && m.row==5)
        );
        assert!(
            matches!(decode_sequence("\x1b[<67;5;6M"),Some(Event::Mouse(m)) if m.kind==MouseEventKind::ScrollRight)
        );
    }
    #[test]
    fn paste_delimiters_preserve_lines_unicode_and_only_external_enter_submits() {
        let mut parser = PasteDecoder::default();
        for ch in "\x1b[200~first\r\n\n中文 😀\rthird\x1b[201~\r".chars() {
            parser.push(ch);
        }
        assert_eq!(
            parser.ready.pop_front(),
            Some(Event::Paste("first\n\n中文 😀\nthird".into()))
        );
        assert!(
            matches!(parser.ready.pop_front(),Some(Event::Key(key)) if key.code==crossterm::event::KeyCode::Enter)
        );
        assert!(parser.ready.is_empty());
    }
    #[test]
    fn fragmented_paste_never_emits_partial_lines_or_commands() {
        let mut parser = PasteDecoder::default();
        for fragment in ["\x1b[2", "00~hello\n", "/exit\r", "\x1b[20"] {
            for ch in fragment.chars() {
                parser.push(ch);
            }
            assert!(parser.ready.is_empty());
        }
        for ch in "1~".chars() {
            parser.push(ch);
        }
        assert_eq!(
            parser.ready.pop_front(),
            Some(Event::Paste("hello\n/exit\n".into()))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn ctrl_h_from_a_terminal_backspace_deletes() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ctrl_h = Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert!(matches!(super::backspace_from_ctrl_h(ctrl_h), Event::Key(key) if key.code == KeyCode::Backspace && key.modifiers.is_empty()));
        let alt_h = Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT));
        assert!(matches!(super::backspace_from_ctrl_h(alt_h), Event::Key(key) if key.code == KeyCode::Char('h')));
    }
}


/// Coalesce unbracketed terminal paste bursts before any embedded Enter can submit.
pub struct Reader {
    raw: RawReader,
    pending: std::collections::VecDeque<Event>,
}
impl Reader {
    pub fn new() -> io::Result<Self> { Ok(Self { raw: RawReader::new()?, pending: Default::default() }) }
    pub fn next(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if let Some(event) = self.pending.pop_front() { return Ok(Some(event)); }
        let Some(first) = self.raw.next(timeout)? else { return Ok(None); };
        if paste_character(&first).is_none() { return Ok(Some(first)); }
        let mut events = vec![first];
        while let Some(event) = self.raw.next(Duration::from_millis(8))? {
            if matches!(&event, Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Release) { continue; }
            if paste_character(&event).is_none() { self.pending.push_back(event); break; }
            events.push(event);
            if events.len() >= 1_000_000 { break; }
        }
        let multiline = events.iter().any(|event| paste_character(event) == Some('\n'));
        if events.len() >= 8 || (events.len() >= 2 && multiline) {
            Ok(Some(Event::Paste(events.iter().filter_map(paste_character).collect())))
        } else {
            let first = events.remove(0);
            for event in events.into_iter().rev() { self.pending.push_front(event); }
            Ok(Some(first))
        }
    }
}
fn paste_character(event: &Event) -> Option<char> {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    let Event::Key(key) = event else { return None; };
    if key.kind == KeyEventKind::Release || key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) { return None; }
    match key.code { KeyCode::Char(ch) => Some(ch), KeyCode::Enter => Some('\n'), KeyCode::Tab => Some('\t'), _ => None }
}

pub fn paste_label(text: &str) -> Option<String> {
    let count = text.chars().count();
    (count >= 200 || text.lines().count() >= 3).then(|| format!("[Pasted Content {count} chars]"))
}


#[cfg(test)]
mod burst_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    #[test]
    fn burst_enter_remains_text_and_unicode_count_is_not_bytes() {
        let events = [Event::Key(KeyEvent::new(KeyCode::Char('中'), KeyModifiers::NONE)), Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))];
        assert_eq!(events.iter().filter_map(paste_character).collect::<String>(), "中\n");
        assert_eq!(paste_label(&"中".repeat(200)).as_deref(), Some("[Pasted Content 200 chars]"));
        assert!(paste_label("hello").is_none());
        assert_eq!(paste_label("a\nb\nc").as_deref(), Some("[Pasted Content 5 chars]"));
    }
}
