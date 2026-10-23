//! Remote Control's platform layer: input, the permissions it needs, and display screenshots
//! from the shared capture-only package. Only this executable posts input (#2665).

/// A mouse button, for Remote Control input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// A Remote Control input failure the server can name: `rtx-control` answers it with its `code`
/// as well as the message.
#[derive(Debug)]
// Only Windows reports one so far.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct InputError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InputError {}

/// What this computer's clipboard holds, as Remote Control reads it for a viewer (#2682).
pub enum ClipboardContent {
    Empty,
    Text(String),
    /// Encoded image bytes: PNG, or BMP (Windows bitmaps) that the viewer turns into PNG.
    Image {
        bytes: Vec<u8>,
        mime: &'static str,
    },
    /// Absolute paths of copied files and folders.
    Files(Vec<String>),
}

pub use rtx_screens::{capture_display, capture_display_frame, init_window_server, Frame, Shot};
// Numbers live-view frames (#2683): the Wayland capture here, and peer.rs's tests.
#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
pub use rtx_screens::next_frame_seq;

pub const PLATFORM: &str = std::env::consts::OS;

/// Whether this build implements Remote Control input (`rtx-control --check`).
pub const INPUT_AVAILABLE: bool = cfg!(any(target_os = "macos", windows, target_os = "linux"));

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{
    control_permissions, permission_owner, request_control_permission, screenshot_needs_input,
    Input,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{
    control_permissions, permission_owner, request_control_permission, screenshot_needs_input,
    Input,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{
    control_permissions, permission_owner, request_control_permission, screenshot_needs_input,
    Input,
};

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod unsupported;
#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub use unsupported::{
    control_permissions, permission_owner, request_control_permission, screenshot_needs_input,
    Input,
};
