//! CLI Module - Command line interface
//!
//! Provides CLI-specific functionality: input handling, output rendering,
//! initialization, and main entry point orchestration.

use serde::{Deserialize, Serialize};

static SCREEN_READER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn configure_accessibility(enabled: bool) {
    SCREEN_READER.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
pub fn screen_reader() -> bool {
    SCREEN_READER.load(std::sync::atomic::Ordering::Relaxed)
}
/// The startup header, as in the reference CLI: product and version, the
/// model, and the workspace with the home directory shortened to `~`.
pub fn banner(model: &str, workspace: &std::path::Path) -> String {
    let path = match dirs::home_dir().and_then(|home| workspace.strip_prefix(home).ok().map(std::path::Path::to_path_buf)) {
        Some(relative) if relative.as_os_str().is_empty() => "~".to_owned(),
        Some(relative) => format!("~{}{}", std::path::MAIN_SEPARATOR, relative.display()),
        None => workspace.display().to_string(),
    };
    let model: String = model.chars().filter(|ch| !ch.is_control()).collect();
    let path: String = path.chars().filter(|ch| !ch.is_control()).collect();
    let title = format!("DeepSeek Code v{}", env!("CARGO_PKG_VERSION"));
    if screen_reader() || std::env::var_os("NO_COLOR").is_some() {
        return format!("{title}\n{model}\n{path}\n");
    }
    use crossterm::style::{Attribute, ResetColor, SetAttribute, SetForegroundColor};
    let accent = SetForegroundColor(style::accent());
    let muted = SetForegroundColor(style::muted());
    format!(
        " {accent}\u{273b}{ResetColor} {}{title}{}\n   {muted}{model} \u{b7} /help for commands{ResetColor}\n   {muted}{path}{ResetColor}\n",
        SetAttribute(Attribute::Bold),
        SetAttribute(Attribute::Reset),
    )
}

/// The input prompt marker: the reference CLI's `❯`, or `>` for screen
/// readers and dumb terminals.
pub fn prompt_glyph() -> &'static str {
    if screen_reader() || std::env::var("TERM").as_deref() == Ok("dumb") {
        ">"
    } else {
        "\u{276f}"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliConfig {
    pub verbose: bool,
    pub quiet: bool,
    pub output_format: OutputFormat,
    pub no_color: bool,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            verbose: false,
            quiet: false,
            output_format: OutputFormat::Pretty,
            no_color: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OutputFormat {
    Pretty,
    Compact,
    Json,
    Markdown,
}

pub mod input;
pub mod progress;
pub mod prompt_display;
pub mod live;
pub(crate) mod paste_input;
pub mod change_view;
pub mod tool_view;
pub mod transcript_pager;
pub mod picker;
pub mod tips;
pub mod markdown;
pub mod vim;
mod display_path;
pub mod style;
pub mod screen_mode;

/// Assemble prompts without creating another stdin reader. Explicit multiline
/// mode treats slash commands as literal text until /end or /cancel.
pub mod prompt {
    #[derive(Debug, PartialEq)]
    pub enum Input {
        Pending,
        Cancelled,
        Command(String),
        Prompt(String),
    }

    #[derive(Default)]
    pub struct Composer {
        lines: Vec<String>,
        multiline: bool,
    }

    impl Composer {
        pub fn is_pending(&self) -> bool {
            self.multiline || !self.lines.is_empty()
        }

        pub fn cancel(&mut self) {
            self.lines.clear();
            self.multiline = false;
        }

        pub fn push(&mut self, line: String) -> Input {
            if self.is_pending() && line.trim() == "/cancel" {
                self.cancel();
                return Input::Cancelled;
            }
            if self.multiline {
                if line.trim() == "/end" {
                    self.multiline = false;
                    return Input::Prompt(std::mem::take(&mut self.lines).join("\n"));
                }
                self.lines.push(line);
                return Input::Pending;
            }
            if !self.is_pending() && line.trim() == "/multiline" {
                self.multiline = true;
                return Input::Pending;
            }
            // An odd number of trailing backslashes continues the prompt;
            // pairs remain literal so Windows paths can be entered unchanged.
            let trailing = line.chars().rev().take_while(|c| *c == '\\').count();
            if trailing % 2 == 1 {
                self.lines.push(line[..line.len() - 1].to_owned());
                return Input::Pending;
            }
            if !self.lines.is_empty() {
                self.lines.push(line);
                return Input::Prompt(std::mem::take(&mut self.lines).join("\n"));
            }
            Input::Command(line)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn multiline_preserves_whitespace_and_literal_commands() {
            let mut composer = Composer::default();
            assert_eq!(composer.push("/multiline".into()), Input::Pending);
            for line in ["  code  ", "", "/exit"] {
                assert_eq!(composer.push(line.into()), Input::Pending);
            }
            assert_eq!(
                composer.push("/end".into()),
                Input::Prompt("  code  \n\n/exit".into())
            );
            assert!(!composer.is_pending());
        }

        #[test]
        fn continuation_and_cancellation_reset_state() {
            let mut composer = Composer::default();
            assert_eq!(composer.push("first\\".into()), Input::Pending);
            assert_eq!(
                composer.push("second".into()),
                Input::Prompt("first\nsecond".into())
            );
            assert_eq!(composer.push("discard\\".into()), Input::Pending);
            assert_eq!(composer.push("/cancel".into()), Input::Cancelled);
            assert_eq!(
                composer.push("hello".into()),
                Input::Command("hello".into())
            );
            composer.push("/multiline".into());
            composer.cancel();
            assert!(!composer.is_pending());
        }

        #[test]
        fn double_backslash_is_literal() {
            let mut composer = Composer::default();
            assert_eq!(
                composer.push("path\\\\".into()),
                Input::Command("path\\\\".into())
            );
        }
    }
}

pub mod render {
    use super::OutputFormat;

    pub struct Renderer {
        format: OutputFormat,
        no_color: bool,
    }

    impl Default for Renderer {
        fn default() -> Self {
            Self::new(OutputFormat::Pretty, false)
        }
    }

    impl Renderer {
        pub fn new(format: OutputFormat, no_color: bool) -> Self {
            Self { format, no_color }
        }

        pub fn render(&self, content: &str) -> String {
            match self.format {
                OutputFormat::Json => self.render_json(content),
                OutputFormat::Compact | OutputFormat::Markdown => content.to_string(),
                _ => self.render_pretty(content),
            }
        }

        fn render_pretty(&self, content: &str) -> String {
            if self.no_color {
                content.to_string()
            } else {
                format!("\x1b[1;34m{}\x1b[0m", content)
            }
        }

        fn render_json(&self, content: &str) -> String {
            serde_json::to_string_pretty(&serde_json::json!({
                "output": content
            }))
            .unwrap_or_else(|_| content.to_string())
        }
    }
}

pub mod init {
    use std::path::Path;

    pub fn ensure_directories(base_path: &Path) -> std::io::Result<()> {
        let dirs = [
            ".claude",
            ".claude/sessions",
            ".claude/logs",
            ".claude/plugins",
        ];

        for dir in &dirs {
            let path = base_path.join(dir);
            if !path.exists() {
                std::fs::create_dir_all(&path)?;
            }
        }

        Ok(())
    }
}

#[path = "terminal/answer_input.rs"]
mod answer_input;
