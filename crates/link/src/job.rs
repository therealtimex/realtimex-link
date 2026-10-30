//! The login-window job's input (realtimex-link#11 I4–I7, Addendum 5, C1–C3): while
//! loginwindow holds Secure Input, CGEvent posts are dropped, and only IOHIDPostEvent from this
//! process, the one launchd started, types. rtx-control translates the viewer's requests and
//! sends the events here on the channel this process made; this only checks and posts them.
//!
//! Bounds (I6, C2): a closed set of fixed-size kinds, checked fields, mouse positions clamped
//! to the displays, a rate limit, a limit on held keys. On `release`, a closed channel, a
//! malformed datagram or a burst past the rate, every key and button is let go and the global
//! flags go to 0; the last two also end the job (launchd starts it again), so a press is never
//! dropped without the sender finding out. Letting go (a key up, a button up, a modifier
//! cleared) always goes through, whatever the rate (Review of !13). Nothing typed is logged (I7).
//!
//! [`Poster`] keeps that state and asks a [`Sink`] to post; the IOHID sink is macOS's, a
//! recording one tests it.

use std::collections::BTreeSet;
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

/// At most this many presses, moves and scrolls a second (a fast typist and a dragging mouse
/// fit well inside); letting go isn't counted against it.
const RATE: u32 = 400;
/// At most this many keys held at once.
const MAX_HELD: usize = 16;

/// One NX event for the HID system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nx {
    Key { kind: u32, code: u16, char_code: u16, orig_char_code: u16, flags: u32 },
    Mouse { kind: u32, x: i16, y: i16, button: u8, click_state: u8, pressure: u8, flags: u32 },
    Scroll { x: i16, y: i16, dy: i16, dx: i16, flags: u32 },
}

/// Where the poster's events go: IOHIDPostEvent on macOS, a list in the tests.
pub trait Sink {
    fn post(&mut self, event: Nx);
    /// The union of the displays' bounds in global points: left, top, right, bottom.
    fn bounds(&self) -> (i32, i32, i32, i32);
}

/// What one datagram did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handled {
    Posted,
    /// Nothing to post (an up for what isn't held, a key past the held limit).
    Ignored,
    /// Past the rate: everything was let go, and the job ends.
    Overflow,
}

pub struct Poster<S: Sink> {
    sink: S,
    held_keys: BTreeSet<u16>,
    held_buttons: [bool; 3],
    flags: u32,
    position: (i16, i16),
    window: Instant,
    in_window: u32,
}

impl<S: Sink> Poster<S> {
    pub fn new(sink: S, now: Instant) -> Self {
        Poster {
            sink,
            held_keys: BTreeSet::new(),
            held_buttons: [false; 3],
            flags: 0,
            position: (0, 0),
            window: now,
            in_window: 0,
        }
    }

    /// Whether `event` lets go of something: those are never held back.
    fn lets_go(&self, event: &Event) -> bool {
        match *event {
            Event::Key { down, .. } => !down,
            Event::Flags { mask, .. } => mask & !self.flags == 0,
            Event::Mouse { kind, .. } => kind == MouseKind::Up,
            Event::Release => true,
            Event::Scroll { .. } => false,
        }
    }

