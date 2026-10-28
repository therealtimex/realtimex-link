//! Remote Control keys and pointer positions in Windows terms, without calling Windows: the
//! virtual-key codes for [`Key`]s and SendInput's normalised absolute coordinates.

use crate::codes::PhysicalKey;
use crate::keys::Key;

/// A virtual-key code, and whether it is an extended key (sent with KEYEVENTF_EXTENDEDKEY, so
/// the arrows and the editing block aren't read as the numeric keypad's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vk {
    pub code: u16,
    pub extended: bool,
}

const fn vk(code: u16) -> Option<Vk> {
    Some(Vk {
        code,
        extended: false,
    })
}

const fn extended(code: u16) -> Option<Vk> {
    Some(Vk {
        code,
        extended: true,
    })
}

/// The virtual key for a named key, letters and digits. Other characters depend on the
/// keyboard layout (`VkKeyScanW`) or are typed as Unicode: None here.
pub fn named_vk(key: Key) -> Option<Vk> {
    match key {
        Key::Return => vk(0x0d),
        Key::Tab => vk(0x09),
        Key::Escape => vk(0x1b),
        Key::Backspace => vk(0x08),
        Key::Delete => extended(0x2e),
        Key::Insert => extended(0x2d),
        Key::Home => extended(0x24),
        Key::End => extended(0x23),
        Key::PageUp => extended(0x21),
        Key::PageDown => extended(0x22),
        Key::Left => extended(0x25),
        Key::Up => extended(0x26),
        Key::Right => extended(0x27),
        Key::Down => extended(0x28),
        Key::Control => vk(0x11),
        Key::Shift => vk(0x10),
        Key::Alt => vk(0x12),
        Key::Super => extended(0x5b),
        Key::Menu => extended(0x5d),
        Key::PrintScreen => extended(0x2c),
        Key::F(n @ 1..=12) => vk(0x70 + n as u16 - 1),
        Key::F(_) => None,
        // VK_A..VK_Z and VK_0..VK_9 are the same on every layout.
        Key::Char(c) if c.is_ascii_lowercase() || c.is_ascii_uppercase() => {
            vk(c.to_ascii_uppercase() as u16)
        }
        Key::Char(c) if c.is_ascii_digit() => vk(c as u16),
        Key::Char(' ') => vk(0x20),
        Key::Char(_) => None,
    }
}

/// The virtual-key part of a `VkKeyScanW` result, when the character needs no Shift, Ctrl or
/// Alt on the current layout (otherwise it is typed as Unicode instead).
pub fn unshifted_scan(scan: i16) -> Option<Vk> {
    if scan == -1 {
        return None;
    }
    let [code, shift_state] = (scan as u16).to_le_bytes();
    (shift_state == 0 && code != 0).then_some(Vk {
        code: code as u16,
        extended: false,
    })
}

/// How SendInput presses a physical key (#2909): by scancode, so the layout and input method
/// decide what it types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    Scancode {
        scan: u16,
        extended: bool,
    },
    /// Pause (E1 1D 45) has no scancode SendInput takes, and NumLock's 45 is Pause's tail:
    /// both go by virtual key.
    Vk(Vk),
}

pub fn physical_press(key: &PhysicalKey) -> Press {
    match key.code {
        "Pause" => Press::Vk(Vk {
            code: 0x13,
            extended: false,
        }),
        "NumLock" => Press::Vk(Vk {
            code: 0x90,
            extended: true,
        }),
        _ => Press::Scancode {
            scan: key.windows & 0xff,
            extended: key.windows >> 8 == 0xe0,
        },
    }
}

/// The virtual desktop (all monitors): origin and size in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Desktop {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// A pixel on the virtual desktop in SendInput's absolute units with MOUSEEVENTF_VIRTUALDESK:
/// 0..=65535 across the whole desktop, clamped onto it.
pub fn normalise(desktop: Desktop, x: f64, y: f64) -> (i32, i32) {
    let axis = |value: f64, origin: i32, size: i32| {
        let span = (size - 1).max(1) as f64;
        let offset = (value - origin as f64).clamp(0.0, span);
        (offset * 65535.0 / span).round() as i32
    };
    (
        axis(x, desktop.x, desktop.width),
        axis(y, desktop.y, desktop.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_letters_and_digits_have_fixed_virtual_keys() {
        assert_eq!(named_vk(Key::Return), vk(0x0d));
        assert_eq!(named_vk(Key::Left), extended(0x25));
        assert_eq!(named_vk(Key::Super), extended(0x5b));
        assert_eq!(named_vk(Key::F(1)), vk(0x70));
        assert_eq!(named_vk(Key::F(12)), vk(0x7b));
        assert_eq!(named_vk(Key::F(13)), None);
        assert_eq!(named_vk(Key::Char('l')), vk(0x4c));
        assert_eq!(named_vk(Key::Char('L')), vk(0x4c));
        assert_eq!(named_vk(Key::Char('7')), vk(0x37));
        assert_eq!(named_vk(Key::Char('é')), None);
        assert_eq!(named_vk(Key::Char('/')), None);
    }

    #[test]
    fn layout_keys_are_used_only_without_modifiers() {
        // '/' on US: VK_OEM_2, no shift.
        assert_eq!(unshifted_scan(0x00bf), vk(0xbf));
        // '?' on US: VK_OEM_2 with Shift.
        assert_eq!(unshifted_scan(0x01bf), None);
        assert_eq!(unshifted_scan(-1), None);
    }

    #[test]
    fn physical_keys_go_by_scancode_with_the_extended_flag() {
        use crate::codes::{physical_key, KEYS};
        let press = |code| physical_press(physical_key(code).unwrap());
        assert_eq!(
            press("KeyA"),
            Press::Scancode {
                scan: 0x1e,
                extended: false
            }
        );
        assert_eq!(
            press("ArrowUp"),
            Press::Scancode {
                scan: 0x48,
                extended: true
            }
        );
        // Numpad 8 and ArrowUp share 0x48; only the flag tells them apart.
        assert_eq!(
            press("Numpad8"),
            Press::Scancode {
                scan: 0x48,
                extended: false
            }
        );
        assert_eq!(
            press("MetaLeft"),
            Press::Scancode {
                scan: 0x5b,
                extended: true
            }
        );
        assert_eq!(press("Pause"), Press::Vk(vk(0x13).unwrap()));
        assert_eq!(press("NumLock"), Press::Vk(extended(0x90).unwrap()));
        for key in KEYS {
            if let Press::Scancode { scan, .. } = physical_press(key) {
                assert!(scan != 0, "{}", key.code);
            }
        }
    }

    #[test]
    fn pixels_map_onto_the_whole_virtual_desktop() {
        let single = Desktop {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(normalise(single, 0.0, 0.0), (0, 0));
        assert_eq!(normalise(single, 1919.0, 1079.0), (65535, 65535));
        assert_eq!(normalise(single, 5000.0, -3.0), (65535, 0));
        // A second monitor on the left: the primary's (0, 0) is a third of the way across.
        let two = Desktop {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        let (x, _) = normalise(two, 0.0, 0.0);
        assert_eq!(x, (1920.0 * 65535.0 / 3839.0f64).round() as i32);
    }
}
