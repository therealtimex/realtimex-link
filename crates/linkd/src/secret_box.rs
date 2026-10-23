//! The daemon's encryption for secrets at rest (the password), with its own key in secret.key.
//! A sealed value is { v, iv, tag, data } in base64 (AES-256-GCM); the password is kept as that
//! object's JSON text (`ciphertext`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::files;
use crate::util::{base64, random_bytes, unbase64};

pub const KEY_FILE: &str = "secret.key";
const VERSION: u64 = 1;
const TAG_BYTES: usize = 16;

pub struct SecretBox {
    data_dir: PathBuf,
    key: OnceLock<[u8; 32]>,
}

impl SecretBox {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            key: OnceLock::new(),
        }
    }

    fn key(&self) -> Result<[u8; 32]> {
        if let Some(key) = self.key.get() {
            return Ok(*key);
        }
        files::ensure_private_dir(&self.data_dir)?;
        let file = self.data_dir.join(KEY_FILE);
        let fresh: [u8; 32] = random_bytes();
        let key = match files::create_new(&file, &base64(&fresh)) {
            Ok(()) => fresh,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let saved = unbase64(&fs::read_to_string(&file)?).unwrap_or_default();
                let Ok(key) = <[u8; 32]>::try_from(saved.as_slice()) else {
                    bail!("{KEY_FILE} is not a 256-bit key");
                };
                key
            }
            Err(error) => return Err(error.into()),
        };
        Ok(*self.key.get_or_init(|| key))
    }

    pub fn seal(&self, text: &str) -> Result<Value> {
        let key = self.key()?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let iv: [u8; 12] = random_bytes();
        let Ok(mut sealed) = cipher.encrypt(Nonce::from_slice(&iv), text.as_bytes()) else {
            bail!("Could not encrypt");
        };
        // aes-gcm appends the tag; the sealed format keeps it apart.
        let tag = sealed.split_off(sealed.len() - TAG_BYTES);
        Ok(json!({ "v": VERSION, "iv": base64(&iv), "tag": base64(&tag), "data": base64(&sealed) }))
    }

    /// The text, or None when it can't be opened (another key, tampered, not sealed).
    pub fn open(&self, sealed: &Value) -> Option<String> {
        if sealed.get("v").and_then(Value::as_u64) != Some(VERSION) {
            return None;
        }
        let field = |name: &str| sealed.get(name).and_then(Value::as_str).and_then(unbase64);
        let (iv, tag, mut data) = (field("iv")?, field("tag")?, field("data")?);
        if iv.len() != 12 || tag.len() != TAG_BYTES {
            return None;
        }
        let key = self.key().ok()?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        data.extend_from_slice(&tag);
        let plain = cipher
            .decrypt(Nonce::from_slice(&iv), data.as_slice())
            .ok()?;
        String::from_utf8(plain).ok()
    }

    /// HostService's `encryption.encrypt`: the sealed value as JSON text.
    pub fn encrypt(&self, text: &str) -> Result<String> {
        Ok(self.seal(text)?.to_string())
    }

    /// HostService's `encryption.decrypt`.
    pub fn decrypt(&self, ciphertext: &str) -> Option<String> {
        self.open(&serde_json::from_str(ciphertext).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_with_its_own_key_and_opens_only_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let sealed = SecretBox::new(dir.path()).encrypt("pass-1234").unwrap();
        assert!(!sealed.contains("pass-1234"));
        // A fresh box on the same folder reads the saved key.
        assert_eq!(
            SecretBox::new(dir.path()).decrypt(&sealed).as_deref(),
            Some("pass-1234")
        );
        let other = tempfile::tempdir().unwrap();
        assert_eq!(SecretBox::new(other.path()).decrypt(&sealed), None);
        assert_eq!(SecretBox::new(dir.path()).decrypt("not json"), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn opens_what_node_sealed() {
        // Sealed by Node with this key (crypto.createCipheriv("aes-256-gcm")): the format is the same.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(KEY_FILE),
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
        )
        .unwrap();
        let sealed = json!({
            "v": 1,
            "iv": "AAAAAAAAAAAAAAAA",
            "tag": "zjLnfEp25j37YPsMPWDqVg==",
            "data": "ZtnZstoAo8pn2sVR",
        });
        assert_eq!(
            SecretBox::new(dir.path()).open(&sealed).as_deref(),
            Some("hello, world")
        );
    }
}
