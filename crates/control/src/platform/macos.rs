//! Remote Control input on macOS: CGEvents posted at the HID level, on the main display.
//! Posting needs the Accessibility permission (System Settings → Privacy & Security →
//! Accessibility), credited to the app that launched rtx-control.
//!
//! Every event comes from one private event source (`kCGEventSourceStatePrivate`), the kind Apple
//! describes for remote control: its key and button state is only what this session pressed, so
//! the events never take on modifiers the person at this Mac is holding, and nothing here reads
//! the keyboard's state (#2665).

use std::ffi::c_void;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::codes::{Modifier, PhysicalKey};
use crate::jobinput::{Event, MouseKind};
use crate::keys::{from_keysym, Key};
use crate::platform::{Button, ClipboardContent, PasteGap};

/// The login-window job's input (realtimex-link#11, Addendum 5).
#[path = "macos_job.rs"]
pub mod job;
/// The lock screen's picture (realtimex-link#11, Addendum 8).
#[path = "macos_locked.rs"]
mod locked;

/// `display` (None: the main display), at most `max_width` wide: ScreenCaptureKit, or
/// CGDisplayCreateImage while the session is locked (ScreenCaptureKit's picture is blank then;
/// S2b).
pub fn capture_display_on(display: Option<&str>, max_width: u32) -> Result<super::Shot> {
    if locked::screen_locked() {
        locked::screenshot(display, max_width)
    } else {
        rtx_screens::capture_display_on(display, max_width)
    }
}

/// A live-view frame, the same way.
pub fn capture_display_frame_on(display: Option<&str>, max_width: u32) -> Result<super::Frame> {
    if locked::screen_locked() {
        locked::frame(display, max_width)
    } else {
        rtx_screens::capture_display_frame_on(display, max_width)
    }
}

/// The displays (CoreGraphics only: also at the lock screen and the login window), each with
/// its bounds in global points, where posted mouse events land (realtimex-link#12).
pub fn displays() -> Result<Vec<crate::displays::Display>> {
    Ok(rtx_screens::displays()?
        .into_iter()
        .map(|info| {
            let bounds = unsafe { CGDisplayBounds(info.id.parse().unwrap_or(0)) };
            crate::displays::Display {
                rect: crate::displays::Rect {
                    x: bounds.origin.x,
                    y: bounds.origin.y,
                    width: bounds.size.width,
                    height: bounds.size.height,
                },
                info,
            }
        })
        .collect())
}

type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        kind: u32,
        point: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, keycode: u16, down: bool)
        -> CGEventRef;
    fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventKeyboardSetUnicodeString(event: CGEventRef, length: usize, string: *const u16);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: CGEventRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *const c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
}

const SETTINGS_SCREEN_RECORDING: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
const SETTINGS_ACCESSIBILITY: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

extern "C" {
    // libsystem: the app macOS credits this process's permissions to.
    fn responsibility_get_pid_responsible_for_pid(pid: i32) -> i32;
    fn proc_pidpath(pid: i32, buffer: *mut u8, size: u32) -> i32;
}

/// The app these permissions belong to in System Settings: the installed RealTimeX, or in
/// development the terminal that started the dev server.
pub fn permission_owner() -> Option<String> {
    let pid = unsafe { responsibility_get_pid_responsible_for_pid(std::process::id() as i32) };
    let mut buffer = vec![0u8; 4096];
    let length = unsafe { proc_pidpath(pid, buffer.as_mut_ptr(), buffer.len() as u32) };
    if length <= 0 {
        return None;
    }
    app_name(&String::from_utf8_lossy(&buffer[..length as usize]))
}

