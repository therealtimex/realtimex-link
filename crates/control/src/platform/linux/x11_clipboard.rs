//! Reading the X11 CLIPBOARD for a viewer (#2682): a watcher counts owner changes (XFixes),
//! and a read asks the owner for its TARGETS, then for files, an image or text, the way any app
//! pastes. Large data comes in parts (INCR), as ICCCM describes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, Property, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::CURRENT_TIME;

use crate::platform::ClipboardContent;

/// How long a clipboard owner has to answer (each part of a large transfer, too).
const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);
/// The largest content read (an image over the paste limit is still described by its size).
const MAX_READ_BYTES: usize = 64 * 1024 * 1024;

/// Counts CLIPBOARD owner changes on its own connection, from a thread.
pub struct ClipboardWatcher {
    changes: Arc<AtomicU64>,
}

impl ClipboardWatcher {
    pub fn start() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("connect to the X11 display")?;
        conn.xfixes_query_version(5, 0)?
            .reply()
            .context("XFixes is not available")?;
        let root = conn.setup().roots[screen].root;
        let clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
        conn.xfixes_select_selection_input(
            root,
            clipboard,
            SelectionEventMask::SET_SELECTION_OWNER,
        )?;
        conn.flush()?;
        let changes = Arc::new(AtomicU64::new(1));
        let counter = Arc::clone(&changes);
        std::thread::spawn(move || {
            while let Ok(event) = conn.wait_for_event() {
                if let Event::XfixesSelectionNotify(_) = event {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        Ok(Self { changes })
    }

    pub fn seq(&self) -> u64 {
        self.changes.load(Ordering::SeqCst)
    }
}

/// The current CLIPBOARD content, asked of its owner on a connection of its own.
pub fn read_clipboard() -> Result<ClipboardContent> {
    let (conn, screen) = x11rb::connect(None).context("connect to the X11 display")?;
    let root = conn.setup().roots[screen].root;
    let window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let atom = |name: &str| -> Result<Atom> {
        Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    };
    let clipboard = atom("CLIPBOARD")?;
    let property = atom("RTX_CLIPBOARD_READ")?;
    if conn.get_selection_owner(clipboard)?.reply()?.owner == x11rb::NONE {
        return Ok(ClipboardContent::Empty);
    }
    let targets_atom = atom("TARGETS")?;
    let targets: Vec<Atom> = match convert(&conn, window, clipboard, targets_atom, property)? {
        Some(bytes) => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_ne_bytes(*c))
            .collect(),
        None => return Ok(ClipboardContent::Empty),
    };
    let offered = |name: &str| -> Result<Option<Atom>> {
        let wanted = atom(name)?;
        Ok(targets.contains(&wanted).then_some(wanted))
    };

    for name in ["x-special/gnome-copied-files", "text/uri-list"] {
        if let Some(target) = offered(name)? {
            if let Some(bytes) = convert(&conn, window, clipboard, target, property)? {
                let paths = paths_of_uri_text(&String::from_utf8_lossy(&bytes));
                if !paths.is_empty() {
                    return Ok(ClipboardContent::Files(paths));
                }
            }
        }
    }
    if let Some(target) = offered("image/png")? {
        if let Some(bytes) = convert(&conn, window, clipboard, target, property)? {
            return Ok(ClipboardContent::Image {
                bytes,
                mime: "image/png",
            });
        }
    }
    for name in ["UTF8_STRING", "text/plain;charset=utf-8", "STRING", "TEXT"] {
        if let Some(target) = offered(name)? {
            if let Some(bytes) = convert(&conn, window, clipboard, target, property)? {
                return Ok(ClipboardContent::Text(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ));
            }
        }
    }
    Ok(ClipboardContent::Empty)
}

/// Ask the owner for `target` into `property` on `window`, and read it (INCR included).
/// None when the owner refuses it.
fn convert(
    conn: &RustConnection,
    window: Window,
    selection: Atom,
    target: Atom,
    property: Atom,
) -> Result<Option<Vec<u8>>> {
    conn.convert_selection(window, selection, target, property, CURRENT_TIME)?;
    conn.flush()?;
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        match next_event(conn, deadline)? {
            Event::SelectionNotify(notify) if notify.requestor == window => {
                if notify.property == x11rb::NONE {
                    return Ok(None);
                }
                break;
            }
            _ => {}
        }
    }
    let incr = conn.intern_atom(false, b"INCR")?.reply()?.atom;
    let reply = conn
        .get_property(true, window, property, AtomEnum::ANY, 0, u32::MAX / 4)?
        .reply()?;
    conn.flush()?;
    if reply.type_ != incr {
        return Ok(Some(reply.value));
    }
    // INCR: the owner writes the data in parts; each part is read (and deleted) on its
    // PropertyNotify NEW_VALUE; an empty part ends it.
    let mut data = Vec::new();
    loop {
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        match next_event(conn, deadline)? {
            Event::PropertyNotify(notify)
                if notify.window == window
                    && notify.atom == property
                    && notify.state == Property::NEW_VALUE =>
            {
                let part = conn
                    .get_property(true, window, property, AtomEnum::ANY, 0, u32::MAX / 4)?
                    .reply()?;
                conn.flush()?;
                if part.value.is_empty() {
                    return Ok(Some(data));
                }
                data.extend_from_slice(&part.value);
                if data.len() > MAX_READ_BYTES {
                    bail!("the clipboard holds more than {MAX_READ_BYTES} bytes");
                }
            }
            _ => {}
        }
    }
}

/// The next event, or an error once `deadline` passes.
fn next_event(conn: &RustConnection, deadline: Instant) -> Result<Event> {
    loop {
        if let Some(event) = conn.poll_for_event()? {
            return Ok(event);
        }
        if Instant::now() >= deadline {
            bail!("the clipboard owner did not answer");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Paths from text/uri-list or x-special/gnome-copied-files ("copy" or "cut" first): file://
/// lines only, comments and the action skipped.
pub(crate) fn paths_of_uri_text(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| line.starts_with("file://"))
        .map(super::path_of_uri)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::paths_of_uri_text;

    #[test]
    fn reads_paths_from_uri_lists_and_gnome_copied_files() {
        assert_eq!(
            paths_of_uri_text("# comment\r\nfile:///home/a/x%20y.txt\r\nfile:///tmp/b.png\r\n"),
            vec!["/home/a/x y.txt", "/tmp/b.png"]
        );
        assert_eq!(
            paths_of_uri_text("copy\nfile:///home/a/T%E1%BA%A3i.pdf"),
            vec!["/home/a/Tải.pdf"]
        );
        assert!(paths_of_uri_text("just text").is_empty());
    }
}
