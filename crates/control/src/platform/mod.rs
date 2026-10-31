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

/// How long a paste's text stays on the clipboard before the next paste may replace it: the target
/// app reads the clipboard only when it handles the paste key, after the keys were sent. Without
/// it, several pastes in one batch all pasted the last text (Windows QA of Link 0.2.4: "one two
/// three" came out "three three three"; a 30 ms gap was already enough). A single paste
/// doesn't wait.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub const PASTE_GAP: std::time::Duration = std::time::Duration::from_millis(150);

/// Keeps pastes in a row PASTE_GAP apart (macOS and Windows; X11 and Wayland wait themselves).
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
#[derive(Default)]
pub struct PasteGap {
    last: Option<std::time::Instant>,
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
impl PasteGap {
    /// Before writing the clipboard: wait until the last paste had its time.
    pub fn wait(&mut self) {
        let left = self.left(std::time::Instant::now());
        if !left.is_zero() {
            std::thread::sleep(left);
        }
    }

    /// After pressing the paste keys.
    pub fn pasted(&mut self) {
        self.last = Some(std::time::Instant::now());
    }

    fn left(&self, now: std::time::Instant) -> std::time::Duration {
        self.last.map_or(std::time::Duration::ZERO, |last| {
            PASTE_GAP.saturating_sub(now.saturating_duration_since(last))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn only_a_paste_right_after_another_waits() {
        let mut gap = PasteGap::default();
        let start = Instant::now();
        assert_eq!(gap.left(start), Duration::ZERO);
        gap.last = Some(start);
        assert_eq!(gap.left(start), PASTE_GAP);
        assert_eq!(
            gap.left(start + Duration::from_millis(100)),
            Duration::from_millis(50)
        );
        assert_eq!(gap.left(start + PASTE_GAP), Duration::ZERO);
        assert_eq!(gap.left(start + Duration::from_secs(5)), Duration::ZERO);
    }
}

#[cfg(not(target_os = "macos"))]
pub use rtx_screens::{capture_display_frame_on, capture_display_on};
pub use rtx_screens::{init_window_server, Frame, Shot};
// macOS: the lock screen's own picture while it shows (realtimex-link#11).
#[cfg(target_os = "macos")]
pub use macos::{capture_display_frame_on, capture_display_on, displays};

/// The displays, each with the rectangle the pointer reaches it in: physical pixels of the
/// virtual desktop, as listed (Windows, X11). GNOME Wayland lists its own (`Input::displays`).
#[cfg(not(target_os = "macos"))]
pub fn displays() -> anyhow::Result<Vec<crate::displays::Display>> {
    Ok(rtx_screens::displays()?
        .into_iter()
        .map(|info| crate::displays::Display {
            rect: crate::displays::Rect::of(&info),
            info,
        })
        .collect())
}
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
    capture_bounded, control_permissions, permission_owner, request_control_permission,
    screenshot_needs_input, Input,
};

/// macOS, under Link's supervisor (realtimex-link#11): the login-window job's channel.
#[cfg(target_os = "macos")]
pub fn macos_job_channel(fd: i32) {
    macos::job::set_channel(fd);
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{
    control_permissions, follow_input_desktop, permission_owner, request_control_permission,
    screenshot_needs_input, Input,
};

/// Windows moves the control thread to the desktop receiving input (sign-in, lock and UAC
/// screens, realtimex-link#10); other systems have one desktop per session.
#[cfg(not(windows))]
pub fn follow_input_desktop() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{
    control_permissions, permission_owner, request_control_permission, screenshot_needs_input,
    set_virtual_displays, Input,
};

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod unsupported;
#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub use unsupported::{
    control_permissions, permission_owner, request_control_permission, screenshot_needs_input,
    Input,
};
