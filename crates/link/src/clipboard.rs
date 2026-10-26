//! The menu bar / tray icon's "Copy App ID and password": straight to the system clipboard. The
//! window's own Copy buttons go through egui (eframe's clipboard feature), but the icon can't: on
//! macOS and Windows its window may be hidden and drawing no frames, so a copy queued in egui's
//! output would wait for the window to open; on Linux the icon is a process with no window at all.

use std::sync::Mutex;

/// The clipboard, opened once and kept for the life of the process. On Linux whoever copied hands
/// the text to each app that pastes it (an X11 selection, a Wayland data source), from a thread
/// arboard keeps only while this lives: dropped after set_text, the clipboard would empty again.
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

/// Put the text on the system clipboard, or say why it couldn't.
pub fn copy(text: &str) -> Result<(), String> {
    let mut kept = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
    let mut clipboard = match kept.take() {
        Some(clipboard) => clipboard,
        None => arboard::Clipboard::new().map_err(|e| e.to_string())?,
    };
    clipboard.set_text(text).map_err(|e| e.to_string())?;
    // Kept only once it worked: after an error it is opened afresh next time (the display or
    // compositor may have gone and come back).
    *kept = Some(clipboard);
    Ok(())
}
