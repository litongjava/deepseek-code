//! Own the alternate screen for the entire REPL, restoring it on every exit.
use crossterm::{
    cursor::MoveTo,
    execute,
    terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, IsTerminal};

#[derive(Default)]
pub struct ScreenMode {
    fullscreen: bool,
}
impl ScreenMode {
    /// Returns true when the new screen needs its transcript replayed.
    pub fn set(&mut self, mode: &str) -> io::Result<bool> {
        let fullscreen = mode == "fullscreen" && !crate::terminal::screen_reader();
        if fullscreen == self.fullscreen {
            return Ok(false);
        }
        if !io::stderr().is_terminal() {
            return Err(io::Error::other(
                "Fullscreen requires an interactive terminal",
            ));
        }
        if fullscreen {
            execute!(
                io::stderr(),
                EnterAlternateScreen,
                Clear(ClearType::All),
                MoveTo(0, 0)
            )?;
        } else {
            execute!(io::stderr(), LeaveAlternateScreen)?;
        }
        self.fullscreen = fullscreen;
        Ok(fullscreen)
    }
}
impl Drop for ScreenMode {
    fn drop(&mut self) {
        if self.fullscreen {
            let _ = execute!(io::stderr(), LeaveAlternateScreen);
        }
    }
}

/// Final safety net: restore the shell's exact console modes after all TUI guards.
#[cfg(windows)]
pub struct ConsoleModes(Vec<(*mut std::ffi::c_void, u32)>);
#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetStdHandle(which: u32) -> *mut std::ffi::c_void;
    fn GetConsoleMode(handle: *mut std::ffi::c_void, mode: *mut u32) -> i32;
    fn SetConsoleMode(handle: *mut std::ffi::c_void, mode: u32) -> i32;
}
#[cfg(windows)]
impl ConsoleModes {
    pub fn capture() -> Self {
        let mut modes = Vec::new();
        for which in [-10i32, -11, -12] {
            unsafe {
                let handle = GetStdHandle(which as u32);
                let mut mode = 0;
                if GetConsoleMode(handle, &mut mode) != 0 {
                    modes.push((handle, mode));
                }
            }
        }
        Self(modes)
    }
}
#[cfg(windows)]
impl Drop for ConsoleModes {
    fn drop(&mut self) {
        for &(handle, mode) in &self.0 {
            unsafe {
                SetConsoleMode(handle, mode);
            }
        }
    }
}

/// Inline REPL output is ephemeral; persisted session history remains on disk.
pub struct ExitCleanup(pub bool);
impl Drop for ExitCleanup {
    fn drop(&mut self) {
        if !self.0 {
            return;
        }
        use crossterm::{
            cursor::Show,
            event::{DisableBracketedPaste, DisableMouseCapture},
            style::ResetColor,
        };
        let _ = execute!(
            io::stderr(),
            DisableMouseCapture,
            DisableBracketedPaste,
            ResetColor,
            Show
        );
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = execute!(
            io::stderr(),
            Clear(ClearType::All),
            Clear(ClearType::Purge),
            MoveTo(0, 0)
        );
    }
}
