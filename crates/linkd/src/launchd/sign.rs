//! Signatures (#11 M2, M4): this running code's own team, from the kernel's view of it
//! (`SecCodeCopySelf`, never a file, a store or the environment), a peer's by its audit token,
//! and a bundle on disk (codesign and spctl, as update.rs).

/// Link's main app and its daemon: the only peers the supervisor answers.
pub const PEER_IDENTIFIERS: [&str; 2] = ["ai.realtimex.link", "ai.realtimex.link.rtx-linkd"];
/// The root copy must be the main app itself.
pub const APP_IDENTIFIER: &str = "ai.realtimex.link";

/// The requirement for our team's Developer ID code with one of `identifiers`.
pub fn requirement(team: &str, identifiers: &[&str]) -> Option<String> {
    let signed = crate::update::codesign_requirement(team)?;
    let names: Vec<String> = identifiers
        .iter()
        .map(|name| format!("identifier \"{name}\""))
        .collect();
    Some(format!("{signed} and ({})", names.join(" or ")))
}

#[cfg(target_os = "macos")]
pub use platform::*;

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::path::Path;
    use std::process::Stdio;
    use std::sync::OnceLock;

    type CFTypeRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    const SIGNING_INFORMATION: u32 = 1 << 1;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(object: CFTypeRef);
        fn CFStringCreateWithCString(allocator: CFTypeRef, text: *const c_char, encoding: u32) -> CFTypeRef;
        fn CFStringGetCString(text: CFTypeRef, buffer: *mut c_char, size: isize, encoding: u32) -> bool;
        fn CFDataCreate(allocator: CFTypeRef, bytes: *const u8, length: isize) -> CFTypeRef;
        fn CFDictionaryCreate(
            allocator: CFTypeRef,
            keys: *const CFTypeRef,
            values: *const CFTypeRef,
            count: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFTypeRef;
        fn CFDictionaryGetValue(dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        fn CFGetTypeID(object: CFTypeRef) -> usize;
        fn CFStringGetTypeID() -> usize;
        static kCFTypeDictionaryKeyCallBacks: [usize; 6];
        static kCFTypeDictionaryValueCallBacks: [usize; 5];
    }

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        fn SecCodeCopySelf(flags: u32, code: *mut CFTypeRef) -> i32;
        fn SecCodeCopySigningInformation(code: CFTypeRef, flags: u32, information: *mut CFTypeRef) -> i32;
        fn SecCodeCopyGuestWithAttributes(
            host: CFTypeRef,
            attributes: CFTypeRef,
            flags: u32,
            guest: *mut CFTypeRef,
        ) -> i32;
        fn SecRequirementCreateWithString(text: CFTypeRef, flags: u32, requirement: *mut CFTypeRef) -> i32;
        fn SecCodeCheckValidity(code: CFTypeRef, flags: u32, requirement: CFTypeRef) -> i32;
        static kSecCodeInfoTeamIdentifier: CFTypeRef;
        static kSecGuestAttributeAudit: CFTypeRef;
    }

    /// A CF object released on drop.
    struct Owned(CFTypeRef);

    impl Owned {
        fn new(object: CFTypeRef) -> Option<Owned> {
            (!object.is_null()).then_some(Owned(object))
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: an object this module created or copied, released once.
            unsafe { CFRelease(self.0) };
        }
    }

    fn cf_string(text: &str) -> Option<Owned> {
        let text = CString::new(text).ok()?;
        // SAFETY: a NUL-terminated UTF-8 string.
        Owned::new(unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) })
    }

    fn rust_string(text: CFTypeRef) -> Option<String> {
        // SAFETY: a type check before reading it as a string; the buffer's size is passed.
        unsafe {
            if text.is_null() || CFGetTypeID(text) != CFStringGetTypeID() {
                return None;
            }
            let mut buffer = [0 as c_char; 256];
            CFStringGetCString(text, buffer.as_mut_ptr(), buffer.len() as isize, UTF8)
                .then(|| CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned())
        }
    }

    /// The Team ID this running process is signed with, when it is a Developer ID-shaped one
    /// (ten letters and digits): None for an unsigned or ad-hoc build.
    pub fn own_team() -> Option<String> {
        static TEAM: OnceLock<Option<String>> = OnceLock::new();
        TEAM.get_or_init(|| {
            let mut code: CFTypeRef = std::ptr::null();
            // SAFETY: out-pointers to locals; every copied object is released (Owned).
            unsafe {
                if SecCodeCopySelf(0, &mut code) != 0 {
                    return None;
                }
                let code = Owned::new(code)?;
                let mut information: CFTypeRef = std::ptr::null();
                if SecCodeCopySigningInformation(code.0, SIGNING_INFORMATION, &mut information) != 0 {
                    return None;
                }
                let information = Owned::new(information)?;
                let team = rust_string(CFDictionaryGetValue(information.0, kSecCodeInfoTeamIdentifier))?;
                crate::update::codesign_requirement(&team).map(|_| team)
            }
        })
        .clone()
    }

    /// Whether the process behind a socket's audit token satisfies `requirement` (M4).
    pub fn peer_satisfies(audit_token: &[u8; 32], requirement: &str) -> bool {
        // SAFETY: CF objects made here and released on drop; the token's bytes are copied.
        unsafe {
            let Some(token) = Owned::new(CFDataCreate(std::ptr::null(), audit_token.as_ptr(), 32)) else {
                return false;
            };
            let keys = [kSecGuestAttributeAudit];
            let values = [token.0];
            let Some(attributes) = Owned::new(CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            )) else {
                return false;
            };
            let mut guest: CFTypeRef = std::ptr::null();
            if SecCodeCopyGuestWithAttributes(std::ptr::null(), attributes.0, 0, &mut guest) != 0 {
                return false;
            }
            let Some(guest) = Owned::new(guest) else { return false };
            let Some(text) = cf_string(requirement) else { return false };
            let mut compiled: CFTypeRef = std::ptr::null();
            if SecRequirementCreateWithString(text.0, 0, &mut compiled) != 0 {
                return false;
            }
            let Some(compiled) = Owned::new(compiled) else { return false };
            SecCodeCheckValidity(guest.0, 0, compiled.0) == 0
        }
    }

    /// A bundle on disk is our team's, sealed, and accepted by Gatekeeper (M2: the staged copy,
    /// never the source).
    pub fn bundle_is_ours(app: &Path, team: &str) -> bool {
        let Some(requirement) = super::requirement(team, &[super::APP_IDENTIFIER]) else {
            return false;
        };
        let verified = std::process::Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict", "-R"])
            .arg(format!("={requirement}"))
            .arg(app)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        verified
            && std::process::Command::new("/usr/sbin/spctl")
                .args(["--assess", "--type", "execute"])
                .arg(app)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
    }
}

/// Elsewhere (the tests on Linux): never signed.
#[cfg(not(target_os = "macos"))]
pub fn own_team() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn bundle_is_ours(_app: &std::path::Path, _team: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_must_be_our_teams_app_or_daemon() {
        let text = requirement("ABCDE12345", &PEER_IDENTIFIERS).unwrap();
        assert!(text.starts_with("anchor apple generic and "));
        assert!(text.contains("certificate leaf[subject.OU] = \"ABCDE12345\""));
        assert!(text.ends_with(
            " and (identifier \"ai.realtimex.link\" or identifier \"ai.realtimex.link.rtx-linkd\")"
        ));
        assert!(requirement("not-a-team", &PEER_IDENTIFIERS).is_none());
    }
}
