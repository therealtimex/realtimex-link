//! Remote Control input on X11, through the XTest extension (what xdotool uses): pointer moves,
//! buttons, wheel steps and keysyms, on the primary monitor (the one `capture_display` shows).
//!
//! A keysym the keyboard map lacks (a non-Latin character pressed as a key) is bound for that
//! press to a spare keycode, one with no symbols, and unbound after its release. Text is not
//! typed key by key: `paste_text` owns the CLIPBOARD selection on a second connection and
//! presses Ctrl+V, so an input method (Vietnamese Telex) can't change it. In a terminal it owns
//! PRIMARY too and presses Shift+Insert instead (#2747).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::protocol::xtest::{self, ConnectionExt as _};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use super::super::Button;
use super::terminal::is_terminal_class;

const KEYSYM_CTRL: u32 = 0xffe3;
const KEYSYM_SHIFT: u32 = 0xffe1;
const KEYSYM_V: u32 = 0x76;
const KEYSYM_INSERT: u32 = 0xff63;
const NO_SYMBOL: u32 = 0;
/// Time for the focused app to fetch the selection we just took, and to paste it.
const PASTE_SETTLE: Duration = Duration::from_millis(150);
const OWNER_TIMEOUT: Duration = Duration::from_secs(2);
/// Larger text would need the INCR protocol; agents paste far less.
const MAX_PASTE_BYTES: usize = 256 * 1024;

/// X11 button numbers.
fn x_button(button: Button) -> u8 {
    match button {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
    }
}

/// Wheel steps as button presses: 4 up, 5 down, 6 left, 7 right (positive = down/right).
fn wheel_buttons(dx: i32, dy: i32) -> Vec<u8> {
    let mut buttons = Vec::new();
    let vertical = if dy > 0 { 5 } else { 4 };
    buttons.extend(std::iter::repeat_n(vertical, dy.unsigned_abs() as usize));
    let horizontal = if dx > 0 { 7 } else { 6 };
    buttons.extend(std::iter::repeat_n(horizontal, dx.unsigned_abs() as usize));
    buttons
}

/// The keyboard map: `keysyms[(keycode - min) * per + column]`.
#[derive(Debug, Clone)]
struct Keymap {
    min_keycode: u8,
    per: usize,
    keysyms: Vec<u32>,
}

/// Where a keysym is on the keyboard: its keycode, and whether Shift must be held (it is the
/// key's second column only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyLocation {
    keycode: u8,
    shift: bool,
}

impl Keymap {
    fn keycodes(&self) -> impl Iterator<Item = (u8, &[u32])> {
        self.keysyms
            .chunks(self.per.max(1))
            .enumerate()
            .map(|(index, syms)| (self.min_keycode.saturating_add(index as u8), syms))
    }

    fn locate(&self, keysym: u32) -> Option<KeyLocation> {
        let unshifted = self
            .keycodes()
            .find(|(_, syms)| syms.first() == Some(&keysym))
            .map(|(keycode, _)| KeyLocation {
                keycode,
                shift: false,
            });
        unshifted.or_else(|| {
            self.keycodes()
                .find(|(_, syms)| syms.get(1) == Some(&keysym))
                .map(|(keycode, _)| KeyLocation {
                    keycode,
                    shift: true,
                })
        })
    }

    /// A keycode with no symbols at all, free to bind for a moment.
    fn spare(&self) -> Option<u8> {
        self.keycodes()
            .filter(|(keycode, _)| *keycode > self.min_keycode)
            .find(|(_, syms)| syms.iter().all(|&sym| sym == NO_SYMBOL))
            .map(|(keycode, _)| keycode)
    }

    fn set(&mut self, keycode: u8, syms: &[u32]) {
        let start = (keycode - self.min_keycode) as usize * self.per;
        for (column, sym) in self.keysyms[start..start + self.per].iter_mut().enumerate() {
            *sym = syms.get(column).copied().unwrap_or(NO_SYMBOL);
        }
    }
}

/// How a pressed keysym was sent, so its release undoes exactly that.
#[derive(Debug, Clone, Copy)]
struct Pressed {
    keycode: u8,
    shift: bool,
    remapped: bool,
}

