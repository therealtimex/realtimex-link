//! The login-window job's input (realtimex-link#11 I4–I7, Addendum 5, C1–C3): while
//! loginwindow holds Secure Input, CGEvent posts are dropped, and only IOHIDPostEvent from this
//! process, the one launchd started, types. rtx-control translates the viewer's requests and
//! sends the events here on the channel this process made; this only checks and posts them.
//!
//! Bounds (I6, C2): a closed set of fixed-size kinds, checked fields, mouse positions clamped
//! to the displays, a rate limit, a limit on held keys. On `release`, a closed channel or a
//! malformed datagram, every key and button is let go and the global flags go to 0; a malformed
//! one also ends the job (launchd starts it again). Nothing typed is logged (I7).

use std::collections::BTreeSet;
use std::ffi::{c_char, c_void};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

#[path = "../../control/src/jobinput.rs"]
#[allow(dead_code)]
mod jobinput;

use jobinput::{Event, MouseKind};

const NX_LMOUSEDOWN: u32 = 1;
const NX_LMOUSEUP: u32 = 2;
const NX_RMOUSEDOWN: u32 = 3;
const NX_RMOUSEUP: u32 = 4;
const NX_MOUSEMOVED: u32 = 5;
const NX_LMOUSEDRAGGED: u32 = 6;
const NX_RMOUSEDRAGGED: u32 = 7;
const NX_KEYDOWN: u32 = 10;
const NX_KEYUP: u32 = 11;
const NX_FLAGSCHANGED: u32 = 12;
const NX_SCROLLWHEELMOVED: u32 = 22;
const NX_OMOUSEDOWN: u32 = 25;
const NX_OMOUSEUP: u32 = 26;
const NX_OMOUSEDRAGGED: u32 = 27;
const SET_GLOBAL_EVENT_FLAGS: u32 = 1;
const SET_CURSOR_POSITION: u32 = 2;
/// kNXEventDataVersion.
const EVENT_DATA_VERSION: u32 = 2;

/// At most this many events a second (a fast typist and a dragging mouse fit well inside).
const RATE: u32 = 400;
/// At most this many keys held at once.
const MAX_HELD: usize = 16;

#[repr(C)]
#[derive(Clone, Copy)]
struct IOGPoint {
    x: i16,
    y: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    static kIOMasterPortDefault: u32;
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(port: u32, matching: *mut c_void) -> u32;
    fn IOServiceOpen(service: u32, task: u32, kind: u32, connect: *mut u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOHIDPostEvent(
        connect: u32,
        kind: u32,
        location: IOGPoint,
        data: *const c_void,
        version: u32,
        flags: u32,
        options: u32,
    ) -> i32;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
}

extern "C" {
    static mach_task_self_: u32;
}

/// The union of the displays' bounds, in global points.
fn display_union() -> (i32, i32, i32, i32) {
    let mut displays = [0u32; 16];
    let mut count = 0u32;
    // SAFETY: the buffer's length is passed.
    if unsafe { CGGetActiveDisplayList(16, displays.as_mut_ptr(), &mut count) } != 0 || count == 0 {
        return (0, 0, 0, 0);
    }
    let (mut left, mut top, mut right, mut bottom) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for &display in &displays[..count as usize] {
        // SAFETY: a display id CoreGraphics just listed.
        let bounds = unsafe { CGDisplayBounds(display) };
        left = left.min(bounds.x as i32);
        top = top.min(bounds.y as i32);
        right = right.max((bounds.x + bounds.width) as i32 - 1);
        bottom = bottom.max((bounds.y + bounds.height) as i32 - 1);
    }
    (left, top, right, bottom)
}

/// The HID system, as IOHIDPostEvent wants it.
fn hid_system() -> Option<u32> {
    // SAFETY: IOKit lookups by name; the service is released once opened.
    unsafe {
        let service = IOServiceGetMatchingService(kIOMasterPortDefault, IOServiceMatching(c"IOHIDSystem".as_ptr()));
        if service == 0 {
            return None;
        }
        let mut connect = 0;
        // kIOHIDParamConnectType
        let status = IOServiceOpen(service, mach_task_self_, 1, &mut connect);
        IOObjectRelease(service);
        (status == 0).then_some(connect)
    }
}

struct Poster {
    connect: u32,
    held_keys: BTreeSet<u16>,
    held_buttons: [bool; 3],
    flags: u32,
    position: IOGPoint,
    window: Instant,
    in_window: u32,
}

impl Poster {
    fn post(&self, kind: u32, location: IOGPoint, data: &[u8; 128], flags: u32, options: u32) {
        // SAFETY: data outlives the call and is larger than NXEventData.
        unsafe { IOHIDPostEvent(self.connect, kind, location, data.as_ptr().cast(), EVENT_DATA_VERSION, flags, options) };
    }

    fn key(&self, kind: u32, code: u16, char_code: u16, orig_char_code: u16, flags: u32) {
        // NXEventData.key: origCharSet, repeat, charSet, charCode, keyCode, origCharCode.
        let mut data = [0u8; 128];
        data[6..8].copy_from_slice(&char_code.to_ne_bytes());
        data[8..10].copy_from_slice(&code.to_ne_bytes());
        data[10..12].copy_from_slice(&orig_char_code.to_ne_bytes());
        let options = if kind == NX_FLAGSCHANGED { SET_GLOBAL_EVENT_FLAGS } else { 0 };
        self.post(kind, IOGPoint { x: 0, y: 0 }, &data, flags, options);
    }

