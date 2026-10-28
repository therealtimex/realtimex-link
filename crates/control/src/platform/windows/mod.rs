//! Remote Control input on Windows through SendInput. Pointer positions are pixels of the
//! primary monitor (what `capture_display` shoots), sent as absolute positions on the virtual
//! desktop; keys are virtual keys, or Unicode for characters the layout has no plain key for.
//!
//! Windows drops input aimed at a window of a process with a higher integrity level (an app
//! run as administrator, UAC prompts) and at the secure desktop (lock and sign-in screens).
//! SendInput then inserts fewer events than it was given, and the request fails with the
//! `input-blocked` code.

use std::mem::size_of;
use std::ptr;

use anyhow::{bail, Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardOwner,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, GetSystemMetrics, PostMessageW,
    TranslateMessage, HWND_MESSAGE, MSG, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_QUIT,
};

use rtx_screens::win as screens;

use self::clipboard::Clipboard;
use self::keymap::{named_vk, normalise, physical_press, unshifted_scan, Desktop, Press, Vk};
use crate::codes::PhysicalKey;
use crate::keys::{from_keysym, Key};
use crate::platform::{Button, InputError, Shot};

mod clipboard;
mod keymap;

/// Clipboard format of UTF-16 text (CF_UNICODETEXT).
const CF_UNICODETEXT: u32 = 13;
/// Clipboard format of a file list (CF_HDROP).
const CF_HDROP: u32 = 15;
/// Clipboard format of a device-independent bitmap (CF_DIB).
const CF_DIB: u32 = 8;
/// One wheel notch (WHEEL_DELTA).
const WHEEL_DELTA: i32 = 120;
const VK_CONTROL: u16 = 0x11;
const VK_SHIFT: u16 = 0x10;
const VK_V: u16 = 0x56;
/// How long the pasted text stays on the clipboard before the previous text comes back: the
/// target app reads it when it handles Ctrl+V, after SendInput has returned. The paste doesn't
/// wait for it (typing would take this long per batch): the control loop restores it when due
/// (`restore_due`), and a paste before then moves it on.
const RESTORE_CLIPBOARD_AFTER: std::time::Duration = std::time::Duration::from_millis(400);

/// Nothing to grant on Windows.
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
    bail!("{name} needs no permission on Windows")
}

pub struct Input {
    /// The primary monitor in physical pixels.
    screen: (f64, f64),
    clipboard: WinClipboard,
    /// The clipboard sequence number right after this helper's own last write (a paste, or
    /// putting back what was there): not a copy to send to the viewer (#2682).
    own_seq: Option<u32>,
    /// The last paste's text to put back, and when.
    restore: Option<(clipboard::Saved, std::time::Instant)>,
}

impl Input {
    pub fn new() -> Result<Self> {
        // Physical pixels, the same as the screenshots.
        screens::init();
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if w <= 0 || h <= 0 {
            bail!("no display size");
        }
        Ok(Self {
            screen: (w as f64, h as f64),
            clipboard: WinClipboard::new()?,
            own_seq: None,
            restore: None,
        })
    }

    pub fn screen_size(&self) -> Option<(f64, f64)> {
        Some(self.screen)
    }

    /// Screenshots don't go through input on Windows (see `screenshot_needs_input`).
    pub fn screenshot(&mut self, max_width: u32, _live: bool) -> Result<Shot> {
        screens::capture_display(max_width)
    }