pub struct X11Input {
    conn: RustConnection,
    root: Window,
    /// The primary monitor's top-left corner and size, in root-window pixels.
    origin: (i16, i16),
    size: (u16, u16),
    keymap: Keymap,
    pressed: HashMap<u32, Pressed>,
    clipboard: Option<ClipboardOwner>,
    /// Counts clipboard owner changes, once a viewer asks (#2682).
    watcher: Option<super::x11_clipboard::ClipboardWatcher>,
}

impl X11Input {
    pub fn new() -> Result<Self> {
        if std::env::var_os("DISPLAY").is_none() {
            bail!("no X11 display (DISPLAY is not set)");
        }
        let (conn, screen) = x11rb::connect(None).context("connect to the X11 display")?;
        if conn
            .extension_information(xtest::X11_EXTENSION_NAME)?
            .is_none()
        {
            bail!("XTest extension is not available");
        }
        conn.xtest_get_version(2, 2)?.reply()?;
        let root = conn.setup().roots[screen].root;
        let (origin, size) = primary_monitor(&conn, root)?;
        let keymap = read_keymap(&conn)?;
        Ok(Self {
            conn,
            root,
            origin,
            size,
            keymap,
            pressed: HashMap::new(),
            clipboard: None,
            watcher: None,
        })
    }