/// "/Applications/RealTimeX.AI.app/Contents/MacOS/RealTimeX.AI" -> "RealTimeX.AI"; the
/// outermost bundle wins, so a helper inside an app reports the app.
fn app_name(path: &str) -> Option<String> {
    path.split('/')
        .find_map(|part| part.strip_suffix(".app"))
        .map(str::to_string)
        .or_else(|| {
            path.rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
}

/// macOS screenshots need Screen Recording only, not the input session.
pub fn screenshot_needs_input() -> bool {
    false
}

/// Screen Recording and Accessibility, both needed to be controlled. Checking never prompts.
pub fn control_permissions() -> (Option<bool>, Option<bool>) {
    unsafe {
        (
            Some(CGPreflightScreenCaptureAccess()),
            Some(AXIsProcessTrusted()),
        )
    }
}

/// Ask macOS for one permission: its own prompt adds the app to the list in System Settings,
/// and the matching Privacy pane opens so the user can turn it on.
pub fn request_control_permission(name: &str) -> Result<()> {
    let pane = match name {
        "screenRecording" => {
            unsafe { CGRequestScreenCaptureAccess() };
            SETTINGS_SCREEN_RECORDING
        }
        "accessibility" => {
            unsafe {
                let keys = [kAXTrustedCheckOptionPrompt];
                let values = [kCFBooleanTrue];
                let options = CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    1,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                );
                AXIsProcessTrustedWithOptions(options);
                if !options.is_null() {
                    CFRelease(options);
                }
            }
            SETTINGS_ACCESSIBILITY
        }
        other => bail!("Unknown permission: {other}"),
    };
    Command::new("/usr/bin/open")
        .arg(pane)
        .status()
        .context("open System Settings")?;
    Ok(())
}

/// kCGEventSourceStatePrivate: a source with its own state, apart from the hardware's.
const EVENT_SOURCE_STATE_PRIVATE: i32 = -1;
/// kCGEventSourceStateHIDSystemState: the hardware's state.
const EVENT_SOURCE_STATE_HID_SYSTEM: i32 = 1;
const HID_EVENT_TAP: u32 = 0;
const MOUSE_MOVED: u32 = 5;
const SCROLL_UNITS_LINE: u32 = 1;
/// kCGMouseEventClickState: 2 makes a second click a double click.
const FIELD_CLICK_STATE: u32 = 1;
/// kCGKeyboardEventAutorepeat: 1 on a held key's repeated down. macOS doesn't repeat posted
/// keys itself, so the viewer's repeats are posted with it.
const FIELD_KEYBOARD_AUTOREPEAT: u32 = 8;
/// How long the pasted text stays on the pasteboard before the previous text comes back: the
/// app reads it when it handles ⌘V, after the events are posted. The paste doesn't wait for it;
/// the control loop puts it back when due (`restore_due`), and a paste before then moves it on.
const RESTORE_CLIPBOARD_AFTER: Duration = Duration::from_millis(400);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALT: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;

const KEYCODE_V: u16 = 0x09;
const KEYCODE_COMMAND: u16 = 0x37;

/// How long one CG input call may take on the worker (realtimex-link#11 I1).
const POST_LIMIT: Duration = Duration::from_secs(2);

type Work = Box<dyn FnOnce() -> Result<()> + Send>;

/// Run one CG input call on the worker: making the event, setting its fields and posting it,
/// waiting at most [`POST_LIMIT`] (I1, Addendum 6). Under Secure Input (the login window)
/// WindowServer may never answer, already when the event is made; the request then fails with
/// `input-unavailable` instead of holding the loop.
fn on_worker(work: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    use std::sync::mpsc;
    use std::sync::{Mutex, OnceLock};
    static WORKER: OnceLock<Mutex<mpsc::Sender<(Work, mpsc::SyncSender<Result<()>>)>>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (send, receive) = mpsc::channel::<(Work, mpsc::SyncSender<Result<()>>)>();
        std::thread::spawn(move || {
            for (work, done) in receive {
                let _ = done.send(work());
            }
        });
        Mutex::new(send)
    });
    let (done, finished) = mpsc::sync_channel(1);
    let sent = worker
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .send((Box::new(work), done));
    match (sent, finished.recv_timeout(POST_LIMIT)) {
        (Ok(()), Ok(result)) => result,
        _ => Err(super::InputError {
            code: "input-unavailable",
            message: "Input isn't getting through on this screen".into(),
        }
        .into()),
    }
}

/// How long one capture may take (realtimex-link#11 C5), and how soon after one that timed out
/// the next is tried. 10 s, not 5: a cold first screenshot (window list, picture, text) took
/// longer than 5 s on office-mac-mini-01, and ScreenCaptureKit's own calls already stop at 5 s
/// each (rtx-screens' gate).
const CAPTURE_LIMIT: Duration = Duration::from_secs(10);
const CAPTURE_RETRY: Duration = Duration::from_secs(2);

type CaptureWork = Box<dyn FnOnce() + Send>;

