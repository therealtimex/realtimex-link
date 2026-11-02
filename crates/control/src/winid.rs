//! Window ids (realtimex-ai-app#3055): opaque strings to clients, made and read here only, by
//! rtx-control and rtx-linkd (which includes this file). A prefix says which system it is from,
//! so ids from another one (Phase 2's `a:` ids) never mix with these:
//!
//! | System  | Id                    | Example      |
//! |---------|-----------------------|--------------|
//! | macOS   | CGWindowID, decimal   | `m:4211`     |
//! | Windows | HWND, hex             | `w:2a0c46`   |
//! | X11     | XID, hex              | `x:3a00007`  |

/// A window's id on one system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowId {
    Mac(u32),
    Win(u64),
    X11(u32),
}

impl WindowId {
    /// The id `text` is, or None for anything else (another prefix, a malformed number).
    pub fn parse(text: &str) -> Option<WindowId> {
        let (prefix, number) = text.split_once(':')?;
        let digits = |radix: u32| {
            !number.is_empty() && number.len() <= 16 && number.chars().all(|c| c.is_digit(radix))
        };
        match prefix {
            "m" if digits(10) => number.parse().ok().map(WindowId::Mac),
            "w" if digits(16) => u64::from_str_radix(number, 16).ok().map(WindowId::Win),
            "x" if digits(16) => u32::from_str_radix(number, 16).ok().map(WindowId::X11),
            _ => None,
        }
    }
}

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WindowId::Mac(id) => write!(f, "m:{id}"),
            WindowId::Win(hwnd) => write!(f, "w:{hwnd:x}"),
            WindowId::X11(xid) => write!(f, "x:{xid:x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WindowId;

    #[test]
    fn window_ids_read_back_what_was_written() {
        for id in [
            WindowId::Mac(4211),
            WindowId::Win(0x2a0c46),
            WindowId::Win(u64::MAX),
            WindowId::X11(0x3a00007),
            WindowId::Mac(0),
        ] {
            assert_eq!(WindowId::parse(&id.to_string()), Some(id), "{id}");
        }
        assert_eq!(WindowId::Mac(4211).to_string(), "m:4211");
        assert_eq!(WindowId::Win(0x2a0c46).to_string(), "w:2a0c46");
        assert_eq!(WindowId::X11(0x3a00007).to_string(), "x:3a00007");
        assert_eq!(WindowId::parse("w:2A0C46"), Some(WindowId::Win(0x2a0c46)));
    }

    #[test]
    fn anything_else_is_no_window_id() {
        for text in [
            "",
            "m",
            "m:",
            "4211",
            "a:12",
            "m:12a",
            "m:+12",
            "m:-1",
            "w:0x2a",
            "w:zz",
            "x:",
            "m:99999999999",
            "x:123456789",
            "w:12345678901234567",
            " m:1",
            "m:1 ",
        ] {
            assert_eq!(WindowId::parse(text), None, "{text:?}");
        }
    }
}
