//! Remote Control input on Linux: GNOME on Wayland through Mutter's remote-desktop session, and
//! X11 through the XTest extension.

use anyhow::{bail, Result};

use rtx_screens::x11;

use self::wayland_input::WaylandInput;
use self::x11_input::X11Input;
use super::{Button, Frame, Shot};

mod terminal;
mod wayland_input;
mod x11_clipboard;
mod x11_input;

/// On Wayland the screenshot comes from the remote-desktop session's own stream.
pub fn screenshot_needs_input() -> bool {
    x11::wayland()
}

/// Nothing to grant on Linux: GNOME shows its sharing indicator instead.
pub fn control_permissions() -> (Option<bool>, Option<bool>) {
    (None, None)
}

pub fn permission_owner() -> Option<String> {
    None
}

pub fn request_control_permission(name: &str) -> Result<()> {
    bail!("{name} needs no permission on Linux")
}

pub enum Input {
    Wayland(WaylandInput),
    // Boxed: an X11 session holds the keyboard map and a clipboard owner.
    X11(Box<X11Input>),
}

impl Input {
    pub fn new() -> Result<Self> {
        Ok(if x11::wayland() {
            Self::Wayland(WaylandInput::new()?)
        } else {
            Self::X11(Box::new(X11Input::new()?))
        })
    }

    pub fn screen_size(&self) -> Option<(f64, f64)> {
        match self {
            Self::Wayland(input) => input.screen_size(),
            Self::X11(input) => input.screen_size(),
        }
    }

    /// Only Wayland takes screenshots through input (`screenshot_needs_input`).
    pub fn screenshot(&mut self, max_width: u32, live: bool) -> Result<Shot> {
        match self {
            Self::Wayland(input) => input.screenshot(max_width, live),
            Self::X11(_) => x11::capture_display(max_width),
        }
    }

    /// A live-view video frame, like [`Self::screenshot`].
    pub fn frame(&mut self, max_width: u32) -> Result<Frame> {
        match self {
            Self::Wayland(input) => input.frame(max_width),
            Self::X11(_) => x11::capture_display_frame(max_width),
        }
    }

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        match self {
            Self::Wayland(input) => input.move_to(x, y),
            Self::X11(input) => input.move_to(x, y),
        }
    }

    pub fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        match self {
            Self::Wayland(input) => input.button(button, pressed),
            Self::X11(input) => input.button(button, pressed),
        }
    }

    pub fn keysym(&mut self, keysym: u32, pressed: bool) -> Result<()> {
        match self {
            Self::Wayland(input) => input.keysym(keysym, pressed),
            Self::X11(input) => input.keysym(keysym, pressed),
        }
    }

    pub fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        match self {
            Self::Wayland(input) => input.scroll(dx, dy),
            Self::X11(input) => input.scroll(dx, dy),
        }
    }

    pub fn paste_text(&mut self, text: &str, terminal: bool) -> Result<()> {
        match self {
            Self::Wayland(input) => input.paste_text(text, terminal),
            Self::X11(input) => input.paste_text(text, terminal),
        }
    }

    /// The bitmap is Windows'; X11 and Wayland apps take the PNG.
    pub fn paste_image(&mut self, png: &[u8], _dib: Option<&[u8]>) -> Result<()> {
        match self {
            Self::Wayland(input) => input.paste_image(png),
            Self::X11(input) => input.paste_image(png),
        }
    }

    pub fn paste_files(&mut self, paths: &[String]) -> Result<()> {
        match self {
            Self::Wayland(input) => input.paste_files(paths),
            Self::X11(input) => input.paste_files(paths),
        }
    }

    pub fn clipboard_seq(&mut self) -> Result<(u64, bool)> {
        match self {
            Self::Wayland(input) => input.clipboard_seq(),
            Self::X11(input) => input.clipboard_seq(),
        }
    }

    pub fn clipboard_read(&mut self) -> Result<crate::platform::ClipboardContent> {
        match self {
            Self::Wayland(input) => input.clipboard_read(),
            Self::X11(input) => input.clipboard_read(),
        }
    }
}

/// A path as a file:// URI, as file managers put it on the clipboard: bytes outside the
/// unreserved set and '/' are percent-encoded (UTF-8 names included).
pub(crate) fn file_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// The path of a file:// URI (percent-decoded); other text as it is.
pub(crate) fn path_of_uri(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_string();
    };
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod uri_tests {
    #[test]
    fn file_uris_encode_spaces_and_unicode_and_decode_back() {
        let path = "/home/a/Tải xuống/báo cáo #1.pdf";
        let uri = super::file_uri(path);
        assert_eq!(
            uri,
            "file:///home/a/T%E1%BA%A3i%20xu%E1%BB%91ng/b%C3%A1o%20c%C3%A1o%20%231.pdf"
        );
        assert_eq!(super::path_of_uri(&uri), path);
        assert_eq!(super::path_of_uri("not a uri"), "not a uri");
    }
}