/// Run one capture (a screenshot, a stream's start or frame) on the capture worker, waiting at
/// most [`CAPTURE_LIMIT`] (C5, Addendum 8). At a locked screen a capture may not answer: the
/// request then answers `capture-unavailable` and the helper stays (input still works there).
/// Only one capture is in flight, so a stuck one leaves no thread per attempt behind, and after
/// a timeout the next try waits [`CAPTURE_RETRY`].
pub fn capture_bounded<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, OnceLock};
    static WORKER: OnceLock<Mutex<mpsc::Sender<CaptureWork>>> = OnceLock::new();
    static BUSY: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    static LAST_TIMEOUT: Mutex<Option<Instant>> = Mutex::new(None);
    let unavailable = || -> anyhow::Error {
        super::InputError {
            code: "capture-unavailable",
            message: "The screen can't be captured right now".into(),
        }
        .into()
    };
    if LAST_TIMEOUT
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_some_and(|at| at.elapsed() < CAPTURE_RETRY)
    {
        return Err(unavailable());
    }
    let busy = BUSY.get_or_init(|| Arc::new(AtomicBool::new(false))).clone();
    if busy.swap(true, Ordering::SeqCst) {
        return Err(unavailable());
    }
    let worker = WORKER.get_or_init(|| {
        let (send, receive) = mpsc::channel::<CaptureWork>();
        std::thread::spawn(move || {
            for work in receive {
                work();
            }
        });
        Mutex::new(send)
    });
    let (done, answer) = mpsc::sync_channel(1);
    let finished = busy.clone();
    let job: CaptureWork = Box::new(move || {
        let _ = done.send(work());
        finished.store(false, Ordering::SeqCst);
    });
    if worker.lock().unwrap_or_else(|p| p.into_inner()).send(job).is_err() {
        busy.store(false, Ordering::SeqCst);
        return Err(unavailable());
    }
    match answer.recv_timeout(CAPTURE_LIMIT) {
        Ok(result) => result,
        Err(_) => {
            *LAST_TIMEOUT.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
            Err(unavailable())
        }
    }
}

/// Post an event the worker just made, and release it. Only on the worker.
fn post(event: CGEventRef) -> Result<()> {
    if event.is_null() {
        bail!("could not create the input event");
    }
    // SAFETY: an event made for this post, released once posted.
    unsafe {
        CGEventPost(HID_EVENT_TAP, event);
        CFRelease(event);
    }
    Ok(())
}

/// US-layout virtual key codes (kVK_*). A character's is its unshifted key's (see `char_key`);
/// characters with no key are typed by their Unicode value.
fn keycode(key: Key) -> Option<u16> {
    Some(match key {
        Key::Return => 0x24,
        Key::Tab => 0x30,
        Key::Escape => 0x35,
        Key::Backspace => 0x33,
        Key::Delete => 0x75,
        Key::Insert => 0x72,
        Key::Home => 0x73,
        Key::End => 0x77,
        Key::PageUp => 0x74,
        Key::PageDown => 0x79,
        Key::Left => 0x7b,
        Key::Right => 0x7c,
        Key::Down => 0x7d,
        Key::Up => 0x7e,
        Key::Control => 0x3b,
        Key::Shift => 0x38,
        Key::Alt => 0x3a,
        Key::Super => KEYCODE_COMMAND,
        Key::Menu | Key::PrintScreen => return None,
        Key::F(n) => [
            0x7a, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6d, 0x67, 0x6f,
        ]
        .get(n.checked_sub(1)? as usize)
        .copied()?,
        Key::Char(c) => match c {
            'a' => 0x00,
            's' => 0x01,
            'd' => 0x02,
            'f' => 0x03,
            'h' => 0x04,
            'g' => 0x05,
            'z' => 0x06,
            'x' => 0x07,
            'c' => 0x08,
            'v' => 0x09,
            'b' => 0x0b,
            'q' => 0x0c,
            'w' => 0x0d,
            'e' => 0x0e,
            'r' => 0x0f,
            'y' => 0x10,
            't' => 0x11,
            '1' => 0x12,
            '2' => 0x13,
            '3' => 0x14,
            '4' => 0x15,
            '6' => 0x16,
            '5' => 0x17,
            '=' => 0x18,
            '9' => 0x19,
            '7' => 0x1a,
            '-' => 0x1b,
            '8' => 0x1c,
            '0' => 0x1d,
            ']' => 0x1e,
            'o' => 0x1f,
            'u' => 0x20,
            '[' => 0x21,
            'i' => 0x22,
            'p' => 0x23,
            'l' => 0x25,
            'j' => 0x26,
            '\'' => 0x27,
            'k' => 0x28,
            ';' => 0x29,
            '\\' => 0x2a,
            ',' => 0x2b,
            '/' => 0x2c,
            'n' => 0x2d,
            'm' => 0x2e,
            '.' => 0x2f,
            '`' => 0x32,
            ' ' => 0x31,
            _ => return None,
        },
    })
}

