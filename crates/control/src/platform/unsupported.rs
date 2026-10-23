//! Remote Control input where it is not implemented yet.

use anyhow::{bail, Result};

use crate::platform::Button;

/// Permissions are a macOS concept here; elsewhere nothing needs granting.
pub fn control_permissions() -> (Option<bool>, Option<bool>) {
    (None, None)
}

pub fn screenshot_needs_input() -> bool {
    false
}

pub fn permission_owner() -> Option<String> {
    None
}

pub fn request_control_permission(name: &str) -> Result<()> {
    bail!("{name} needs no permission on this platform")
}

pub struct Input;

impl Input {
    pub fn new() -> Result<Self> {
        bail!("Remote Control input is not available on this platform yet")
    }
    pub fn screen_size(&self) -> Option<(f64, f64)> {
        None
    }
    pub fn screenshot(&mut self, _max_width: u32, _live: bool) -> Result<crate::platform::Shot> {
        unreachable!()
    }
    pub fn frame(&mut self, _max_width: u32) -> Result<crate::platform::Frame> {
        unreachable!()
    }
    pub fn move_to(&mut self, _x: f64, _y: f64) -> Result<()> {
        unreachable!()
    }
    pub fn button(&mut self, _button: Button, _pressed: bool) -> Result<()> {
        unreachable!()
    }
    pub fn keysym(&mut self, _keysym: u32, _pressed: bool) -> Result<()> {
        unreachable!()
    }
    pub fn scroll(&mut self, _dx: i32, _dy: i32) -> Result<()> {
        unreachable!()
    }
    pub fn paste_text(&mut self, _text: &str, _terminal: bool) -> Result<()> {
        unreachable!()
    }
    pub fn paste_image(&mut self, _png: &[u8], _dib: Option<&[u8]>) -> Result<()> {
        unreachable!()
    }
    pub fn paste_files(&mut self, _paths: &[String]) -> Result<()> {
        unreachable!()
    }
    pub fn clipboard_seq(&mut self) -> Result<(u64, bool)> {
        unreachable!()
    }
    pub fn clipboard_read(&mut self) -> Result<crate::platform::ClipboardContent> {
        unreachable!()
    }
}
