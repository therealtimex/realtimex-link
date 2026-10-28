//! Physical keys for Remote Control (realtimex-link#7, realtimex-ai-app#2909): a W3C UI Events
//! `KeyboardEvent.code` names a key by its position, and each platform presses the key at that
//! position. The host's own layout and input source decide what it types.
//!
//! rtx-linkd shares this file (`#[path]`) to check codes before a batch runs, so it holds data
//! only: no input is posted from here.

/// A key by position, in each platform's terms.
#[derive(Debug, PartialEq, Eq)]
pub struct PhysicalKey {
    pub code: &'static str,
    /// Linux evdev (`KEY_*`). XTest keycodes are these plus 8; for the main block they are
    /// also the PC set-1 scancodes.
    pub evdev: u16,
    /// macOS virtual key code (`kVK_*`).
    pub mac: u16,
    /// Windows set-1 scancode; 0xe0xx is an extended key (KEYEVENTF_EXTENDEDKEY). Pause and
    /// NumLock follow Chromium's table (0x0045 and 0xe045) and are sent by virtual key instead.
    pub windows: u16,
    pub modifier: Option<Modifier>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modifier {
    Shift,
    Control,
    Alt,
    /// ⌘ on macOS, the Windows key elsewhere.
    Meta,
}

const fn key(code: &'static str, evdev: u16, mac: u16, windows: u16) -> PhysicalKey {
    PhysicalKey {
        code,
        evdev,
        mac,
        windows,
        modifier: None,
    }
}

const fn modifier(
    code: &'static str,
    evdev: u16,
    mac: u16,
    windows: u16,
    modifier: Modifier,
) -> PhysicalKey {
    PhysicalKey {
        code,
        evdev,
        mac,
        windows,
        modifier: Some(modifier),
    }
}

/// Every code Link presses: the list agreed with RealTimeX (#2909). A PC key with no Mac
/// equivalent is the key a PC keyboard's one sends on a Mac (PrintScreen is F13, NumLock is
/// Clear).
pub const KEYS: &[PhysicalKey] = &[
    key("KeyA", 30, 0x00, 0x1e),
    key("KeyB", 48, 0x0b, 0x30),
    key("KeyC", 46, 0x08, 0x2e),
    key("KeyD", 32, 0x02, 0x20),
    key("KeyE", 18, 0x0e, 0x12),
    key("KeyF", 33, 0x03, 0x21),
    key("KeyG", 34, 0x05, 0x22),
    key("KeyH", 35, 0x04, 0x23),
    key("KeyI", 23, 0x22, 0x17),
    key("KeyJ", 36, 0x26, 0x24),
    key("KeyK", 37, 0x28, 0x25),
    key("KeyL", 38, 0x25, 0x26),
    key("KeyM", 50, 0x2e, 0x32),
    key("KeyN", 49, 0x2d, 0x31),
    key("KeyO", 24, 0x1f, 0x18),
    key("KeyP", 25, 0x23, 0x19),
    key("KeyQ", 16, 0x0c, 0x10),
    key("KeyR", 19, 0x0f, 0x13),
    key("KeyS", 31, 0x01, 0x1f),
    key("KeyT", 20, 0x11, 0x14),
    key("KeyU", 22, 0x20, 0x16),
    key("KeyV", 47, 0x09, 0x2f),
    key("KeyW", 17, 0x0d, 0x11),
    key("KeyX", 45, 0x07, 0x2d),
    key("KeyY", 21, 0x10, 0x15),
    key("KeyZ", 44, 0x06, 0x2c),
    key("Digit1", 2, 0x12, 0x02),
    key("Digit2", 3, 0x13, 0x03),
    key("Digit3", 4, 0x14, 0x04),
    key("Digit4", 5, 0x15, 0x05),
    key("Digit5", 6, 0x17, 0x06),
    key("Digit6", 7, 0x16, 0x07),
    key("Digit7", 8, 0x1a, 0x08),
    key("Digit8", 9, 0x1c, 0x09),
    key("Digit9", 10, 0x19, 0x0a),
    key("Digit0", 11, 0x1d, 0x0b),
    key("Backquote", 41, 0x32, 0x29),
    key("Minus", 12, 0x1b, 0x0c),
    key("Equal", 13, 0x18, 0x0d),
    key("BracketLeft", 26, 0x21, 0x1a),
    key("BracketRight", 27, 0x1e, 0x1b),
    key("Backslash", 43, 0x2a, 0x2b),
    key("Semicolon", 39, 0x29, 0x27),
    key("Quote", 40, 0x27, 0x28),
    key("Comma", 51, 0x2b, 0x33),
    key("Period", 52, 0x2f, 0x34),
    key("Slash", 53, 0x2c, 0x35),
    key("IntlBackslash", 86, 0x0a, 0x56),
    key("IntlRo", 89, 0x5e, 0x73),
    key("IntlYen", 124, 0x5d, 0x7d),
    key("Space", 57, 0x31, 0x39),
    key("Enter", 28, 0x24, 0x1c),
    key("Tab", 15, 0x30, 0x0f),
    key("Escape", 1, 0x35, 0x01),
    key("Backspace", 14, 0x33, 0x0e),
    key("Delete", 111, 0x75, 0xe053),
    key("Insert", 110, 0x72, 0xe052),
    key("CapsLock", 58, 0x39, 0x3a),
    key("Home", 102, 0x73, 0xe047),
    key("End", 107, 0x77, 0xe04f),
    key("PageUp", 104, 0x74, 0xe049),
    key("PageDown", 109, 0x79, 0xe051),
    key("ArrowUp", 103, 0x7e, 0xe048),
    key("ArrowDown", 108, 0x7d, 0xe050),
    key("ArrowLeft", 105, 0x7b, 0xe04b),
    key("ArrowRight", 106, 0x7c, 0xe04d),
    modifier("ShiftLeft", 42, 0x38, 0x2a, Modifier::Shift),
    modifier("ShiftRight", 54, 0x3c, 0x36, Modifier::Shift),
    modifier("ControlLeft", 29, 0x3b, 0x1d, Modifier::Control),
    modifier("ControlRight", 97, 0x3e, 0xe01d, Modifier::Control),
    modifier("AltLeft", 56, 0x3a, 0x38, Modifier::Alt),
    modifier("AltRight", 100, 0x3d, 0xe038, Modifier::Alt),
    modifier("MetaLeft", 125, 0x37, 0xe05b, Modifier::Meta),
    modifier("MetaRight", 126, 0x36, 0xe05c, Modifier::Meta),
    key("ContextMenu", 127, 0x6e, 0xe05d),
    key("F1", 59, 0x7a, 0x3b),
    key("F2", 60, 0x78, 0x3c),
    key("F3", 61, 0x63, 0x3d),
    key("F4", 62, 0x76, 0x3e),
    key("F5", 63, 0x60, 0x3f),
    key("F6", 64, 0x61, 0x40),
    key("F7", 65, 0x62, 0x41),
    key("F8", 66, 0x64, 0x42),
    key("F9", 67, 0x65, 0x43),
    key("F10", 68, 0x6d, 0x44),
    key("F11", 87, 0x67, 0x57),
    key("F12", 88, 0x6f, 0x58),
    key("F13", 183, 0x69, 0x64),
    key("F14", 184, 0x6b, 0x65),
    key("F15", 185, 0x71, 0x66),
    key("F16", 186, 0x6a, 0x67),
    key("F17", 187, 0x40, 0x68),
    key("F18", 188, 0x4f, 0x69),
    key("F19", 189, 0x50, 0x6a),
    key("F20", 190, 0x5a, 0x6b),
    key("PrintScreen", 99, 0x69, 0xe037),
    key("ScrollLock", 70, 0x6b, 0x46),
    key("Pause", 119, 0x71, 0x0045),
    key("NumLock", 69, 0x47, 0xe045),
    key("Numpad0", 82, 0x52, 0x52),
    key("Numpad1", 79, 0x53, 0x4f),
    key("Numpad2", 80, 0x54, 0x50),
    key("Numpad3", 81, 0x55, 0x51),
    key("Numpad4", 75, 0x56, 0x4b),
    key("Numpad5", 76, 0x57, 0x4c),
    key("Numpad6", 77, 0x58, 0x4d),
    key("Numpad7", 71, 0x59, 0x47),
    key("Numpad8", 72, 0x5b, 0x48),
    key("Numpad9", 73, 0x5c, 0x49),
    key("NumpadAdd", 78, 0x45, 0x4e),
    key("NumpadSubtract", 74, 0x4e, 0x4a),
    key("NumpadMultiply", 55, 0x43, 0x37),
    key("NumpadDivide", 98, 0x4b, 0xe035),
    key("NumpadDecimal", 83, 0x41, 0x53),
    key("NumpadEnter", 96, 0x4c, 0xe01c),
    key("NumpadEqual", 117, 0x51, 0x59),
];

/// The key for a code, or None for codes Link does not press (`Lang1`, `Fn`, media keys):
/// those are skipped, not refused (#2909).
pub fn physical_key(code: &str) -> Option<&'static PhysicalKey> {
    KEYS.iter().find(|key| key.code == code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The codes RealTimeX sends (#2909 design §1, Link Dev's list of 2026-10-28).
    const AGREED: &[&str] = &[
        "Backquote",
        "Minus",
        "Equal",
        "BracketLeft",
        "BracketRight",
        "Backslash",
        "Semicolon",
        "Quote",
        "Comma",
        "Period",
        "Slash",
        "IntlBackslash",
        "IntlRo",
        "IntlYen",
        "Space",
        "Enter",
        "Tab",
        "Escape",
        "Backspace",
        "Delete",
        "Insert",
        "CapsLock",
        "Home",
        "End",
        "PageUp",
        "PageDown",
        "ArrowUp",
        "ArrowDown",
        "ArrowLeft",
        "ArrowRight",
        "ShiftLeft",
        "ShiftRight",
        "ControlLeft",
        "ControlRight",
        "AltLeft",
        "AltRight",
        "MetaLeft",
        "MetaRight",
        "ContextMenu",
        "PrintScreen",
        "ScrollLock",
        "Pause",
        "NumLock",
        "NumpadAdd",
        "NumpadSubtract",
        "NumpadMultiply",
        "NumpadDivide",
        "NumpadDecimal",
        "NumpadEnter",
        "NumpadEqual",
    ];

    fn agreed() -> Vec<String> {
        let mut codes: Vec<String> = AGREED.iter().map(|c| c.to_string()).collect();
        codes.extend(('A'..='Z').map(|c| format!("Key{c}")));
        codes.extend((0..=9).map(|n| format!("Digit{n}")));
        codes.extend((0..=9).map(|n| format!("Numpad{n}")));
        codes.extend((1..=20).map(|n| format!("F{n}")));
        codes
    }

    #[test]
    fn every_agreed_code_has_a_key_on_each_platform() {
        let codes = agreed();
        for code in &codes {
            let key = physical_key(code).unwrap_or_else(|| panic!("{code} has no key"));
            assert!(
                key.evdev > 0 && key.evdev + 8 <= 255,
                "{code}: XTest keycode out of range"
            );
            assert!(key.mac <= 0x7f, "{code}: no macOS key");
            assert!(key.windows & 0xff != 0, "{code}: no Windows scancode");
            assert!(
                matches!(key.windows >> 8, 0 | 0xe0),
                "{code}: bad Windows prefix"
            );
        }
        assert_eq!(
            KEYS.len(),
            codes.len(),
            "the table has codes nobody agreed to"
        );
    }

    /// Two codes on one native key would type the same thing for different keys. PrintScreen,
    /// ScrollLock and Pause are F13–F15 on a Mac, as a PC keyboard's are.
    #[test]
    fn each_platform_has_one_native_key_per_code() {
        let unique =
            |native: fn(&PhysicalKey) -> u16| KEYS.iter().map(native).collect::<HashSet<_>>().len();
        assert_eq!(unique(|k| k.evdev), KEYS.len());
        assert_eq!(unique(|k| k.windows), KEYS.len());
        assert_eq!(unique(|k| k.mac), KEYS.len() - 3);
        for (pc, mac) in [
            ("PrintScreen", "F13"),
            ("ScrollLock", "F14"),
            ("Pause", "F15"),
        ] {
            assert_eq!(
                physical_key(pc).unwrap().mac,
                physical_key(mac).unwrap().mac
            );
        }
    }

    /// The main block's evdev codes are its set-1 scancodes; a slip in one column shows here.
    #[test]
    fn evdev_and_windows_agree_on_the_main_block() {
        for key in KEYS.iter().filter(|k| k.windows < 0x59 && k.evdev < 89) {
            if key.code == "Pause" {
                continue;
            }
            assert_eq!(key.evdev, key.windows, "{}", key.code);
        }
    }

    #[test]
    fn modifiers_and_unknown_codes() {
        assert_eq!(
            physical_key("MetaLeft").unwrap().modifier,
            Some(Modifier::Meta)
        );
        assert_eq!(
            physical_key("ShiftRight").unwrap().modifier,
            Some(Modifier::Shift)
        );
        assert_eq!(physical_key("KeyA").unwrap().modifier, None);
        assert_eq!(KEYS.iter().filter(|k| k.modifier.is_some()).count(), 8);
        for code in ["Lang1", "Fn", "MediaPlayPause", "", "keya", "Unidentified"] {
            assert_eq!(physical_key(code), None, "{code}");
        }
    }
}