/// The key that types this character on a US layout, and whether it needs Shift: 'I' is the
/// i key with Shift. Apps such as TextEdit read the case from the key and the flags, not only
/// from the event's Unicode string.
fn char_key(c: char) -> Option<(u16, bool)> {
    let (base, shift) = match c {
        'A'..='Z' => (c.to_ascii_lowercase(), true),
        '~' => ('`', true),
        '!' => ('1', true),
        '@' => ('2', true),
        '#' => ('3', true),
        '$' => ('4', true),
        '%' => ('5', true),
        '^' => ('6', true),
        '&' => ('7', true),
        '*' => ('8', true),
        '(' => ('9', true),
        ')' => ('0', true),
        '_' => ('-', true),
        '+' => ('=', true),
        '{' => ('[', true),
        '}' => (']', true),
        '|' => ('\\', true),
        ':' => (';', true),
        '"' => ('\'', true),
        '<' => (',', true),
        '>' => ('.', true),
        '?' => ('/', true),
        _ => (c, false),
    };
    Some((keycode(Key::Char(base))?, shift))
}

fn physical_flag(modifier: Modifier) -> u64 {
    match modifier {
        Modifier::Shift => FLAG_SHIFT,
        Modifier::Control => FLAG_CONTROL,
        Modifier::Alt => FLAG_ALT,
        Modifier::Meta => FLAG_COMMAND,
    }
}

fn modifier_flag(key: Key) -> u64 {
    match key {
        Key::Shift => FLAG_SHIFT,
        Key::Control => FLAG_CONTROL,
        Key::Alt => FLAG_ALT,
        Key::Super => FLAG_COMMAND,
        _ => 0,
    }
}

pub struct Input {
    /// The private event source every event is created from; released on drop.
    source: CGEventSourceRef,
    bounds: CGRect,
    position: CGPoint,
    buttons_down: [bool; 3],
    /// Modifiers held through `keysym`.
    keysym_flags: u64,
    /// Physical modifier keys held (#2909): every posted key and mouse event carries their
    /// flags, so Shift+click and ⌘+click arrive as such.
    physical_modifiers: Vec<&'static PhysicalKey>,
    /// This request goes to the login-window job, not CGEventPost (the daemon's `via:job`, C4).
    via_job: bool,
    /// The job was sent something: it releases everything when this session ends.
    used_job: bool,
    last_click: Option<(Button, Instant)>,
    click_state: i64,
    /// The pasteboard's changeCount right after this helper's own last write: not a copy to
    /// send back to the viewer (#2682).
    own_change: Option<isize>,
    /// The text the last paste took off the pasteboard, and when it goes back.
    restore: Option<(String, Instant)>,
    gap: PasteGap,
}

impl Input {
    pub fn new() -> Result<Self> {
        if !unsafe { AXIsProcessTrusted() } {
            bail!(
                "Remote Control needs the Accessibility permission: System Settings → Privacy & \
                 Security → Accessibility → turn on RealTimeX (or the terminal running it)"
            );
        }
        // At the login window (realtimex-link#11, as root under launchd) a private source can't
        // be made: the HID system's state is used there, and with neither, events go without a
        // source (CGEventCreate*Event takes NULL).
        let source = [EVENT_SOURCE_STATE_PRIVATE, EVENT_SOURCE_STATE_HID_SYSTEM]
            .into_iter()
            .map(|state| unsafe { CGEventSourceCreate(state) })
            .find(|source| !source.is_null())
            .unwrap_or(std::ptr::null_mut());
        let bounds = unsafe { CGDisplayBounds(CGMainDisplayID()) };
        Ok(Self {
            source,
            bounds,
            position: CGPoint {
                x: bounds.origin.x,
                y: bounds.origin.y,
            },
            buttons_down: [false; 3],
            keysym_flags: 0,
            physical_modifiers: Vec::new(),
            last_click: None,
            own_change: None,
            restore: None,
            gap: PasteGap::default(),
            click_state: 1,
            via_job: false,
            used_job: false,
        })
    }

    /// Where the next input request posts: the login-window job (`via:job`) or CGEventPost.
    pub fn route(&mut self, via_job: bool) {
        self.via_job = via_job;
        self.used_job |= via_job;
    }

    fn job_mouse(&self, kind: MouseKind, button: u32) -> Result<()> {
        job::send(&[Event::Mouse {
            x: self.position.x as i32,
            y: self.position.y as i32,
            kind,
            button: button.min(2) as u8,
            click_state: self.click_state.clamp(0, 3) as u8,
        }])
    }

    /// The flags of every modifier held, by keysym or physically.
    fn flags(&self) -> u64 {
        self.physical_modifiers
            .iter()
            .filter_map(|key| key.modifier)
            .fold(self.keysym_flags, |flags, modifier| {
                flags | physical_flag(modifier)
            })
    }

    /// The main display in points; screenshot pixels are scaled onto it.
    pub fn screen_size(&self) -> Option<(f64, f64)> {
        Some((self.bounds.size.width, self.bounds.size.height))
    }

    /// The displays come from the platform's list, not through input (only GNOME Wayland's do).
    pub fn displays(&mut self) -> Result<Vec<crate::displays::Display>> {
        displays()
    }