    pub fn handle(&mut self, event: Event, now: Instant) -> Handled {
        if !self.lets_go(&event) {
            if now.duration_since(self.window) >= Duration::from_secs(1) {
                self.window = now;
                self.in_window = 0;
            }
            self.in_window += 1;
            if self.in_window > RATE {
                self.release();
                return Handled::Overflow;
            }
        }
        match event {
            Event::Key { code, char_code, orig_char_code, down, flags } => {
                if down {
                    if self.held_keys.len() >= MAX_HELD && !self.held_keys.contains(&code) {
                        return Handled::Ignored;
                    }
                    self.held_keys.insert(code);
                } else if !self.held_keys.remove(&code) {
                    return Handled::Ignored;
                }
                let kind = if down { NX_KEYDOWN } else { NX_KEYUP };
                self.sink.post(Nx::Key { kind, code, char_code, orig_char_code, flags });
            }
            Event::Flags { code, mask } => {
                self.flags = mask;
                self.sink.post(Nx::Key { kind: NX_FLAGSCHANGED, code, char_code: 0, orig_char_code: 0, flags: mask });
            }
            Event::Mouse { x, y, kind, button, click_state } => {
                let (left, top, right, bottom) = self.sink.bounds();
                let clamp = |value: i32, low: i32, high: i32| {
                    value.clamp(low, high.max(low)).clamp(i16::MIN.into(), i16::MAX.into()) as i16
                };
                let (x, y) = (clamp(x, left, right), clamp(y, top, bottom));
                self.position = (x, y);
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
                    MouseKind::Up if !self.held_buttons[index] => return Handled::Ignored,
                    MouseKind::Up => self.held_buttons[index] = false,
                    _ => {}
                }
                let pressure = if matches!(kind, MouseKind::Down | MouseKind::Drag) { 255 } else { 0 };
                self.sink.post(Nx::Mouse { kind: nx, x, y, button, click_state, pressure, flags: self.flags });
            }
            Event::Scroll { dy, dx } => {
                let (x, y) = self.position;
                self.sink.post(Nx::Scroll { x, y, dy, dx, flags: self.flags });
            }
            Event::Release => self.release(),
        }
        Handled::Posted
    }

    /// Let go of everything, and the global flags to 0 (I6), so nothing carries into the
    /// session that follows a sign-in.
    pub fn release(&mut self) {
        for code in std::mem::take(&mut self.held_keys) {
            self.sink.post(Nx::Key { kind: NX_KEYUP, code, char_code: 0, orig_char_code: 0, flags: 0 });
        }
        let (x, y) = self.position;
        let held = std::mem::take(&mut self.held_buttons);
        for (button, _) in held.iter().enumerate().filter(|(_, &down)| down) {
            let kind = [NX_LMOUSEUP, NX_RMOUSEUP, NX_OMOUSEUP][button];
            self.sink.post(Nx::Mouse { kind, x, y, button: button as u8, click_state: 0, pressure: 0, flags: 0 });
        }
        self.flags = 0;
        self.sink.post(Nx::Key { kind: NX_FLAGSCHANGED, code: 0, char_code: 0, orig_char_code: 0, flags: 0 });
    }
}

#[cfg(target_os = "macos")]
pub use hid::serve;

#[cfg(target_os = "macos")]
mod hid {
    use std::ffi::{c_char, c_void};
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::time::Instant;

    use super::{jobinput, Event, Handled, Nx, Poster, Sink, NX_FLAGSCHANGED, NX_LMOUSEDRAGGED, NX_MOUSEMOVED, NX_OMOUSEDRAGGED, NX_RMOUSEDRAGGED, NX_SCROLLWHEELMOVED};

    const SET_GLOBAL_EVENT_FLAGS: u32 = 1;
    const SET_CURSOR_POSITION: u32 = 2;
    /// kNXEventDataVersion.
    const EVENT_DATA_VERSION: u32 = 2;

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

    struct Hid(u32);

    impl Hid {
        fn send(&self, kind: u32, location: IOGPoint, data: &[u8; 128], flags: u32, options: u32) {
            // SAFETY: data outlives the call and is larger than NXEventData.
            unsafe { IOHIDPostEvent(self.0, kind, location, data.as_ptr().cast(), EVENT_DATA_VERSION, flags, options) };
        }
    }

