//! Terminal colors selected by persisted CLI preferences.
use crossterm::style::Color;
use std::sync::atomic::{AtomicU8, Ordering};

static THEME: AtomicU8 = AtomicU8::new(0);
static ACCENT: AtomicU8 = AtomicU8::new(0);

pub fn configure(theme: &str, color: &str) {
    THEME.store(
        match theme {
            "light" => 1,
            "dark-daltonized" => 2,
            "light-daltonized" => 3,
            "dark-ansi" => 4,
            "light-ansi" => 5,
            _ => 0,
        },
        Ordering::Relaxed,
    );
    ACCENT.store(
        match color {
            "red" => 1,
            "orange" => 2,
            "yellow" => 3,
            "green" => 4,
            "blue" => 5,
            "purple" => 6,
            "pink" => 7,
            "cyan" => 8,
            _ => 0,
        },
        Ordering::Relaxed,
    );
}

pub fn muted() -> Color {
    if THEME.load(Ordering::Relaxed) % 2 == 1 {
        Color::Rgb {
            r: 102,
            g: 102,
            b: 102,
        }
    } else {
        Color::Rgb {
            r: 153,
            g: 153,
            b: 153,
        }
    }
}

pub fn selected() -> Color {
    match THEME.load(Ordering::Relaxed) {
        1 => Color::Rgb {
            r: 70,
            g: 80,
            b: 180,
        },
        2 | 3 => Color::Cyan,
        4 | 5 => Color::Blue,
        _ => Color::Rgb {
            r: 177,
            g: 185,
            b: 249,
        },
    }
}

pub fn border() -> Color {
    match ACCENT.load(Ordering::Relaxed) {
        1 => Color::Red,
        2 => Color::Rgb {
            r: 215,
            g: 119,
            b: 87,
        },
        3 => Color::Yellow,
        4 => Color::Green,
        5 => Color::Blue,
        6 => Color::Magenta,
        7 => Color::Rgb {
            r: 224,
            g: 130,
            b: 180,
        },
        8 => Color::Cyan,
        _ => muted(),
    }
}

pub fn activity() -> &'static str {
    match THEME.load(Ordering::Relaxed) {
        1 => "38;2;166;74;44",
        2 | 3 => "36",
        4 | 5 => "33",
        _ => "38;2;215;119;87",
    }
}

pub fn accent() -> Color {
    match THEME.load(Ordering::Relaxed) {
        1 => Color::Rgb {
            r: 166,
            g: 74,
            b: 44,
        },
        2 | 3 => Color::Cyan,
        4 | 5 => Color::Yellow,
        _ => Color::Rgb {
            r: 215,
            g: 119,
            b: 87,
        },
    }
}

pub fn success() -> Color {
    match THEME.load(Ordering::Relaxed) {
        1 => Color::Rgb {
            r: 22,
            g: 128,
            b: 35,
        },
        2 | 3 => Color::Blue,
        4 | 5 => Color::Green,
        _ => Color::Rgb {
            r: 78,
            g: 186,
            b: 101,
        },
    }
}

pub fn muted_ansi() -> &'static str {
    if THEME.load(Ordering::Relaxed) % 2 == 1 {
        "38;2;102;102;102"
    } else {
        "38;2;153;153;153"
    }
}

pub fn input_background() -> Color {
    match THEME.load(Ordering::Relaxed) {
        4 => Color::DarkGrey,
        5 => Color::Grey,
        1 | 3 => Color::Rgb {
            r: 235,
            g: 235,
            b: 235,
        },
        _ => Color::Rgb {
            r: 55,
            g: 55,
            b: 55,
        },
    }
}
pub fn input_foreground() -> Color {
    if THEME.load(Ordering::Relaxed) % 2 == 1 {
        Color::Black
    } else {
        Color::White
    }
}

pub fn theme_variant() -> u8 {
    THEME.load(Ordering::Relaxed)
}

/// The footer's permission mode label, as in the reference CLI, e.g.
/// `⏵⏵ accept edits on (shift+tab to cycle)`; none for the default ask mode.
pub fn mode_indicator(mode: &str) -> Option<(String, Color)> {
    let red = Color::Rgb { r: 255, g: 107, b: 107 };
    let (symbol, title, color) = match mode {
        "accept-edits" => ("\u{23f5}\u{23f5}", "accept edits", Color::Rgb { r: 175, g: 135, b: 255 }),
        "plan" => ("\u{23f8}", "plan mode", Color::Rgb { r: 72, g: 160, b: 150 }),
        "bypass" => ("\u{23f5}\u{23f5}", "bypass permissions", red),
        "dont-ask" => ("\u{23f5}\u{23f5}", "don't ask", red),
        "auto" => ("\u{23f5}\u{23f5}", "auto mode", Color::Yellow),
        _ => return None,
    };
    Some((format!("{symbol} {title} on (shift+tab to cycle)"), color))
}

/// Keyboard shortcuts listed by `?` on an empty prompt.
pub const SHORTCUTS: &[(&str, &str)] = &[
    ("/", "commands"),
    ("enter", "send (queues while working)"),
    ("alt+enter", "new line"),
    ("shift+tab", "cycle permission modes"),
    ("esc", "interrupt the current turn"),
    ("ctrl+o", "expand the last turn's tool calls"),
    ("tab", "accept the suggested prompt"),
    ("\u{2191} \u{2193}", "prompt history"),
    ("ctrl+r", "search prompt history"),
    ("ctrl+u / ctrl+k", "delete before / after the cursor"),
    ("ctrl+l", "redraw"),
    ("ctrl+d", "exit on an empty prompt"),
    ("/btw", "ask a side question while working"),
];