    /// Screenshots don't go through input on macOS (see `screenshot_needs_input`).
    pub fn screenshot(
        &mut self,
        max_width: u32,
        _live: bool,
        display: Option<&str>,
    ) -> Result<crate::platform::Shot> {
        let display = display.map(str::to_string);
        capture_bounded(move || capture_display_on(display.as_deref(), max_width))
    }

    pub fn frame(&mut self, max_width: u32, display: Option<&str>) -> Result<crate::platform::Frame> {
        let display = display.map(str::to_string);
        capture_bounded(move || capture_display_frame_on(display.as_deref(), max_width))
    }

    /// The pointer to (x, y) in global points, on `_display` (realtimex-link#12: the caller
    /// mapped the point onto that display's bounds).
    pub fn move_to(&mut self, _display: &crate::displays::Display, x: f64, y: f64) -> Result<()> {
        self.position = CGPoint { x, y };
        // While a button is held, macOS expects dragged events instead of moves.
        let (kind, button) = match self.buttons_down {
            [true, _, _] => (6, 0),
            [_, true, _] => (7, 1),
            [_, _, true] => (27, 2),
            _ => (MOUSE_MOVED, 0),
        };
        if self.via_job {
            let kind = if kind == MOUSE_MOVED { MouseKind::Move } else { MouseKind::Drag };
            return self.job_mouse(kind, button);
        }
        let (source, position, flags) = (self.source as usize, self.position, self.flags());
        on_worker(move || {
            // SAFETY: the source lives as long as this Input; the event is posted and released.
            let event = unsafe { CGEventCreateMouseEvent(source as CGEventSourceRef, kind, position, button) };
            if !event.is_null() {
                unsafe { CGEventSetFlags(event, flags) };
            }
            post(event)
        })
    }

