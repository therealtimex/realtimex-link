//! This computer's password: made readable, checked in constant time.

use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::util::{random_bytes, same_bytes};

// No look-alike characters (0/O, 1/l/I), so a password read off the screen types right.
const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
pub const MIN_PASSWORD_LENGTH: usize = 8;
pub const MAX_PASSWORD_LENGTH: usize = 128;

/// A readable random password such as "k7mp-x2qa-9tvd" (about 60 bits).
pub fn generate() -> String {
    let bytes: [u8; 12] = random_bytes();
    let chars: Vec<char> = bytes
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect();
    chars
        .chunks(4)
        .map(|group| group.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// JavaScript counts UTF-16 code units; so does this.
pub fn validate(password: &str) -> Result<String, ApiError> {
    let length = password.encode_utf16().count();
    if !(MIN_PASSWORD_LENGTH..=MAX_PASSWORD_LENGTH).contains(&length) {
        return Err(ApiError::bad_request(format!(
            "Password must be {MIN_PASSWORD_LENGTH} to {MAX_PASSWORD_LENGTH} characters"
        )));
    }
    Ok(password.to_string())
}

/// Compared as SHA-256 digests in constant time; an unset password matches nothing.
pub fn same(expected: &str, supplied: &str) -> bool {
    !expected.is_empty()
        && same_bytes(
            &Sha256::digest(expected.as_bytes()),
            &Sha256::digest(supplied.as_bytes()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_and_checked() {
        let password = generate();
        assert_eq!(password.len(), 14);
        assert!(password
            .chars()
            .all(|c| c == '-' || ALPHABET.contains(&(c as u8))));
        assert!(same(&password, &password));
        assert!(!same(&password, "nope"));
        assert!(!same("", ""));
        assert!(validate("short").is_err());
        assert!(validate("long-enough").is_ok());
    }
}
