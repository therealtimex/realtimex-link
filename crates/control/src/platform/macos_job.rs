//! Input for the login-window job (realtimex-link#11, Addendum 5): what a request the daemon
//! marked `via:job` becomes. The same key table, held-key rules and mapping as CGEventPost
//! (I2, I5) give the key code; this adds the character codes and the modifier sequence
//! (Addendum 4) and sends the datagrams on the channel the job made (`--input-fd`). With no
//! channel, a `via:job` request fails: nothing falls back to CGEventPost under Secure Input
//! (C4). Nothing typed is logged (I7).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;

use crate::jobinput::Event;

static CHANNEL: OnceLock<OwnedFd> = OnceLock::new();

/// Take the job's channel (fd 3 from the bridge). It never goes to this process's children
/// (C1): close-on-exec.
pub fn set_channel(fd: i32) {
    // SAFETY: the fd the bridge handed this process, owned from here on.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fcntl and setsockopt on that fd. A 2 s send timeout bounds a slow job (I1).
    unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        let timeout = libc::timeval { tv_sec: 2, tv_usec: 0 };
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            (&timeout as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }
    let _ = CHANNEL.set(fd);
}

fn unavailable(message: &str) -> anyhow::Error {
    super::super::InputError { code: "input-unavailable", message: message.to_string() }.into()
}

/// Send the events, in order.
pub fn send(events: &[Event]) -> anyhow::Result<()> {
    let channel = CHANNEL
        .get()
        .ok_or_else(|| unavailable("Input at the login window isn't available in this session"))?;
    for event in events {
        let bytes = event.encode();
        // SAFETY: one datagram on a SOCK_SEQPACKET socket this process owns.
        let sent = unsafe { libc::send(channel.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) };
        if sent != bytes.len() as isize {
            return Err(unavailable("The login window's input didn't answer"));
        }
    }
    Ok(())
}

/// NX Shift bit, the same value as kCGEventFlagMaskShift.
pub const SHIFT: u32 = 0x0002_0000;
/// The left Shift key (kVK_Shift).
pub const SHIFT_KEY: u16 = 0x38;

/// The character code that goes with a key that types none of its own (Addendum 4): Return,
/// keypad Enter, Tab, Delete (Backspace) and Escape. Others (arrows, function keys) need only
/// their key code.
pub fn special_char_code(code: u16) -> u16 {
    match code {
        0x24 => 0x0d,
        0x4c => 0x03,
        0x30 => 0x09,
        0x33 => 0x7f,
        0x35 => 0x1b,
        _ => 0,
    }
}

/// A key's events for the job: a character's code from its own value when it is printable,
/// else from the special table. A character that needs Shift and isn't part of a shortcut gets
/// Shift around it as a modifier change (`held` is the mask without it), as the
/// TextEdit check showed CGEvent-style flags alone lose it.
pub fn key_events(code: u16, character: Option<char>, needs_shift: bool, held: u32, down: bool) -> Vec<Event> {
    let char_code = character
        .filter(|c| (*c as u32) < 0x1_0000 && !c.is_control())
        .map_or_else(|| special_char_code(code), |c| c as u16);
    let shifted = needs_shift && held & SHIFT == 0;
    let flags = if shifted { held | SHIFT } else { held };
    let key = Event::Key { code, char_code, orig_char_code: char_code, down, flags };
    match (shifted, down) {
        (false, _) => vec![key],
        (true, true) => vec![Event::Flags { code: SHIFT_KEY, mask: flags }, key],
        (true, false) => vec![key, Event::Flags { code: SHIFT_KEY, mask: held }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_that_type_nothing_still_carry_their_codes() {
        for code in [0x24u16, 0x30, 0x33] {
            assert_ne!(special_char_code(code), 0, "{code:#x}");
        }
        match key_events(0x24, None, false, 0, true)[..] {
            [Event::Key { char_code: 0x0d, orig_char_code: 0x0d, .. }] => {}
            ref other => panic!("{other:?}"),
        }
        match key_events(0x00, Some('a'), false, 0, true)[..] {
            [Event::Key { code: 0x00, char_code: 97, .. }] => {}
            ref other => panic!("{other:?}"),
        }
    }

    /// The TextEdit check (Addendum 4): each shifted character is Shift down as a flags change,
    /// the key with Shift in its mask, then Shift up; unshifted ones carry no Shift.
    #[test]
    fn shift_goes_around_each_character_that_needs_it() {
        let text = "Ab1!xZ?_+{}|:\"<>~ @#$%^&*()";
        for c in text.chars() {
            let (code, shift) = super::super::char_key(c).expect("every character has a US key");
            let down = key_events(code, Some(c), shift, 0, true);
            let up = key_events(code, Some(c), shift, 0, false);
            if shift {
                assert_eq!(down[0], Event::Flags { code: SHIFT_KEY, mask: SHIFT }, "{c}");
                assert!(matches!(down[1], Event::Key { down: true, flags: SHIFT, .. }), "{c}");
                assert!(matches!(up[0], Event::Key { down: false, flags: SHIFT, .. }), "{c}");
                assert_eq!(up[1], Event::Flags { code: SHIFT_KEY, mask: 0 }, "{c}");
            } else {
                assert!(matches!(down[..], [Event::Key { flags: 0, .. }]), "{c}");
            }
        }
        // Shift already held (a shortcut): no extra change.
        assert_eq!(key_events(0x00, Some('A'), true, SHIFT, true).len(), 1);
    }

    /// I5: the job's key codes are the CGEvent path's (one table).
    #[test]
    fn both_paths_use_the_same_key_codes() {
        for c in ['a', 'Z', '1', '!', ' ', '/'] {
            let (code, _) = super::super::char_key(c).unwrap();
            match key_events(code, Some(c), false, 0, true).last() {
                Some(Event::Key { code: sent, .. }) => assert_eq!(*sent, code, "{c}"),
                other => panic!("{other:?}"),
            }
        }
    }
}