    pub fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        let (index, down, up) = match button {
            Button::Left => (0u32, 1u32, 2u32),
            Button::Right => (1, 3, 4),
            Button::Middle => (2, 25, 26),
        };
        if pressed {
            let now = Instant::now();
            self.click_state = match self.last_click {
                Some((last, at)) if last == button && now - at < DOUBLE_CLICK_INTERVAL => {
                    self.click_state + 1
                }
                _ => 1,
            };
            self.last_click = Some((button, now));
        }
        self.buttons_down[index as usize] = pressed;
        if self.via_job {
            return self.job_mouse(if pressed { MouseKind::Down } else { MouseKind::Up }, index);
        }
        let (source, position, flags, clicks) = (self.source as usize, self.position, self.flags(), self.click_state);
        on_worker(move || {
            // SAFETY: as in move_to.
            let event = unsafe {
                CGEventCreateMouseEvent(source as CGEventSourceRef, if pressed { down } else { up }, position, index)
            };
            if !event.is_null() {
                unsafe {
                    CGEventSetIntegerValueField(event, FIELD_CLICK_STATE, clicks);
                    CGEventSetFlags(event, flags);
                }
            }
            post(event)
        })
    }

    pub fn keysym(&mut self, keysym: u32, pressed: bool) -> Result<()> {
        let key = from_keysym(keysym).with_context(|| format!("unsupported keysym {keysym:#x}"))?;
        if key.is_modifier() {
            let flag = modifier_flag(key);
            self.keysym_flags = if pressed {
                self.keysym_flags | flag
            } else {
                self.keysym_flags & !flag
            };
        }
        let (code, shift) = match key {
            Key::Char(c) => char_key(c).map_or((None, false), |(code, shift)| (Some(code), shift)),
            _ => (keycode(key), false),
        };
        // A shortcut keeps its flags as sent: ⌘ with 'C' is still ⌘C.
        let shortcut = self.flags() & (FLAG_COMMAND | FLAG_CONTROL) != 0;
        if self.via_job {
            let held = self.flags() as u32;
            if key.is_modifier() {
                return job::send(&[Event::Flags { code: code.unwrap_or(0), mask: held }]);
            }
            let code = code.with_context(|| format!("{key:?} can't be typed at the login window"))?;
            let character = match key {
                Key::Char(c) => Some(c),
                _ => None,
            };
            return job::send(&job::key_events(code, character, shift && !shortcut, held, pressed));
        }
        let flags = self.flags() | if shift && !shortcut { FLAG_SHIFT } else { 0 };
        if code.is_none() && !matches!(key, Key::Char(_)) {
            bail!("{key:?} has no macOS key");
        }
        let source = self.source as usize;
        on_worker(move || {
            // SAFETY: as in move_to.
            let event = unsafe { CGEventCreateKeyboardEvent(source as CGEventSourceRef, code.unwrap_or(0), pressed) };
            if event.is_null() {
                bail!("could not create the key event");
            }
            unsafe {
                // The exact character too, so typing doesn't depend on the keyboard layout.
                if let Key::Char(c) = key {
                    if code.is_none() || !shortcut {
                        let mut units = [0u16; 2];
                        let units = c.encode_utf16(&mut units);
                        CGEventKeyboardSetUnicodeString(event, units.len(), units.as_ptr());
                    }
                }
                CGEventSetFlags(event, flags);
            }
            post(event)
        })
    }

    /// A physical key by position (#2909). No Unicode string goes with it, so this Mac's own
    /// layout and input source decide what it types. `repeat` marks a held key's repeated down.
    pub fn key(&mut self, key: &'static PhysicalKey, pressed: bool, repeat: bool) -> Result<()> {
        if key.modifier.is_some() {
            self.physical_modifiers.retain(|held| held.code != key.code);
            if pressed {
                self.physical_modifiers.push(key);
            }
        }
        if self.via_job {
            let mask = self.flags() as u32;
            if key.modifier.is_some() {
                return job::send(&[Event::Flags { code: key.mac, mask }]);
            }
            let char_code = job::special_char_code(key.mac);
            return job::send(&[Event::Key {
                code: key.mac,
                char_code,
                orig_char_code: char_code,
                down: pressed,
                flags: mask,
            }]);
        }
        let (source, code, flags) = (self.source as usize, key.mac, self.flags());
        on_worker(move || {
            // SAFETY: as in move_to.
            let event = unsafe { CGEventCreateKeyboardEvent(source as CGEventSourceRef, code, pressed) };
            if event.is_null() {
                bail!("could not create the key event");
            }
            unsafe {
                if repeat {
                    CGEventSetIntegerValueField(event, FIELD_KEYBOARD_AUTOREPEAT, 1);
                }
                CGEventSetFlags(event, flags);
            }
            post(event)
        })
    }

    pub fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        // Positive requests scroll down/right; macOS wheel deltas point the other way.
        if self.via_job {
            let clamp = |delta: i32| (-delta).clamp(-100, 100) as i16;
            return job::send(&[Event::Scroll { dy: clamp(dy), dx: clamp(dx) }]);
        }
        let source = self.source as usize;
        on_worker(move || {
            // SAFETY: as in move_to.
            let event = unsafe {
                CGEventCreateScrollWheelEvent2(source as CGEventSourceRef, SCROLL_UNITS_LINE, 2, -dy, -dx, 0)
            };
            post(event)
        })
    }

    /// The pasteboard's change count, and whether its content is this helper's own write.
    pub fn clipboard_seq(&mut self) -> Result<(u64, bool)> {
        let count = pasteboard_change_count()?;
        Ok((count as u64, self.own_change == Some(count)))
    }

    /// What the general pasteboard holds: files, an image (PNG, TIFF made PNG) or text.
    pub fn clipboard_read(&mut self) -> Result<ClipboardContent> {
        read_pasteboard()
    }

    fn note_own_write(&mut self) {
        self.own_change = pasteboard_change_count().ok();
    }

    /// Put these files on the general pasteboard (file URLs, as Finder copies them) and press ⌘V:
    /// a Finder window copies them in, an app such as RealTimeX attaches them. They stay on the
    /// pasteboard, as after a local paste.
    pub fn paste_files(&mut self, paths: &[String]) -> Result<()> {
        self.gap.wait();
        self.restore = None;
        put_files_on_pasteboard(paths)?;
        self.note_own_write();
        self.press_paste()
    }

    /// Put a PNG on the general pasteboard (`public.png`) and press ⌘V. (A bitmap is Windows'.)
    pub fn paste_image(&mut self, png: &[u8], _dib: Option<&[u8]>) -> Result<()> {
        self.gap.wait();
        self.restore = None;
        put_png_on_pasteboard(png)?;
        self.note_own_write();
        self.press_paste()
    }

    /// Put the text on the pasteboard and press ⌘V (terminals paste with ⌘V on macOS too). It is
    /// written in-process, not by starting pbcopy, and the text that was there comes back later
    /// (`restore_clipboard`). Other content (an image, files) stays replaced, as before.
    pub fn paste_text(&mut self, text: &str, _terminal: bool) -> Result<()> {
        self.gap.wait();
        let previous = match self.restore.take() {
            Some((previous, _)) => Some(previous),
            None => match read_pasteboard() {
                Ok(ClipboardContent::Text(previous)) => Some(previous),
                _ => None,
            },
        };
        put_text_on_pasteboard(text)?;
        self.note_own_write();
        let sent = self.press_paste();
        if let Some(previous) = previous {
            self.restore = Some((previous, Instant::now() + RESTORE_CLIPBOARD_AFTER));
        }
        sent
    }

    /// When the last paste's previous text is due back.
    pub fn restore_due(&self) -> Option<Instant> {
        self.restore.as_ref().map(|(_, due)| *due)
    }

    /// Put the last paste's previous text back now, unless something newer was copied meanwhile.
    pub fn restore_clipboard(&mut self) {
        let Some((previous, _)) = self.restore.take() else {
            return;
        };
        let unchanged = pasteboard_change_count()
            .ok()
            .is_some_and(|count| self.own_change == Some(count));
        if unchanged && put_text_on_pasteboard(&previous).is_ok() {
            self.note_own_write();
        }
    }

    fn press_paste(&mut self) -> Result<()> {
        let flags = self.flags() | FLAG_COMMAND;
        for (code, down) in [
            (KEYCODE_COMMAND, true),
            (KEYCODE_V, true),
            (KEYCODE_V, false),
            (KEYCODE_COMMAND, false),
        ] {
            let event_flags = if code == KEYCODE_COMMAND && !down { self.flags() } else { flags };
            let source = self.source as usize;
            on_worker(move || {
                // SAFETY: as in move_to.
                let event = unsafe { CGEventCreateKeyboardEvent(source as CGEventSourceRef, code, down) };
                if !event.is_null() {
                    unsafe { CGEventSetFlags(event, event_flags) };
                }
                post(event)
            })?;
        }
        self.gap.pasted();
        Ok(())
    }
}

