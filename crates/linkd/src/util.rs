//! Small shared pieces: timestamps as JavaScript writes them, random ids, hashes, and the
//! programs this daemon starts.

use std::ffi::OsStr;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// "2026-10-23T05:12:34.567Z", as Date#toISOString.
pub fn iso(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn iso_now() -> String {
    iso(Utc::now())
}

pub fn parse_iso(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

pub fn base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn unbase64(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text.trim()).ok()
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    !a.is_empty() && a.len() == b.len() && bool::from(a.ct_eq(b))
}

pub fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Windows: CREATE_NO_WINDOW. rtx-linkd has no console (rtx-link starts it with this flag), so a
/// console program it starts without the flag gets a console window of its own, on screen for
/// as long as it runs (rtx-control: the whole session).
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Every program this daemon starts begins here (or in `tokio_command`): no console window on
/// Windows. The test below keeps it that way.
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// `command`, for tokio.
pub fn tokio_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    #[allow(unused_mut)]
    let mut command = tokio::process::Command::new(program);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program started any other way than `command` / `tokio_command` would open a console
    /// window on Windows: none may, anywhere in this crate.
    #[test]
    fn every_program_starts_without_a_console_window() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            if path.extension() != Some(OsStr::new("rs")) || path.ends_with("util.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (n, line) in text.lines().enumerate() {
                if line.contains("Command::new(") {
                    offenders.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
        assert!(offenders.is_empty(), "use util::command: {offenders:?}");
    }

    #[test]
    fn timestamps_round_trip_as_javascript_writes_them() {
        let at = parse_iso("2026-10-01T00:00:00.000Z").unwrap();
        assert_eq!(iso(at), "2026-10-01T00:00:00.000Z");
        assert_eq!(iso_now().len(), 24);
    }

    #[test]
    fn hashes_match_node() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(same_bytes(b"x", b"x"));
        assert!(!same_bytes(b"", b""));
    }
}