    /// The primary monitor in pixels: screenshot pixels map onto it one to one.
    pub fn screen_size(&self) -> Option<(f64, f64)> {
        Some((self.size.0 as f64, self.size.1 as f64))
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn
            .xtest_fake_input(kind, detail, CURRENT_TIME, self.root, x, y, 0)?;
        Ok(())
    }

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        let max_x = self.size.0.saturating_sub(1) as f64;
        let max_y = self.size.1.saturating_sub(1) as f64;
        let x = self.origin.0 + x.round().clamp(0.0, max_x) as i16;
        let y = self.origin.1 + y.round().clamp(0.0, max_y) as i16;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x, y)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        let kind = if pressed {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        self.fake(kind, x_button(button), 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        for button in wheel_buttons(dx, dy) {
            self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0)?;
            self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn key(&self, keycode: u8, pressed: bool) -> Result<()> {
        let kind = if pressed {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        self.fake(kind, keycode, 0, 0)
    }

    pub fn keysym(&mut self, keysym: u32, pressed: bool) -> Result<()> {
        if pressed {
            self.press(keysym)?;
        } else {
            self.release(keysym)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn press(&mut self, keysym: u32) -> Result<()> {
        if self.pressed.contains_key(&keysym) {
            return Ok(());
        }
        let pressed = match self.keymap.locate(keysym) {
            Some(location) => Pressed {
                keycode: location.keycode,
                shift: location.shift,
                remapped: false,
            },
            None => {
                let spare = self
                    .keymap
                    .spare()
                    .ok_or_else(|| anyhow!("no free keycode to type keysym {keysym:#x}"))?;
                self.bind(spare, &[keysym, keysym])?;
                Pressed {
                    keycode: spare,
                    shift: false,
                    remapped: true,
                }
            }
        };
        if pressed.shift {
            if let Some(shift) = self.keymap.locate(KEYSYM_SHIFT) {
                self.key(shift.keycode, true)?;
            }
        }
        self.key(pressed.keycode, true)?;
        self.pressed.insert(keysym, pressed);
        Ok(())
    }

    fn release(&mut self, keysym: u32) -> Result<()> {
        let Some(pressed) = self.pressed.remove(&keysym) else {
            // Not pressed by us: release its usual key anyway, which is harmless.
            if let Some(location) = self.keymap.locate(keysym) {
                self.key(location.keycode, false)?;
            }
            return Ok(());
        };
        self.key(pressed.keycode, false)?;
        if pressed.shift {
            if let Some(shift) = self.keymap.locate(KEYSYM_SHIFT) {
                self.key(shift.keycode, false)?;
            }
        }
        if pressed.remapped {
            // The release must reach the server before the keycode loses its symbol.
            self.conn.sync()?;
            self.bind(pressed.keycode, &[])?;
        }
        Ok(())
    }

    /// Bind (or with no symbols, unbind) one keycode, and wait for the server to apply it.
    fn bind(&mut self, keycode: u8, syms: &[u32]) -> Result<()> {
        let per = self.keymap.per;
        let row: Vec<u32> = (0..per)
            .map(|column| syms.get(column).copied().unwrap_or(NO_SYMBOL))
            .collect();
        self.conn
            .change_keyboard_mapping(1, keycode, per as u8, &row)?;
        self.conn.sync()?;
        self.keymap.set(keycode, &row);
        Ok(())
    }

    pub fn paste_text(&mut self, text: &str, terminal: bool) -> Result<()> {
        if text.len() > MAX_PASTE_BYTES {
            bail!(
                "text is too long to paste ({} bytes, at most {MAX_PASTE_BYTES})",
                text.len()
            );
        }
        self.offer(Clip::Text(text.to_string()))?;
        self.press_paste(terminal)
    }

    /// Put a PNG on the clipboard and press ctrl+v. It goes in one property write, so it must
    /// fit in one X request (about 16 MB with BIG-REQUESTS); larger needs the INCR protocol.
    pub fn paste_image(&mut self, png: &[u8]) -> Result<()> {
        self.offer(Clip::Png(png.to_vec()))?;
        self.press_paste(false)
    }

    /// How many times the clipboard changed hands, and whether this helper holds it now.
    pub fn clipboard_seq(&mut self) -> Result<(u64, bool)> {
        if self.watcher.is_none() {
            self.watcher = Some(super::x11_clipboard::ClipboardWatcher::start()?);
        }
        let seq = self.watcher.as_ref().unwrap().seq();
        let clipboard = self.conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
        let owner = self.conn.get_selection_owner(clipboard)?.reply()?.owner;
        let own = self
            .clipboard
            .as_ref()
            .is_some_and(|ours| ours.alive() && ours.window == owner);
        Ok((seq, own))
    }

    /// What the clipboard holds, read as an app pastes it.
    pub fn clipboard_read(&mut self) -> Result<crate::platform::ClipboardContent> {
        super::x11_clipboard::read_clipboard()
    }

    /// Put these files on the clipboard (as a file manager copies them) and press ctrl+v.
    pub fn paste_files(&mut self, paths: &[String]) -> Result<()> {
        self.offer(Clip::Files(
            paths.iter().map(|p| super::file_uri(p)).collect(),
        ))?;
        self.press_paste(false)
    }

    /// Whether the focused window (the window manager's active window, else the input focus) is
    /// a terminal, by its WM_CLASS.
    fn focused_is_terminal(&self) -> bool {
        let focused = (|| -> Result<Window> {
            let active = self
                .conn
                .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
                .reply()?
                .atom;
            let reply = self
                .conn
                .get_property(false, self.root, active, AtomEnum::WINDOW, 0, 1)?
                .reply()?;
            match reply.value32().and_then(|mut ids| ids.next()) {
                Some(window) if window != 0 => Ok(window),
                _ => Ok(self.conn.get_input_focus()?.reply()?.focus),
            }
        })();
        let Ok(window) = focused else { return false };
        if window <= 1 {
            // None or PointerRoot.
            return false;
        }
        self.conn
            .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 64)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| is_terminal_class(&reply.value))
    }

    fn offer(&mut self, clip: Clip) -> Result<()> {
        if self.clipboard.as_ref().is_none_or(|owner| !owner.alive()) {
            self.clipboard = Some(ClipboardOwner::start()?);
        }
        self.clipboard.as_ref().unwrap().offer(clip)
    }

    /// Ctrl+V, or in a terminal (asked for, or the focused window is one) Shift+Insert with the
    /// content as the PRIMARY selection too (#2747): terminals take Ctrl+V as a control key, and
    /// their Shift+Insert pastes PRIMARY whatever the person bound to paste.
    fn press_paste(&mut self, terminal: bool) -> Result<()> {
        let terminal = terminal || self.focused_is_terminal();
        if terminal {
            self.clipboard
                .as_ref()
                .context("nothing to paste")?
                .offer_primary()?;
        }
        std::thread::sleep(PASTE_SETTLE);
        let keys: &[u32] = if terminal {
            &[KEYSYM_SHIFT, KEYSYM_INSERT]
        } else {
            &[KEYSYM_CTRL, KEYSYM_V]
        };
        for &keysym in keys {
            self.keysym(keysym, true)?;
        }
        for &keysym in keys.iter().rev() {
            self.keysym(keysym, false)?;
        }
        std::thread::sleep(PASTE_SETTLE);
        Ok(())
    }
}

impl Drop for X11Input {
    fn drop(&mut self) {
        // Unbind any spare keycode still holding a symbol.
        let held: Vec<u32> = self.pressed.keys().copied().collect();
        for keysym in held {
            let _ = self.release(keysym);
        }
        let _ = self.conn.flush();
    }
}

fn primary_monitor(conn: &RustConnection, root: Window) -> Result<((i16, i16), (u16, u16))> {
    let geometry = conn.get_geometry(root)?.reply()?;
    let mut area = ((0, 0), (geometry.width, geometry.height));
    if let Ok(monitors) = conn.randr_get_monitors(root, true)?.reply() {
        if let Some(m) = monitors
            .monitors
            .iter()
            .find(|m| m.primary)
            .or(monitors.monitors.first())
        {
            area = ((m.x, m.y), (m.width, m.height));
        }
    }
    Ok(area)
}

fn read_keymap(conn: &RustConnection) -> Result<Keymap> {
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let reply = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
    Ok(Keymap {
        min_keycode: min,
        per: reply.keysyms_per_keycode as usize,
        keysyms: reply.keysyms,
    })
}

/// Clipboard workers running in this process: each holds one X connection.
static CLIPBOARD_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// What the clipboard holds.
#[derive(Clone)]
enum Clip {
    Text(String),
    Png(Vec<u8>),
    /// file:// URIs.
    Files(Vec<String>),
}

/// Owns CLIPBOARD from a hidden window on its own connection; a thread answers paste requests
/// with the latest content until the owner is dropped. Dropping it stops and joins that thread,
/// which closes the connection and frees the content.
struct ClipboardOwner {
    clip: Arc<Mutex<Clip>>,
    conn: Arc<RustConnection>,
    window: Window,
    clipboard: Atom,
    primary: Atom,
    lost: Arc<Mutex<bool>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    /// Signalled by the worker as it exits, so drop waits a bounded time.
    exited: mpsc::Receiver<()>,
}

impl ClipboardOwner {
    fn start() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("connect to the X11 display")?;
        let conn = Arc::new(conn);
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
            &CreateWindowAux::new(),
        )?;
        let atom = |name: &str| -> Result<Atom> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        let atoms = SelectionAtoms {
            clipboard: atom("CLIPBOARD")?,
            primary: AtomEnum::PRIMARY.into(),
            targets: atom("TARGETS")?,
            utf8: atom("UTF8_STRING")?,
            text: atom("TEXT")?,
            png: atom("image/png")?,
            uri_list: atom("text/uri-list")?,
            gnome_files: atom("x-special/gnome-copied-files")?,
        };
        conn.flush()?;
        let clip = Arc::new(Mutex::new(Clip::Text(String::new())));
        let lost = Arc::new(Mutex::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (exited_tx, exited) = mpsc::channel();
        let worker = {
            let (clip, lost, stop) = (Arc::clone(&clip), Arc::clone(&lost), Arc::clone(&stop));
            let thread_conn = Arc::clone(&conn);
            CLIPBOARD_WORKERS.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                serve(&thread_conn, &atoms, &clip, &lost, &stop);
                *lost.lock().unwrap() = true;
                drop(thread_conn);
                CLIPBOARD_WORKERS.fetch_sub(1, Ordering::SeqCst);
                let _ = exited_tx.send(());
            })
        };
        Ok(Self {
            clip,
            conn,
            window,
            clipboard: atoms.clipboard,
            primary: atoms.primary,
            lost,
            stop,
            worker: Some(worker),
            exited,
        })
    }

    fn alive(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }

    /// Hold `clip` as the clipboard, taking the selection (back) if needed.
    fn offer(&self, clip: Clip) -> Result<()> {
        if let Clip::Png(png) = &clip {
            // The property write's own header and the reply event fit in the margin.
            if png.len() + 64 > self.conn.maximum_request_bytes() {
                bail!(
                    "the image is too large for one X11 transfer ({} bytes, at most about {})",
                    png.len(),
                    self.conn.maximum_request_bytes()
                );
            }
        }
        *self.clip.lock().unwrap() = clip;
        *self.lost.lock().unwrap() = false;
        self.conn
            .set_selection_owner(self.window, self.clipboard, CURRENT_TIME)?;
        let (done, wait) = mpsc::channel();
        let conn = Arc::clone(&self.conn);
        let (window, clipboard) = (self.window, self.clipboard);
        std::thread::spawn(move || {
            let owned = conn
                .get_selection_owner(clipboard)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_some_and(|reply| reply.owner == window);
            let _ = done.send(owned);
        });
        if !wait.recv_timeout(OWNER_TIMEOUT).unwrap_or(false) {
            bail!("could not take the X11 clipboard");
        }
        Ok(())
    }

    /// Hold the same content as the PRIMARY selection too, for a terminal's Shift+Insert. Losing
    /// it later (a person selects text) changes nothing for the clipboard.
    fn offer_primary(&self) -> Result<()> {
        self.conn
            .set_selection_owner(self.window, self.primary, CURRENT_TIME)?;
        self.conn.flush()?;
        Ok(())
    }
}

impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // A ClientMessage to our own window wakes the worker blocked in wait_for_event.
        let wake = ClientMessageEvent::new(32, self.window, AtomEnum::NONE, [0u32; 5]);
        let _ = self
            .conn
            .send_event(false, self.window, EventMask::NO_EVENT, wake);
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
        // Bounded: a worker that does not exit in time is left detached rather than hanging
        // the session; it still exits on its next event or when the server goes away.
        if self.exited.recv_timeout(OWNER_TIMEOUT).is_ok() {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

/// The clipboard worker: answer paste requests until asked to stop or the connection fails.
fn serve(
    conn: &RustConnection,
    atoms: &SelectionAtoms,
    clip: &Mutex<Clip>,
    lost: &Mutex<bool>,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) {
        let Ok(event) = conn.wait_for_event() else {
            return;
        };
        match event {
            Event::SelectionRequest(request) => {
                let current = clip.lock().unwrap().clone();
                let _ = answer(conn, atoms, &request, &current);
            }
            Event::SelectionClear(clear) if clear.selection == atoms.clipboard => {
                *lost.lock().unwrap() = true;
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
struct SelectionAtoms {
    clipboard: Atom,
    /// Taken only to paste into a terminal (#2747).
    primary: Atom,
    targets: Atom,
    utf8: Atom,
    text: Atom,
    png: Atom,
    uri_list: Atom,
    gnome_files: Atom,
}

impl SelectionAtoms {
    /// The selections this owner answers for.
    fn serves(&self, selection: Atom) -> bool {
        selection == self.clipboard || selection == self.primary
    }
}

/// Answer one paste request: the list of targets, or the content: text as UTF-8 (STRING as
/// Latin-1), or the PNG as image/png.
fn answer(
    conn: &RustConnection,
    atoms: &SelectionAtoms,
    request: &SelectionRequestEvent,
    clip: &Clip,
) -> Result<()> {
    // Old clients send no property: use the target, as ICCCM says.
    let property = if request.property == u32::from(AtomEnum::NONE) {
        request.target
    } else {
        request.property
    };
    let string = u32::from(AtomEnum::STRING);
    let mut reply_property = property;
    let text = match clip {
        Clip::Text(text) => text.as_str(),
        Clip::Files(uris) => {
            // A file manager reads x-special/gnome-copied-files or text/uri-list; editors the paths.
            let uri_list = uris
                .iter()
                .map(|uri| format!("{uri}\r\n"))
                .collect::<String>();
            let gnome = format!("copy\n{}", uris.join("\n"));
            let paths = uris
                .iter()
                .map(|uri| super::path_of_uri(uri))
                .collect::<Vec<_>>()
                .join("\n");
            if !atoms.serves(request.selection) {
                reply_property = AtomEnum::NONE.into();
            } else if request.target == atoms.targets {
                conn.change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    &[atoms.targets, atoms.gnome_files, atoms.uri_list, atoms.utf8],
                )?;
            } else if request.target == atoms.gnome_files
                || request.target == atoms.uri_list
                || request.target == atoms.utf8
            {
                let (kind, data) = if request.target == atoms.gnome_files {
                    (atoms.gnome_files, gnome.as_bytes())
                } else if request.target == atoms.uri_list {
                    (atoms.uri_list, uri_list.as_bytes())
                } else {
                    (atoms.utf8, paths.as_bytes())
                };
                conn.change_property8(PropMode::REPLACE, request.requestor, property, kind, data)?;
            } else {
                reply_property = AtomEnum::NONE.into();
            }
            return notify(conn, request, reply_property);
        }
        Clip::Png(png) => {
            if !atoms.serves(request.selection) {
                reply_property = AtomEnum::NONE.into();
            } else if request.target == atoms.targets {
                conn.change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    &[atoms.targets, atoms.png],
                )?;
            } else if request.target == atoms.png {
                conn.change_property8(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    atoms.png,
                    png,
                )?;
            } else {
                reply_property = AtomEnum::NONE.into();
            }
            return notify(conn, request, reply_property);
        }
    };
    if !atoms.serves(request.selection) {
        reply_property = AtomEnum::NONE.into();
    } else if request.target == atoms.targets {
        let targets = [atoms.targets, atoms.utf8, atoms.text, string];
        conn.change_property32(
            PropMode::REPLACE,
            request.requestor,
            property,
            AtomEnum::ATOM,
            &targets,
        )?;
    } else if request.target == atoms.utf8 || request.target == atoms.text {
        conn.change_property8(
            PropMode::REPLACE,
            request.requestor,
            property,
            atoms.utf8,
            text.as_bytes(),
        )?;
    } else if request.target == string {
        let latin1: Vec<u8> = text
            .chars()
            .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
            .collect();
        conn.change_property8(
            PropMode::REPLACE,
            request.requestor,
            property,
            AtomEnum::STRING,
            &latin1,
        )?;
    } else {
        reply_property = AtomEnum::NONE.into();
    }
    notify(conn, request, reply_property)
}

/// Tell the requestor where the answer is (or NONE: refused).
fn notify(
    conn: &RustConnection,
    request: &SelectionRequestEvent,
    reply_property: Atom,
) -> Result<()> {
    let notify = SelectionNotifyEvent {
        response_type: xproto::SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: request.selection,
        target: request.target,
        property: reply_property,
    };
    conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify)?;
    conn.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keymap() -> Keymap {
        // Keycodes 8..=12, two columns each: a/A, b/B, NoSymbol, exclam only shifted, NoSymbol.
        Keymap {
            min_keycode: 8,
            per: 2,
            keysyms: vec![
                0x61, 0x41, // 8: a A
                0x62, 0x42, // 9: b B
                0, 0, // 10: free
                0x31, 0x21, // 11: 1 !
                0, 0, // 12: free
            ],
        }
    }

    #[test]
    fn finds_keys_in_the_first_column_before_the_shifted_one() {
        let map = keymap();
        assert_eq!(
            map.locate(0x61),
            Some(KeyLocation {
                keycode: 8,
                shift: false
            })
        );
        assert_eq!(
            map.locate(0x42),
            Some(KeyLocation {
                keycode: 9,
                shift: true
            })
        );
        assert_eq!(
            map.locate(0x21),
            Some(KeyLocation {
                keycode: 11,
                shift: true
            })
        );
        assert_eq!(map.locate(0x01001ec7), None);
    }

    #[test]
    fn binds_an_unmapped_keysym_to_a_spare_keycode_and_frees_it_again() {
        let mut map = keymap();
        let spare = map.spare().unwrap();
        assert_eq!(spare, 10);
        map.set(spare, &[0x01001ec7, 0x01001ec7]);
        assert_eq!(
            map.locate(0x01001ec7),
            Some(KeyLocation {
                keycode: 10,
                shift: false
            })
        );
        assert_eq!(map.spare(), Some(12));
        map.set(spare, &[]);
        assert_eq!(map.locate(0x01001ec7), None);
        assert_eq!(map.spare(), Some(10));
    }

    #[test]
    fn maps_wheel_steps_to_buttons_four_to_seven() {
        assert_eq!(wheel_buttons(0, 2), vec![5, 5]);
        assert_eq!(wheel_buttons(0, -1), vec![4]);
        assert_eq!(wheel_buttons(-2, 1), vec![5, 6, 6]);
        assert_eq!(wheel_buttons(3, 0), vec![7, 7, 7]);
        assert!(wheel_buttons(0, 0).is_empty());
    }

    #[test]
    fn uses_x11_button_numbers() {
        assert_eq!(x_button(Button::Left), 1);
        assert_eq!(x_button(Button::Middle), 2);
        assert_eq!(x_button(Button::Right), 3);
    }

    /// The Xvfb tests share one X server and count this process's threads and sockets.
    static X_SERVER: Mutex<()> = Mutex::new(());

    fn open_sockets() -> Result<usize> {
        let mut count = 0;
        for entry in std::fs::read_dir("/proc/self/fd")? {
            if let Ok(target) = std::fs::read_link(entry?.path()) {
                count += usize::from(target.to_string_lossy().starts_with("socket:"));
            }
        }
        Ok(count)
    }

    fn threads() -> Result<usize> {
        Ok(std::fs::read_dir("/proc/self/task")?.count())
    }

    /// Events a window received, read for up to `wait`.
    fn collect(conn: &RustConnection, wait: Duration) -> Result<Vec<Event>> {
        let deadline = std::time::Instant::now() + wait;
        let mut events = Vec::new();
        while std::time::Instant::now() < deadline {
            while let Some(event) = conn.poll_for_event()? {
                events.push(event);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(events)
    }

    #[test]
    #[ignore = "needs an X server with XTest: xvfb-run cargo test -- --ignored --test-threads=1"]
    fn moves_clicks_types_and_pastes_into_a_window() -> Result<()> {
        let _x = X_SERVER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            300,
            300,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(
                EventMask::BUTTON_PRESS | EventMask::KEY_PRESS | EventMask::KEY_RELEASE,
            ),
        )?;
        conn.map_window(window)?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(300));
        conn.set_input_focus(xproto::InputFocus::POINTER_ROOT, window, CURRENT_TIME)?;
        conn.flush()?;

        let mut input = X11Input::new()?;
        let (origin_x, origin_y) = input.origin;
        input.move_to(50.0, 60.0)?;
        input.button(Button::Left, true)?;
        input.button(Button::Left, false)?;
        input.keysym(0x61, true)?; // a
        input.keysym(0x61, false)?;
        input.keysym(0x01001ec7, true)?; // ệ, not on the keyboard: a spare keycode
        input.keysym(0x01001ec7, false)?;

        let events = collect(&conn, Duration::from_millis(500))?;
        let click = events.iter().find_map(|event| match event {
            Event::ButtonPress(press) if press.detail == 1 => Some((press.event_x, press.event_y)),
            _ => None,
        });
        assert_eq!(click, Some((50 + origin_x, 60 + origin_y)));
        let presses = events
            .iter()
            .filter(|event| matches!(event, Event::KeyPress(_)))
            .count();
        assert_eq!(presses, 2, "a and the remapped ệ");
        // The spare keycode is free again.
        assert_eq!(read_keymap(&conn)?.locate(0x01001ec7), None);

        // Another app reading CLIPBOARD gets the pasted text.
        input.paste_text("xin chào", false)?;
        let atom = |name: &str| -> Result<Atom> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        let (clipboard, utf8, property) =
            (atom("CLIPBOARD")?, atom("UTF8_STRING")?, atom("RTX_TEST")?);
        conn.convert_selection(window, clipboard, utf8, property, CURRENT_TIME)?;
        conn.flush()?;
        let notified = collect(&conn, Duration::from_millis(500))?
            .into_iter()
            .any(|event| matches!(event, Event::SelectionNotify(n) if n.property == property));
        assert!(notified);
        let value = conn
            .get_property(false, window, property, utf8, 0, 1024)?
            .reply()?
            .value;
        assert_eq!(String::from_utf8(value)?, "xin chào");
        drop(input);
        Ok(())
    }

    #[test]
    #[ignore = "needs an X server with XTest: xvfb-run cargo test -- --ignored --test-threads=1"]
    fn dropping_an_input_stops_its_clipboard_worker_and_closes_its_connections() -> Result<()> {
        let _x = X_SERVER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Another client stays connected, as on a desktop: Xvfb resets when its last client leaves.
        let (_desktop, _) = x11rb::connect(None)?;
        let baseline = (
            CLIPBOARD_WORKERS.load(Ordering::SeqCst),
            open_sockets()?,
            threads()?,
        );
        for round in 0..20 {
            let mut input = X11Input::new()?;
            input.paste_text(&format!("round {round}"), false)?;
            assert_eq!(CLIPBOARD_WORKERS.load(Ordering::SeqCst), baseline.0 + 1);
            assert_eq!(
                open_sockets()?,
                baseline.1 + 2,
                "input and clipboard connections"
            );
            drop(input);
            assert_eq!(CLIPBOARD_WORKERS.load(Ordering::SeqCst), baseline.0);
        }
        // offer()'s owner check thread exits just after it answers.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while threads()? > baseline.2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let now = (
            CLIPBOARD_WORKERS.load(Ordering::SeqCst),
            open_sockets()?,
            threads()?,
        );
        eprintln!("workers, sockets, threads: before {baseline:?}, after 20 sessions {now:?}");
        assert_eq!(now, baseline);
        Ok(())
    }
}