    impl Sink for Hid {
        fn post(&mut self, event: Nx) {
            let mut data = [0u8; 128];
            match event {
                Nx::Key { kind, code, char_code, orig_char_code, flags } => {
                    // NXEventData.key: origCharSet, repeat, charSet, charCode, keyCode, origCharCode.
                    data[6..8].copy_from_slice(&char_code.to_ne_bytes());
                    data[8..10].copy_from_slice(&code.to_ne_bytes());
                    data[10..12].copy_from_slice(&orig_char_code.to_ne_bytes());
                    let options = if kind == NX_FLAGSCHANGED { SET_GLOBAL_EVENT_FLAGS } else { 0 };
                    self.send(kind, IOGPoint { x: 0, y: 0 }, &data, flags, options);
                }
                Nx::Mouse { kind, x, y, button, click_state, pressure, flags } => {
                    // NXEventData.mouse: subx, suby, eventNum, click, pressure, buttonNumber.
                    data[4..8].copy_from_slice(&i32::from(click_state).to_ne_bytes());
                    data[8] = pressure;
                    data[9] = button;
                    let moves = matches!(kind, NX_MOUSEMOVED | NX_LMOUSEDRAGGED | NX_RMOUSEDRAGGED | NX_OMOUSEDRAGGED);
                    let options = if moves { SET_CURSOR_POSITION } else { 0 };
                    self.send(kind, IOGPoint { x, y }, &data, flags, options);
                }
                Nx::Scroll { x, y, dy, dx, flags } => {
                    // NXEventData.scrollWheel: deltaAxis1 (vertical), deltaAxis2 (horizontal).
                    data[0..2].copy_from_slice(&dy.to_ne_bytes());
                    data[2..4].copy_from_slice(&dx.to_ne_bytes());
                    self.send(NX_SCROLLWHEELMOVED, IOGPoint { x, y }, &data, flags, 0);
                }
            }
        }

        fn bounds(&self) -> (i32, i32, i32, i32) {
            let mut displays = [0u32; 16];
            let mut count = 0u32;
            // SAFETY: the buffer's length is passed.
            if unsafe { CGGetActiveDisplayList(16, displays.as_mut_ptr(), &mut count) } != 0 || count == 0 {
                return (0, 0, 0, 0);
            }
            let (mut left, mut top, mut right, mut bottom) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
            for &display in &displays[..count as usize] {
                // SAFETY: a display id CoreGraphics just listed.
                let b = unsafe { CGDisplayBounds(display) };
                left = left.min(b.x as i32);
                top = top.min(b.y as i32);
                right = right.max((b.x + b.width) as i32 - 1);
                bottom = bottom.max((b.y + b.height) as i32 - 1);
            }
            (left, top, right, bottom)
        }
    }

