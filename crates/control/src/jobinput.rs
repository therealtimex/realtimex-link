//! What rtx-control sends the login-window job (realtimex-link#11, Addendum 5): input already
//! translated, as fixed-size datagrams on the SOCK_DGRAM socketpair the job made. rtx-control
//! encodes them (one translator: I2, I5); the job (rtx-link `--launchd session`) decodes,
//! checks (C2) and posts them with IOHIDPostEvent. No kind carries text or clipboard data.
//! Shared by rtx-control, rtx-link and rtx-linkd (`#[path]`); data only.

/// The format's version: a datagram of another is refused.
pub const VERSION: u8 = 1;
/// Every datagram's size.
pub const SIZE: usize = 20;

/// NX modifier bits a flags mask may carry: Caps Lock, Shift, Control, Option, Command, and their
/// left/right device bits.
pub const MODIFIERS: u32 = 0x001F_0000 | 0x0000_207F;
/// Virtual key codes stop at 0x7F.
pub const MAX_KEY_CODE: u16 = 0x7F;
/// One wheel datagram moves at most this many lines.
pub const MAX_SCROLL: i16 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Move,
    Down,
    Up,
    /// Moved with `button` held.
    Drag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A key, with its character codes (Addendum 4) and the modifier mask it goes with.
    Key { code: u16, char_code: u16, orig_char_code: u16, down: bool, flags: u32 },
    /// The held modifiers changed (`code`: the modifier key that did it): posted as
    /// NX_FLAGSCHANGED with kIOHIDSetGlobalEventFlags.
    Flags { code: u16, mask: u32 },
    /// In global display points; `button` 0 left, 1 right, 2 middle.
    Mouse { x: i32, y: i32, kind: MouseKind, button: u8, click_state: u8 },
    Scroll { dy: i16, dx: i16 },
    /// Let go of every key and button, and clear the global flags.
    Release,
}

const KEY: u8 = 1;
const FLAGS: u8 = 2;
const MOUSE: u8 = 3;
const SCROLL: u8 = 4;
const RELEASE: u8 = 5;

impl Event {
    pub fn encode(self) -> [u8; SIZE] {
        let mut out = [0u8; SIZE];
        out[0] = VERSION;
        match self {
            Event::Key { code, char_code, orig_char_code, down, flags } => {
                out[1] = KEY;
                out[2..4].copy_from_slice(&code.to_le_bytes());
                out[4..6].copy_from_slice(&char_code.to_le_bytes());
                out[6..8].copy_from_slice(&orig_char_code.to_le_bytes());
                out[8] = u8::from(down);
                out[12..16].copy_from_slice(&flags.to_le_bytes());
            }
            Event::Flags { code, mask } => {
                out[1] = FLAGS;
                out[2..4].copy_from_slice(&code.to_le_bytes());
                out[12..16].copy_from_slice(&mask.to_le_bytes());
            }
            Event::Mouse { x, y, kind, button, click_state } => {
                out[1] = MOUSE;
                out[2] = match kind {
                    MouseKind::Move => 0,
                    MouseKind::Down => 1,
                    MouseKind::Up => 2,
                    MouseKind::Drag => 3,
                };
                out[3] = button;
                out[4] = click_state;
                out[8..12].copy_from_slice(&x.to_le_bytes());
                out[12..16].copy_from_slice(&y.to_le_bytes());
            }
            Event::Scroll { dy, dx } => {
                out[1] = SCROLL;
                out[2..4].copy_from_slice(&dy.to_le_bytes());
                out[4..6].copy_from_slice(&dx.to_le_bytes());
            }
            Event::Release => out[1] = RELEASE,
        }
        out
    }

    /// A datagram, checked (C2): its size and version, a known kind, a key code in range, only
    /// modifier bits, a known button and mouse kind, and a bounded scroll. None for anything
    /// else: the job then releases everything and closes the channel.
    pub fn decode(bytes: &[u8]) -> Option<Event> {
        if bytes.len() != SIZE || bytes[0] != VERSION {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        let i16_at = |i: usize| i16::from_le_bytes([bytes[i], bytes[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().expect("4 bytes"));
        let i32_at = |i: usize| i32::from_le_bytes(bytes[i..i + 4].try_into().expect("4 bytes"));
        let event = match bytes[1] {
            KEY => {
                let code = u16_at(2);
                let flags = u32_at(12);
                if code > MAX_KEY_CODE || flags & !MODIFIERS != 0 || bytes[8] > 1 {
                    return None;
                }
                Event::Key {
                    code,
                    char_code: u16_at(4),
                    orig_char_code: u16_at(6),
                    down: bytes[8] == 1,
                    flags,
                }
            }
            FLAGS => {
                let (code, mask) = (u16_at(2), u32_at(12));
                if code > MAX_KEY_CODE || mask & !MODIFIERS != 0 {
                    return None;
                }
                Event::Flags { code, mask }
            }
            MOUSE => {
                let kind = match bytes[2] {
                    0 => MouseKind::Move,
                    1 => MouseKind::Down,
                    2 => MouseKind::Up,
                    3 => MouseKind::Drag,
                    _ => return None,
                };
                if bytes[3] > 2 || bytes[4] > 3 {
                    return None;
                }
                Event::Mouse { x: i32_at(8), y: i32_at(12), kind, button: bytes[3], click_state: bytes[4] }
            }
            SCROLL => {
                let (dy, dx) = (i16_at(2), i16_at(4));
                if dy.abs() > MAX_SCROLL || dx.abs() > MAX_SCROLL {
                    return None;
                }
                Event::Scroll { dy, dx }
            }
            RELEASE => Event::Release,
            _ => return None,
        };
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips() {
        for event in [
            Event::Key { code: 0x24, char_code: 0x0d, orig_char_code: 0x0d, down: true, flags: 0x0002_0002 },
            Event::Flags { code: 0x37, mask: 0x0010_0008 },
            Event::Mouse { x: 640, y: -12, kind: MouseKind::Drag, button: 0, click_state: 2 },
            Event::Scroll { dy: -3, dx: 1 },
            Event::Release,
        ] {
            assert_eq!(Event::decode(&event.encode()), Some(event));
        }
    }

    #[test]
    fn anything_else_is_refused() {
        let key = Event::Key { code: 0, char_code: 97, orig_char_code: 97, down: true, flags: 0 }.encode();
        assert!(Event::decode(&key[..SIZE - 1]).is_none());
        let mut version = key;
        version[0] = 2;
        assert!(Event::decode(&version).is_none());
        let mut kind = key;
        kind[1] = 9;
        assert!(Event::decode(&kind).is_none());
        let mut code = key;
        code[2..4].copy_from_slice(&0x80u16.to_le_bytes());
        assert!(Event::decode(&code).is_none());
        let mut flags = key;
        flags[12..16].copy_from_slice(&0x0100_0000u32.to_le_bytes());
        assert!(Event::decode(&flags).is_none());
        let mut button = Event::Mouse { x: 0, y: 0, kind: MouseKind::Down, button: 0, click_state: 1 }.encode();
        button[3] = 7;
        assert!(Event::decode(&button).is_none());
        let scroll = Event::Scroll { dy: 0, dx: 0 }.encode();
        let mut far = scroll;
        far[2..4].copy_from_slice(&500i16.to_le_bytes());
        assert!(Event::decode(&far).is_none());
    }
}