    fn mouse(&self, kind: u32, at: IOGPoint, button: u8, click_state: u8, pressure: u8) {
        // NXEventData.mouse: subx, suby, eventNum, click, pressure, buttonNumber.
        let mut data = [0u8; 128];
        data[4..8].copy_from_slice(&i32::from(click_state).to_ne_bytes());
        data[8] = pressure;
        data[9] = button;
        let options = if matches!(kind, NX_MOUSEMOVED | NX_LMOUSEDRAGGED | NX_RMOUSEDRAGGED | NX_OMOUSEDRAGGED) {
            SET_CURSOR_POSITION
        } else {
            0
        };
        self.post(kind, at, &data, self.flags, options);
    }

    /// Whether this event fits in the rate limit.
    fn admit(&mut self) -> bool {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.in_window = 0;
        }
        self.in_window += 1;
        self.in_window <= RATE
    }

    fn handle(&mut self, event: Event) {
        if event != Event::Release && !self.admit() {
            return;
        }
        match event {
            Event::Key { code, char_code, orig_char_code, down, flags } => {
                if down {
                    if self.held_keys.len() >= MAX_HELD && !self.held_keys.contains(&code) {
                        return;
                    }
                    self.held_keys.insert(code);
                } else if !self.held_keys.remove(&code) {
                    return;
                }
                self.key(if down { NX_KEYDOWN } else { NX_KEYUP }, code, char_code, orig_char_code, flags);
            }
            Event::Flags { code, mask } => {
                self.flags = mask;
                self.key(NX_FLAGSCHANGED, code, 0, 0, mask);
            }
            Event::Mouse { x, y, kind, button, click_state } => {
                let (left, top, right, bottom) = display_union();
                let at = IOGPoint {
                    x: x.clamp(left, right.max(left)).clamp(i16::MIN.into(), i16::MAX.into()) as i16,
                    y: y.clamp(top, bottom.max(top)).clamp(i16::MIN.into(), i16::MAX.into()) as i16,
                };
                self.position = at;
                let index = usize::from(button);
                let nx = match (kind, button) {
                    (MouseKind::Move, _) => NX_MOUSEMOVED,
                    (MouseKind::Drag, 0) => NX_LMOUSEDRAGGED,
                    (MouseKind::Drag, 1) => NX_RMOUSEDRAGGED,
                    (MouseKind::Drag, _) => NX_OMOUSEDRAGGED,
                    (MouseKind::Down, 0) => NX_LMOUSEDOWN,
                    (MouseKind::Down, 1) => NX_RMOUSEDOWN,
                    (MouseKind::Down, _) => NX_OMOUSEDOWN,
                    (MouseKind::Up, 0) => NX_LMOUSEUP,
                    (MouseKind::Up, 1) => NX_RMOUSEUP,
                    (MouseKind::Up, _) => NX_OMOUSEUP,
                };
                match kind {
                    MouseKind::Down => self.held_buttons[index] = true,
                    MouseKind::Up if !self.held_buttons[index] => return,
                    MouseKind::Up => self.held_buttons[index] = false,
                    _ => {}
                }
                let pressure = if kind == MouseKind::Down || kind == MouseKind::Drag { 255 } else { 0 };
                self.mouse(nx, at, button, click_state, pressure);
            }
            Event::Scroll { dy, dx } => {
                // NXEventData.scrollWheel: deltaAxis1 (vertical), deltaAxis2 (horizontal).
                let mut data = [0u8; 128];
                data[0..2].copy_from_slice(&dy.to_ne_bytes());
                data[2..4].copy_from_slice(&dx.to_ne_bytes());
                self.post(NX_SCROLLWHEELMOVED, self.position, &data, self.flags, 0);
            }
            Event::Release => self.release(),
        }
    }

    /// Let go of everything, and the global flags to 0 (I6), so nothing carries into the
    /// session that follows a sign-in.
    fn release(&mut self) {
        for code in std::mem::take(&mut self.held_keys) {
            self.key(NX_KEYUP, code, 0, 0, 0);
        }
        let held = std::mem::take(&mut self.held_buttons);
        for (button, _) in held.iter().enumerate().filter(|(_, &down)| down) {
            let kind = [NX_LMOUSEUP, NX_RMOUSEUP, NX_OMOUSEUP][button];
            let mut data = [0u8; 128];
            data[9] = button as u8;
            self.post(kind, self.position, &data, 0, 0);
        }
        self.flags = 0;
        self.key(NX_FLAGSCHANGED, 0, 0, 0, 0);
    }
}

/// Post what arrives on `channel` until it closes. True when it closed cleanly; false after a
/// malformed datagram (the caller ends the job).
pub fn serve(channel: OwnedFd) -> bool {
    let Some(connect) = hid_system() else {
        // No HID system to post to: rtx-control's sends are dropped by the kernel, nothing waits.
        return true;
    };
    let mut poster = Poster {
        connect,
        held_keys: BTreeSet::new(),
        held_buttons: [false; 3],
        flags: 0,
        position: IOGPoint { x: 0, y: 0 },
        window: Instant::now(),
        in_window: 0,
    };
    let mut buffer = [0u8; jobinput::SIZE + 1];
    loop {
        // SAFETY: a SOCK_SEQPACKET socket this process owns; one datagram per call, and a
        // bigger one shows as a longer length (MSG_TRUNC).
        let read = unsafe {
            libc::recv(channel.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), libc::MSG_TRUNC)
        };
        if read <= 0 {
            poster.release();
            return true;
        }
        let length = (read as usize).min(buffer.len());
        let Some(event) = (read as usize == jobinput::SIZE).then(|| Event::decode(&buffer[..length])).flatten() else {
            eprintln!("job: a malformed input datagram: releasing and ending");
            poster.release();
            return false;
        };
        poster.handle(event);
    }
}
