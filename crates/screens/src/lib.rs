//! Window and display screenshots, shared by RealTimeX's recorder (`rtx-capture`) and Remote
//! Control (`rtx-control`, #2665). Capture only: nothing here posts input, reads keyboard state or
//! uses the clipboard, and this package's dependencies must not either (RealTimeX's no-keyboard
//! check scans this package with the recorder).

pub mod image;
pub mod text;
pub mod window;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
pub use window::WindowInfo;

/// One window screenshot, ready to keep or drop.
pub struct Shot {
    pub jpeg: Vec<u8>,
    /// THUMB_WIDTH x THUMB_HEIGHT grayscale, for change detection.
    pub thumb: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub title: String,
    /// Text recognized on-device (empty where OCR is not available).
    pub text: Vec<crate::text::TextLine>,
}

/// One live-view video frame: packed BGRA (stride `width * 4`), already scaled.
#[derive(Clone)]
pub struct Frame {
    /// Packed BGRA rows; shared, so handing the newest frame to several live views copies nothing.
    pub data: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    /// The picture's identity from [`next_frame_seq`]: the same number means the very same
    /// picture, so an encoder can skip a repeat. 0: not numbered (every frame counts as new).
    pub seq: u64,
}

static NEXT_FRAME_SEQ: AtomicU64 = AtomicU64::new(1);

/// A number for a new picture, unique in this process across capture sources and their restarts,
/// so a restarted source can never repeat a number an encoder has already seen (#2683).
pub fn next_frame_seq() -> u64 {
    NEXT_FRAME_SEQ.fetch_add(1, Ordering::Relaxed)
}

impl Frame {
    /// `img` scaled down to at most `max_width` pixels wide, with even sides (the 4:2:0 encoder
    /// needs them). macOS scales in ScreenCaptureKit instead.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn scaled(img: &crate::image::Bgra, max_width: u32) -> Frame {
        let (w, h) = crate::image::fit_width(img.width, img.height, max_width as usize);
        let (w, h) = ((w & !1).max(2), (h & !1).max(2));
        Frame {
            data: crate::image::scale_bgra(img, w, h).into(),
            width: w as u32,
            height: h as u32,
            seq: 0,
        }
    }
}

/// One native capture request at a time (#2737); used by the macOS backend, tested everywhere.
pub mod gate;
pub use gate::{Busy, Cancelled, TimedOut};

/// ScreenCaptureKit (window and display screenshots, Vision OCR).
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
mod macos_stream;
/// Process snapshots, to name a window's app.
#[cfg(windows)]
pub mod procs;
/// PrintWindow/BitBlt screenshots and Windows.Media.Ocr.
#[cfg(windows)]
pub mod win;
#[cfg(windows)]
mod win_dupl;
/// X11 window list and screenshots (XWayland apps too).
#[cfg(target_os = "linux")]
pub mod x11;

#[cfg(target_os = "macos")]
pub use macos::{capture_display, capture_display_frame, init_window_server};

/// A native capture request this process gave up on that has not answered for so long that it
/// never will, and how long it has been outstanding. While there is one, this process's
/// screenshots are refused; only ending the process frees them.
#[cfg(target_os = "macos")]
pub use macos::stalled_request;

/// Never on other platforms: their captures do not go through the gate.
#[cfg(not(target_os = "macos"))]
pub fn stalled_request() -> Option<(&'static str, std::time::Duration)> {
    None
}

#[cfg(target_os = "linux")]
pub use x11::{capture_display, capture_display_frame};

/// Physical-pixel coordinates on Windows; nothing to prepare on Linux.
#[cfg(not(target_os = "macos"))]
pub fn init_window_server() {
    #[cfg(windows)]
    win::init();
}

#[cfg(windows)]
pub use win::{capture_display, capture_display_frame};

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub fn capture_display(_max_width: u32) -> anyhow::Result<Shot> {
    anyhow::bail!("display screenshots are not supported on this platform yet")
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub fn capture_display_frame(_max_width: u32) -> anyhow::Result<Frame> {
    anyhow::bail!("display screenshots are not supported on this platform yet")
}