    /// Post what arrives on `channel` until it closes. True when it closed cleanly; false after a
    /// malformed datagram or a burst past the rate (the caller ends the job).
    pub fn serve(channel: OwnedFd) -> bool {
        let Some(connect) = hid_system() else {
            // No HID system to post to: rtx-control's sends are dropped by the kernel, nothing waits.
            return true;
        };
        let mut poster = Poster::new(Hid(connect), Instant::now());
        let mut buffer = [0u8; jobinput::SIZE + 1];
        loop {
            // SAFETY: a SOCK_DGRAM socketpair this process owns; one datagram per call, and a
            // bigger one fills the buffer past SIZE (and is refused).
            let read = unsafe { libc::recv(channel.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), 0) };
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
            if poster.handle(event, Instant::now()) == Handled::Overflow {
                eprintln!("job: input past the rate: released everything, ending");
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIFT: u32 = 0x0002_0000;

    #[derive(Default)]
    struct Recorder(Vec<Nx>);

    impl Sink for Recorder {
        fn post(&mut self, event: Nx) {
            self.0.push(event);
        }
        fn bounds(&self) -> (i32, i32, i32, i32) {
            (0, 0, 1919, 1079)
        }
    }

    fn mv() -> Event {
        Event::Mouse { x: 10, y: 10, kind: MouseKind::Move, button: 0, click_state: 0 }
    }

    /// Fill the second's budget up to `left` events, at `now`.
    fn spend(poster: &mut Poster<Recorder>, now: Instant, left: u32) {
        let used = poster.in_window;
        for _ in used..RATE - left {
            assert_eq!(poster.handle(mv(), now), Handled::Posted);
        }
    }

    fn key(code: u16, char_code: u16, down: bool, flags: u32) -> Event {
        Event::Key { code, char_code, orig_char_code: char_code, down, flags }
    }

    #[test]
    fn letting_go_always_goes_through_at_the_rate_boundary() {
        let now = Instant::now();
        let mut poster = Poster::new(Recorder::default(), now);
        // Shift down and a key down, then the budget spent: their ups and the cleared modifier
        // still post (Review of !13's example).
        assert_eq!(poster.handle(Event::Flags { code: 0x38, mask: SHIFT }, now), Handled::Posted);
        assert_eq!(poster.handle(key(0x00, 'A' as u16, true, SHIFT), now), Handled::Posted);
        assert_eq!(poster.handle(Event::Mouse { x: 5, y: 5, kind: MouseKind::Down, button: 0, click_state: 1 }, now), Handled::Posted);
        spend(&mut poster, now, 0);
        poster.sink.0.clear();
        assert_eq!(poster.handle(key(0x00, 'A' as u16, false, SHIFT), now), Handled::Posted);
        assert_eq!(poster.handle(Event::Flags { code: 0x38, mask: 0 }, now), Handled::Posted);
        assert_eq!(poster.handle(Event::Mouse { x: 5, y: 5, kind: MouseKind::Up, button: 0, click_state: 1 }, now), Handled::Posted);
        assert_eq!(
            poster.sink.0,
            [
                Nx::Key { kind: NX_KEYUP, code: 0x00, char_code: 'A' as u16, orig_char_code: 'A' as u16, flags: SHIFT },
                Nx::Key { kind: NX_FLAGSCHANGED, code: 0x38, char_code: 0, orig_char_code: 0, flags: 0 },
                Nx::Mouse { kind: NX_LMOUSEUP, x: 5, y: 5, button: 0, click_state: 1, pressure: 0, flags: 0 },
            ]
        );
        assert!(poster.held_keys.is_empty() && poster.held_buttons == [false; 3] && poster.flags == 0);
    }

    #[test]
    fn a_press_past_the_rate_releases_everything_and_ends_the_job() {
        let now = Instant::now();
        let mut poster = Poster::new(Recorder::default(), now);
        // A shifted character's sequence started: Shift down and the key down, both held.
        poster.handle(Event::Flags { code: 0x38, mask: SHIFT }, now);
        poster.handle(key(0x12, '!' as u16, true, SHIFT), now);
        poster.handle(Event::Mouse { x: 1, y: 1, kind: MouseKind::Down, button: 1, click_state: 1 }, now);
        spend(&mut poster, now, 0);
        poster.sink.0.clear();
        // The next press is past the rate: never silently dropped.
        assert_eq!(poster.handle(key(0x13, '@' as u16, true, SHIFT), now), Handled::Overflow);
        assert_eq!(
            poster.sink.0,
            [
                Nx::Key { kind: NX_KEYUP, code: 0x12, char_code: 0, orig_char_code: 0, flags: 0 },
                Nx::Mouse { kind: NX_RMOUSEUP, x: 1, y: 1, button: 1, click_state: 0, pressure: 0, flags: 0 },
                Nx::Key { kind: NX_FLAGSCHANGED, code: 0, char_code: 0, orig_char_code: 0, flags: 0 },
            ]
        );
        assert!(poster.held_keys.is_empty() && poster.flags == 0);
    }

    #[test]
    fn the_budget_is_per_second_and_ups_of_what_isnt_held_post_nothing() {
        let start = Instant::now();
        let mut poster = Poster::new(Recorder::default(), start);
        spend(&mut poster, start, 0);
        // A new second: presses go through again.
        let later = start + Duration::from_secs(1);
        assert_eq!(poster.handle(key(0x00, 97, true, 0), later), Handled::Posted);
        assert_eq!(poster.handle(key(0x00, 97, false, 0), later), Handled::Posted);
        assert_eq!(poster.handle(key(0x00, 97, false, 0), later), Handled::Ignored);
        assert_eq!(
            poster.handle(Event::Mouse { x: 0, y: 0, kind: MouseKind::Up, button: 2, click_state: 0 }, later),
            Handled::Ignored
        );
    }

    #[test]
    fn the_mouse_stays_on_the_displays() {
        let now = Instant::now();
        let mut poster = Poster::new(Recorder::default(), now);
        poster.handle(Event::Mouse { x: 5000, y: -40, kind: MouseKind::Move, button: 0, click_state: 0 }, now);
        assert!(matches!(poster.sink.0[0], Nx::Mouse { x: 1919, y: 0, .. }));
    }
}
