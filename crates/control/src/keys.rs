//! Keys for Remote Control. Requests name keys by X11 keysym (what GNOME's remote desktop
//! and VNC use); each platform maps a [`Key`] to its own key codes.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A printable character (lowercase letters for shortcuts such as ctrl+l).
    Char(char),
    Return,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Up,
    Right,
    Down,
    Control,
    Shift,
    Alt,
    /// Command on macOS, the Windows key elsewhere.
    Super,
    Menu,
    PrintScreen,
    /// F1..=F12.
    F(u8),
}

impl Key {
    /// macOS keeps its own modifier flags for the events it posts.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn is_modifier(self) -> bool {
        matches!(self, Key::Control | Key::Shift | Key::Alt | Key::Super)
    }
}

/// The key for an X11 keysym, or None for keysyms Remote Control does not send.
pub fn from_keysym(keysym: u32) -> Option<Key> {
    Some(match keysym {
        0xff0d => Key::Return,
        0xff09 => Key::Tab,
        0xff1b => Key::Escape,
        0xff08 => Key::Backspace,
        0xffff => Key::Delete,
        0xff63 => Key::Insert,
        0xff50 => Key::Home,
        0xff57 => Key::End,
        0xff55 => Key::PageUp,
        0xff56 => Key::PageDown,
        0xff51 => Key::Left,
        0xff52 => Key::Up,
        0xff53 => Key::Right,
        0xff54 => Key::Down,
        0xffe3 | 0xffe4 => Key::Control,
        0xffe1 | 0xffe2 => Key::Shift,
        0xffe9 | 0xffea => Key::Alt,
        0xffeb | 0xffec | 0xffe7 | 0xffe8 => Key::Super,
        0xff67 => Key::Menu,
        0xff61 => Key::PrintScreen,
        0xffbe..=0xffc9 => Key::F((keysym - 0xffbe + 1) as u8),
        0x20..=0x7e | 0xa0..=0xff => Key::Char(char::from_u32(keysym)?),
        0x0100_0000..=0x0110_ffff => Key::Char(char::from_u32(keysym - 0x0100_0000)?),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_named_keys_function_keys_and_characters() {
        assert_eq!(from_keysym(0xff0d), Some(Key::Return));
        assert_eq!(from_keysym(0xffe3), Some(Key::Control));
        assert_eq!(from_keysym(0xffeb), Some(Key::Super));
        assert_eq!(from_keysym(0xffbe), Some(Key::F(1)));
        assert_eq!(from_keysym(0xffc9), Some(Key::F(12)));
        assert_eq!(from_keysym(0x6c), Some(Key::Char('l')));
        assert_eq!(from_keysym(0xe9), Some(Key::Char('é')));
        assert_eq!(from_keysym(0x0100_1ec7), Some(Key::Char('ệ')));
        assert_eq!(from_keysym(0x1234), None);
        assert!(Key::Shift.is_modifier());
        assert!(!Key::Char('a').is_modifier());
    }
}