// NSPasteboard lives in AppKit, which the helper otherwise doesn't link.
#[link(name = "AppKit", kind = "framework")]
extern "C" {}

/// Replace the general pasteboard's contents with this text.
fn put_text_on_pasteboard(text: &str) -> Result<()> {
    use objc2::msg_send;
    use objc2::runtime::{AnyObject, Bool};
    use objc2_foundation::NSString;

    let pasteboard: *mut AnyObject = general_pasteboard()?;
    let string = NSString::from_str(text);
    let kind = NSString::from_str("public.utf8-plain-text");
    let written = unsafe {
        let _: isize = msg_send![pasteboard, clearContents];
        let ok: Bool = msg_send![pasteboard, setString: &*string, forType: &*kind];
        ok.as_bool()
    };
    if !written {
        bail!("the pasteboard refused the text");
    }
    Ok(())
}

/// Replace the general pasteboard's contents with this PNG.
fn put_png_on_pasteboard(png: &[u8]) -> Result<()> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_foundation::{NSData, NSString};

    let class = AnyClass::get(c"NSPasteboard").context("NSPasteboard is not available")?;
    let data = NSData::with_bytes(png);
    let kind = NSString::from_str("public.png");
    let written = unsafe {
        let pasteboard: *mut AnyObject = msg_send![class, generalPasteboard];
        if pasteboard.is_null() {
            bail!("no general pasteboard");
        }
        let _: isize = msg_send![pasteboard, clearContents];
        let ok: Bool = msg_send![pasteboard, setData: &*data, forType: &*kind];
        ok.as_bool()
    };
    if !written {
        bail!("the pasteboard refused the PNG");
    }
    Ok(())
}

fn general_pasteboard() -> Result<*mut objc2::runtime::AnyObject> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    let class = AnyClass::get(c"NSPasteboard").context("NSPasteboard is not available")?;
    let pasteboard: *mut AnyObject = unsafe { msg_send![class, generalPasteboard] };
    if pasteboard.is_null() {
        bail!("no general pasteboard");
    }
    Ok(pasteboard)
}

fn pasteboard_change_count() -> Result<isize> {
    use objc2::msg_send;
    let pasteboard = general_pasteboard()?;
    Ok(unsafe { msg_send![pasteboard, changeCount] })
}

