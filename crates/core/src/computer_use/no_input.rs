//! Android lets no ordinary app drive the input of others, so desktop input control
//! reports that clearly. Same names as the desktop input library keep one code path.
use anyhow::Result;

const UNAVAILABLE: &str = "Mouse and keyboard control is not available on Android";

#[derive(Clone, Copy, Debug)]
pub enum Button { Left, Right, Middle, Back, Forward }
#[derive(Clone, Copy, Debug)]
pub enum Direction { Press, Release, Click }
#[derive(Clone, Copy, Debug)]
pub enum Coordinate { Abs }
#[derive(Clone, Copy, Debug)]
pub enum Axis { Horizontal, Vertical }
#[derive(Clone, Copy, Debug)]
pub enum Key {
    Alt, Backspace, CapsLock, Control, Delete, DownArrow, End, Escape, F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    Home, Insert, LeftArrow, Meta, Numlock, PageDown, PageUp, Pause, Print, Return, RightArrow, Scroll, ScrollLock,
    Shift, Snapshot, Space, Tab, UpArrow, Unicode(char),
}
#[derive(Default)]
pub struct Settings;
pub struct Enigo;

impl Enigo {
    pub fn new(_: &Settings) -> Result<Self> {
        anyhow::bail!(UNAVAILABLE)
    }
}

// `Enigo::new` always fails, so these exist only to satisfy the shared code path.
pub trait Mouse {
    fn move_mouse(&mut self, _: i32, _: i32, _: Coordinate) -> Result<()> { anyhow::bail!(UNAVAILABLE) }
    fn button(&mut self, _: Button, _: Direction) -> Result<()> { anyhow::bail!(UNAVAILABLE) }
    fn scroll(&mut self, _: i32, _: Axis) -> Result<()> { anyhow::bail!(UNAVAILABLE) }
    fn main_display(&self) -> Result<(i32, i32)> { anyhow::bail!(UNAVAILABLE) }
}
pub trait Keyboard {
    fn text(&mut self, _: &str) -> Result<()> { anyhow::bail!(UNAVAILABLE) }
    fn key(&mut self, _: Key, _: Direction) -> Result<()> { anyhow::bail!(UNAVAILABLE) }
}
impl Mouse for Enigo {}
impl Keyboard for Enigo {}