    pub fn frame(&mut self, max_width: u32) -> Result<crate::platform::Frame> {
        screens::capture_display_frame(max_width)
    }

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        // The primary monitor's top-left is (0, 0) on the virtual desktop.
        let (dx, dy) = normalise(virtual_desktop(), x, y);
        send(&[mouse(
            dx,
            dy,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
    }

    pub fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        let flags = match (button, pressed) {
            (Button::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (Button::Left, false) => MOUSEEVENTF_LEFTUP,
            (Button::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (Button::Right, false) => MOUSEEVENTF_RIGHTUP,
            (Button::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (Button::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        };
        // Windows counts double clicks itself, from the timing and position of the presses.
        send(&[mouse(0, 0, 0, flags)])
    }

    pub fn keysym(&mut self, keysym: u32, pressed: bool) -> Result<()> {
        let key = from_keysym(keysym).with_context(|| format!("unsupported keysym {keysym:#x}"))?;
        let up = if pressed {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        if let Some(vk) = virtual_key(key) {
            let flags = if vk.extended {
                up | KEYEVENTF_EXTENDEDKEY
            } else {
                up
            };
            return send(&[keyboard(vk.code, 0, flags)]);
        }
        let Key::Char(c) = key else {
            bail!("{key:?} has no Windows key");
        };
        // No plain key on this layout: type the character itself.
        let mut units = [0u16; 2];
        let events: Vec<INPUT> = c
            .encode_utf16(&mut units)
            .iter()
            .map(|&unit| keyboard(0, unit, up | KEYEVENTF_UNICODE))
            .collect();
        send(&events)
    }

    /// A physical key (#2909), by scancode: this computer's layout and input method decide what
    /// it types. Windows doesn't repeat injected keys, so a repeat is simply another down.
    pub fn key(&mut self, key: &'static PhysicalKey, pressed: bool, _repeat: bool) -> Result<()> {
        let up = if pressed {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        let event = match physical_press(key) {
            Press::Scancode { scan, extended } => {
                let flags = up | KEYEVENTF_SCANCODE;
                keyboard(
                    0,
                    scan,
                    if extended {
                        flags | KEYEVENTF_EXTENDEDKEY
                    } else {
                        flags
                    },
                )
            }
            Press::Vk(vk) => keyboard(
                vk.code,
                0,
                if vk.extended {
                    up | KEYEVENTF_EXTENDEDKEY
                } else {
                    up
                },
            ),
        };
        send(&[event])
    }

    /// Wheel steps; positive scrolls down/right (Windows' vertical wheel points the other way).
    pub fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        let mut events = Vec::new();
        if dy != 0 {
            events.push(mouse(0, 0, -dy * WHEEL_DELTA, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            events.push(mouse(0, 0, dx * WHEEL_DELTA, MOUSEEVENTF_HWHEEL));
        }
        send(&events)
    }

    /// Put the text on the clipboard and press Ctrl+V (Ctrl+Shift+V in terminals, which
    /// Windows Terminal and most others accept). The previous text comes back later
    /// (`restore_clipboard`).
    pub fn paste_text(&mut self, text: &str, terminal: bool) -> Result<()> {
        let pending = self.restore.take().map(|(saved, _)| saved);
        let saved = clipboard::put_text(&mut self.clipboard, text, pending)?;
        self.note_own_write();
        let sent = press_paste(terminal);
        if saved.previous.is_some() {
            self.restore = Some((saved, std::time::Instant::now() + RESTORE_CLIPBOARD_AFTER));
        }
        sent
    }

    /// When the last paste's previous text is due back.
    pub fn restore_due(&self) -> Option<std::time::Instant> {
        self.restore.as_ref().map(|(_, due)| *due)
    }

    /// Put the last paste's previous text back now. Kept as it is when newer content arrived
    /// meanwhile.
    pub fn restore_clipboard(&mut self) {
        if let Some((saved, _)) = self.restore.take() {
            if clipboard::restore(&mut self.clipboard, &saved).unwrap_or(false) {
                self.note_own_write();
            }
        }
    }

    fn note_own_write(&mut self) {
        self.own_seq = Some(unsafe { GetClipboardSequenceNumber() });
    }

    /// The clipboard's sequence number, and whether its content is this helper's own write.
    pub fn clipboard_seq(&mut self) -> Result<(u64, bool)> {
        let seq = unsafe { GetClipboardSequenceNumber() };
        Ok((u64::from(seq), self.own_seq == Some(seq)))
    }

    /// What the clipboard holds: files (CF_HDROP), an image (PNG, or a bitmap as BMP) or text.
    pub fn clipboard_read(&mut self) -> Result<crate::platform::ClipboardContent> {
        self.clipboard.open()?;
        let content = read_open_clipboard(&mut self.clipboard);
        self.clipboard.close();
        Ok(content)
    }

    /// Put these files on the clipboard as Explorer copies them (CF_HDROP, "copy") and press
    /// Ctrl+V: a folder copies them in, an app such as RealTimeX attaches them.
    pub fn paste_files(&mut self, paths: &[String]) -> Result<()> {
        // They stay on the clipboard, as after a local paste.
        self.restore = None;
        self.clipboard.open()?;
        let put = self.clipboard.replace_files(paths);
        self.clipboard.close();
        put?;
        self.note_own_write();
        press_paste(false)
    }

    /// Put a PNG on the clipboard (the registered "PNG" format) with the same picture as a
    /// bitmap (CF_DIB, made by the host), and press Ctrl+V. Chromium, Paint and Office paste an
    /// image from CF_DIB; PNG alone pasted nothing in RealTimeX (#2747). It stays on the
    /// clipboard, as after a local paste.
    pub fn paste_image(&mut self, png: &[u8], dib: Option<&[u8]>) -> Result<()> {
        self.restore = None;
        self.clipboard.open()?;
        let put = self.clipboard.replace_png(png, dib);
        self.clipboard.close();
        put?;
        self.note_own_write();
        press_paste(false)
    }
}

impl Drop for Input {
    /// The session ends: the last paste's previous text still comes back, once the app had its
    /// time to read the paste.
    fn drop(&mut self) {
        if let Some(due) = self.restore_due() {
            std::thread::sleep(due.saturating_duration_since(std::time::Instant::now()));
            self.restore_clipboard();
        }
    }
}

/// Ctrl+V, or Ctrl+Shift+V in terminals.
fn press_paste(terminal: bool) -> Result<()> {
    let mut keys = vec![VK_CONTROL];
    if terminal {
        keys.push(VK_SHIFT);
    }
    keys.push(VK_V);
    let mut events: Vec<INPUT> = keys
        .iter()
        .map(|&code| keyboard(code, 0, KEYBD_EVENT_FLAGS(0)))
        .collect();
    events.extend(
        keys.iter()
            .rev()
            .map(|&code| keyboard(code, 0, KEYEVENTF_KEYUP)),
    );
    send(&events)
}

/// The virtual key for a key: fixed ones first, then the current layout's plain key.
fn virtual_key(key: Key) -> Option<Vk> {
    named_vk(key).or_else(|| match key {
        Key::Char(c) if (c as u32) <= 0xffff => unshifted_scan(unsafe { VkKeyScanW(c as u16) }),
        _ => None,
    })
}

fn virtual_desktop() -> Desktop {
    unsafe {
        Desktop {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn keyboard(code: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(code),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Send the events in one batch, so nothing else is interleaved with them.
fn send(events: &[INPUT]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let inserted = unsafe { SendInput(events, size_of::<INPUT>() as i32) };
    if (inserted as usize) < events.len() {
        return Err(InputError {
            code: "input-blocked",
            message: "Windows blocked the input. The window in front may be running as \
                      administrator, or a UAC prompt, the lock screen or the sign-in screen is \
                      showing; Remote Control can't use those. Ask the person at the computer to \
                      close it or switch to another window."
                .into(),
        }
        .into());
    }
    Ok(())
}

/// The Windows clipboard, owned by a message-only window: SetClipboardData fails for a
/// clipboard opened without an owner.
struct WinClipboard {
    owner: HWND,
    open: bool,
}

impl WinClipboard {
    /// The owner window lives on its own thread, which answers its messages (#2747): Explorer
    /// and Chromium ask the clipboard's owner before they take files or an image, and treat a
    /// clipboard whose owner never answers as empty; another app's EmptyClipboard sends it
    /// WM_DESTROYCLIPBOARD and waits. This thread is busy with requests and pumps nothing.
    fn new() -> Result<Self> {
        let (made, owner) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("clipboard-owner".into())
            .spawn(move || {
                let owner = unsafe {
                    CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        w!("STATIC"),
                        w!("RealTimeX Remote Control clipboard"),
                        WINDOW_STYLE(0),
                        0,
                        0,
                        0,
                        0,
                        Some(HWND_MESSAGE),
                        None,
                        None,
                        None,
                    )
                };
                // A window handle is not Send; its value is.
                let window = owner.as_ref().map(|hwnd| hwnd.0 as isize);
                let _ = made.send(window.map_err(|error| anyhow::anyhow!("{error}")));
                let Ok(owner) = owner else { return };
                let mut message = MSG::default();
                // Until Drop posts WM_QUIT.
                while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
                    unsafe {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                // What it put on the clipboard stays there: it was copied, not rendered late.
                let _ = unsafe { DestroyWindow(owner) };
            })
            .context("starting the clipboard owner thread")?;
        let window = owner
            .recv()
            .context("the clipboard owner thread stopped")?
            .context("creating the clipboard owner window")?;
        Ok(Self {
            owner: HWND(window as *mut _),
            open: false,
        })
    }
}

impl Clipboard for WinClipboard {
    fn open(&mut self) -> Result<()> {
        // Another app may hold it for a moment.
        for _ in 0..10 {
            if unsafe { OpenClipboard(Some(self.owner)) }.is_ok() {
                self.open = true;
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        bail!("could not open the clipboard")
    }

    fn close(&mut self) {
        if std::mem::take(&mut self.open) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    fn text(&mut self) -> Option<String> {
        unsafe {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let memory = HGLOBAL(handle.0);
            let data = GlobalLock(memory) as *const u16;
            if data.is_null() {
                return None;
            }
            let units = GlobalSize(memory) / 2;
            let len = (0..units).take_while(|&i| *data.add(i) != 0).count();
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(data, len));
            let _ = GlobalUnlock(memory);
            Some(text)
        }
    }

    fn replace(&mut self, text: &str) -> Result<()> {
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2)?;
            let data = GlobalLock(memory) as *mut u16;
            if data.is_null() {
                let _ = GlobalFree(Some(memory));
                bail!("could not lock clipboard memory");
            }
            ptr::copy_nonoverlapping(units.as_ptr(), data, units.len());
            let _ = GlobalUnlock(memory);
            if let Err(error) = EmptyClipboard() {
                let _ = GlobalFree(Some(memory));
                return Err(error).context("emptying the clipboard");
            }
            // The clipboard owns the memory once this succeeds.
            if let Err(error) = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))) {
                let _ = GlobalFree(Some(memory));
                return Err(error).context("setting the clipboard");
            }
        }
        Ok(())
    }

    fn sequence(&mut self) -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    fn owned(&mut self) -> bool {
        unsafe { GetClipboardOwner() }.is_ok_and(|owner| owner == self.owner)
    }
}

impl WinClipboard {
    /// Empty the open clipboard and put these files on it: CF_HDROP (a DROPFILES header, then
    /// the paths as UTF-16, each ending in a NUL, and one more NUL) and Preferred DropEffect = copy.
    fn replace_files(&mut self, paths: &[String]) -> Result<()> {
        const DROPFILES_SIZE: usize = 20;
        const DROPEFFECT_COPY: u32 = 1;
        let mut names: Vec<u16> = Vec::new();
        for path in paths {
            names.extend(path.encode_utf16());
            names.push(0);
        }
        names.push(0);
        let mut hdrop = vec![0u8; DROPFILES_SIZE];
        hdrop[0..4].copy_from_slice(&(DROPFILES_SIZE as u32).to_le_bytes()); // pFiles
        hdrop[16..20].copy_from_slice(&1u32.to_le_bytes()); // fWide
        hdrop.extend(names.iter().flat_map(|unit| unit.to_le_bytes()));
        let effect = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) };
        unsafe {
            EmptyClipboard().context("emptying the clipboard")?;
        }
        set_clipboard_bytes(CF_HDROP, &hdrop)?;
        if effect != 0 {
            set_clipboard_bytes(effect, &DROPEFFECT_COPY.to_le_bytes())?;
        }
        Ok(())
    }

    /// Empty the open clipboard and put these PNG bytes on it, and the bitmap when there is one.
    fn replace_png(&mut self, png: &[u8], dib: Option<&[u8]>) -> Result<()> {
        let format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
        if format == 0 {
            bail!("could not register the PNG clipboard format");
        }
        unsafe {
            EmptyClipboard().context("emptying the clipboard")?;
        }
        if let Some(dib) = dib {
            set_clipboard_bytes(CF_DIB, dib)?;
        }
        set_clipboard_bytes(format, png)
    }
}

/// The bytes of one clipboard format, if it is there (the clipboard is open).
fn clipboard_bytes(format: u32) -> Option<Vec<u8>> {
    unsafe {
        if IsClipboardFormatAvailable(format).is_err() {
            return None;
        }
        let handle = GetClipboardData(format).ok()?;
        let memory = HGLOBAL(handle.0);
        let data = GlobalLock(memory) as *const u8;
        if data.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(data, GlobalSize(memory)).to_vec();
        let _ = GlobalUnlock(memory);
        Some(bytes)
    }
}

/// Files, then an image, then text, from the open clipboard.
fn read_open_clipboard(clipboard: &mut WinClipboard) -> crate::platform::ClipboardContent {
    use crate::platform::ClipboardContent;
    if let Some(hdrop) = clipboard_bytes(CF_HDROP) {
        let paths = paths_of_hdrop(&hdrop);
        if !paths.is_empty() {
            return ClipboardContent::Files(paths);
        }
    }
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    if let Some(png) = (png_format != 0)
        .then(|| clipboard_bytes(png_format))
        .flatten()
    {
        return ClipboardContent::Image {
            bytes: png,
            mime: "image/png",
        };
    }
    if let Some(dib) = clipboard_bytes(CF_DIB) {
        if let Some(bmp) = bmp_of_dib(&dib) {
            return ClipboardContent::Image {
                bytes: bmp,
                mime: "image/bmp",
            };
        }
    }
    match clipboard.text() {
        Some(text) => ClipboardContent::Text(text),
        None => ClipboardContent::Empty,
    }
}

/// The paths in a CF_HDROP (DROPFILES): wide or ANSI names, each ending in NUL, then a NUL.
fn paths_of_hdrop(hdrop: &[u8]) -> Vec<String> {
    if hdrop.len() < 20 {
        return Vec::new();
    }
    let start = u32::from_le_bytes(hdrop[0..4].try_into().unwrap()) as usize;
    let wide = u32::from_le_bytes(hdrop[16..20].try_into().unwrap()) != 0;
    let list = hdrop.get(start..).unwrap_or_default();
    if wide {
        let units: Vec<u16> = list
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        units
            .split(|&unit| unit == 0)
            .take_while(|name| !name.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    } else {
        list.split(|&byte| byte == 0)
            .take_while(|name| !name.is_empty())
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect()
    }
}

/// A CF_DIB as a .bmp file: the 14-byte file header in front, pointing past the info header,
/// the colour masks (BI_BITFIELDS) and the palette.
fn bmp_of_dib(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let u32_at = |at: usize| u32::from_le_bytes(dib[at..at + 4].try_into().unwrap());
    let header = u32_at(0) as usize;
    let bit_count = u16::from_le_bytes(dib[14..16].try_into().unwrap());
    let compression = u32_at(16);
    let colors_used = u32_at(32) as usize;
    let masks = if compression == 3 && header == 40 {
        12
    } else {
        0
    };
    let palette = if colors_used > 0 {
        colors_used
    } else if bit_count <= 8 {
        1 << bit_count
    } else {
        0
    };
    let offset = 14 + header + masks + palette * 4;
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0, 0, 0, 0]);
    bmp.extend_from_slice(&(offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    Some(bmp)
}

/// Copy bytes into movable global memory and hand them to the open clipboard as `format`.
fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> Result<()> {
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let data = GlobalLock(memory) as *mut u8;
        if data.is_null() {
            let _ = GlobalFree(Some(memory));
            bail!("could not lock clipboard memory");
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        let _ = GlobalUnlock(memory);
        // The clipboard owns the memory once this succeeds.
        if let Err(error) = SetClipboardData(format, Some(HANDLE(memory.0))) {
            let _ = GlobalFree(Some(memory));
            return Err(error).context("setting the clipboard");
        }
    }
    Ok(())
}

impl Drop for WinClipboard {
    fn drop(&mut self) {
        self.close();
        // Its thread ends its loop and destroys the window (only the owning thread can).
        let _ = unsafe { PostMessageW(Some(self.owner), WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}