/// Files first (Finder puts their names as text too), then an image, then text.
fn read_pasteboard() -> Result<ClipboardContent> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSNumber, NSString, NSURL};

    let pasteboard = general_pasteboard()?;
    unsafe {
        // Files: NSURLs that are file URLs.
        let url_class = AnyClass::get(c"NSURL").context("NSURL is not available")?;
        let classes = NSArray::from_slice(&[url_class]);
        let key = NSString::from_str("NSPasteboardURLReadingFileURLsOnlyKey");
        let yes = NSNumber::new_bool(true);
        let options = NSDictionary::from_slices(&[&*key], &[&*yes]);
        let urls: *mut NSArray<NSURL> =
            msg_send![pasteboard, readObjectsForClasses: &*classes, options: &*options];
        if let Some(urls) = urls.as_ref() {
            let paths: Vec<String> = urls
                .iter()
                .filter_map(|url| url.path().map(|path| path.to_string()))
                .collect();
            if !paths.is_empty() {
                return Ok(ClipboardContent::Files(paths));
            }
        }
        // An image: PNG as it is, TIFF (what most Mac apps copy) re-encoded as PNG.
        let png_type = NSString::from_str("public.png");
        let png: *mut NSData = msg_send![pasteboard, dataForType: &*png_type];
        if let Some(png) = png.as_ref() {
            return Ok(ClipboardContent::Image {
                bytes: png.to_vec(),
                mime: "image/png",
            });
        }
        let tiff_type = NSString::from_str("public.tiff");
        let tiff: *mut NSData = msg_send![pasteboard, dataForType: &*tiff_type];
        if let Some(tiff) = tiff.as_ref() {
            let rep_class =
                AnyClass::get(c"NSBitmapImageRep").context("NSBitmapImageRep is not available")?;
            let rep: *mut AnyObject = msg_send![rep_class, imageRepWithData: tiff];
            if !rep.is_null() {
                let props = NSDictionary::<NSString, AnyObject>::new();
                // NSBitmapImageFileTypePNG
                let png: *mut NSData =
                    msg_send![rep, representationUsingType: 4usize, properties: &*props];
                if let Some(png) = png.as_ref() {
                    return Ok(ClipboardContent::Image {
                        bytes: png.to_vec(),
                        mime: "image/png",
                    });
                }
            }
        }
        let text_type = NSString::from_str("public.utf8-plain-text");
        let text: Option<Retained<NSString>> = msg_send![pasteboard, stringForType: &*text_type];
        if let Some(text) = text {
            return Ok(ClipboardContent::Text(text.to_string()));
        }
    }
    Ok(ClipboardContent::Empty)
}

/// Replace the general pasteboard's contents with these files' URLs.
fn put_files_on_pasteboard(paths: &[String]) -> Result<()> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_foundation::{NSArray, NSString, NSURL};

    let class = AnyClass::get(c"NSPasteboard").context("NSPasteboard is not available")?;
    let urls: Vec<Retained<NSURL>> = paths
        .iter()
        .map(|path| NSURL::fileURLWithPath(&NSString::from_str(path)))
        .collect();
    let list = NSArray::from_retained_slice(&urls);
    let written = unsafe {
        let pasteboard: *mut AnyObject = msg_send![class, generalPasteboard];
        if pasteboard.is_null() {
            bail!("no general pasteboard");
        }
        let _: isize = msg_send![pasteboard, clearContents];
        let ok: Bool = msg_send![pasteboard, writeObjects: &*list];
        ok.as_bool()
    };
    if !written {
        bail!("the pasteboard refused the files");
    }
    Ok(())
}

impl Drop for Input {
    /// The session ends: the last paste's previous text still comes back, once the app had its
    /// time to read the paste.
    fn drop(&mut self) {
        // The job lets go of everything this session held there (I6).
        if self.used_job {
            let _ = job::send(&[Event::Release]);
        }
        if let Some(due) = self.restore_due() {
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            self.restore_clipboard();
        }
        if !self.source.is_null() {
            unsafe { CFRelease(self.source) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{app_name, char_key};

    /// #2753 QA: "Issue" typed key by key came out "issue".
    #[test]
    fn capitals_and_shifted_symbols_press_shift_on_their_key() {
        assert_eq!(char_key('I'), Some((0x22, true)));
        assert_eq!(char_key('i'), Some((0x22, false)));
        assert_eq!(char_key('M'), Some((0x2e, true)));
        assert_eq!(char_key('1'), Some((0x12, false)));
        assert_eq!(char_key('!'), Some((0x12, true)));
        assert_eq!(char_key('?'), Some((0x2c, true)));
        assert_eq!(char_key('"'), Some((0x27, true)));
        assert_eq!(char_key(' '), Some((0x31, false)));
        assert_eq!(char_key('ệ'), None);
        for c in "Issue Mac 123 ABC".chars() {
            let (_, shift) = char_key(c).unwrap();
            assert_eq!(shift, c.is_ascii_uppercase(), "{c}");
        }
        for c in ' '..='~' {
            assert!(char_key(c).is_some(), "{c:?} has no key");
        }
    }

    #[test]
    fn names_the_outermost_app_bundle() {
        assert_eq!(
            app_name("/Applications/RealTimeX.AI.app/Contents/MacOS/RealTimeX.AI").as_deref(),
            Some("RealTimeX.AI")
        );
        assert_eq!(
            app_name("/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal")
                .as_deref(),
            Some("Terminal")
        );
        assert_eq!(app_name("/usr/local/bin/tmux").as_deref(), Some("tmux"));
    }
}
