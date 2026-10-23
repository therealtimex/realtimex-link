//! Transfers from a viewer to this computer (#2682): a pasted
//! image, or a file for Downloads, in chunks: `xfer_begin`, `xfer_chunk`…, `xfer_end` (or
//! `xfer_cancel`). Each chunk is written to a temporary file as it arrives. `paste_file`s wait in
//! their group until `xfer_paste` pastes them (the router does the click and the paste).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::util::{unbase64, uuid};

pub const MAX_IMAGE_BYTES: u64 = 15 * 1024 * 1024;
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Raw bytes per chunk: base64 and the JSON around it stay under the 64 KiB message limit.
pub const MAX_CHUNK_BYTES: usize = 40 * 1024;
const MAX_OPEN_PER_SESSION: usize = 8;
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
const MAX_FILES_PER_PASTE: usize = 100;
const MAX_NAME_TRIES: u32 = 10_000;
/// Pasted files stay this long: the app they were pasted into made its own copy by then.
const PASTED_FILES_KEEP: Duration = Duration::from_secs(24 * 60 * 60);

struct Open {
    session: String,
    purpose: String,
    group: Option<String>,
    name: String,
    size: u64,
    received: u64,
    path: PathBuf,
    file: File,
    hash: Sha256,
}

struct Group {
    session: String,
    dir: PathBuf,
    paths: Vec<PathBuf>,
    size: u64,
}

/// What `xfer_end` finished.
pub enum Ended {
    /// A PNG to paste now.
    Image(Vec<u8>),
    /// A file kept for its group's paste.
    Stored,
    /// A file saved to Downloads.
    Saved {
        name: String,
        folder: PathBuf,
        size: u64,
    },
}

/// A group's files, taken for `xfer_paste`.
pub struct PasteGroup {
    pub paths: Vec<PathBuf>,
    pub size: u64,
}

pub struct Transfers {
    open: Mutex<HashMap<String, Open>>,
    groups: Mutex<HashMap<String, Group>>,
    temp_dir: PathBuf,
    downloads: PathBuf,
}

fn err(status: u16, message: impl Into<String>) -> ApiError {
    ApiError::new(status, message)
}

fn id_ok(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A file name safe to create in a folder: no folders, no reserved characters, not empty.
pub fn safe_name(name: &str) -> String {
    let unified = name.replace('\\', "/");
    let base = unified
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    let replaced: String = base
        .chars()
        .map(|c| {
            if (c as u32) < 0x20 || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned: String = replaced
        .trim()
        .trim_start_matches('.')
        .chars()
        .take(200)
        .collect();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned
    }
}

fn text<'a>(message: &'a Value, key: &str) -> &'a str {
    message.get(key).and_then(Value::as_str).unwrap_or("")
}

impl Transfers {
    pub fn new(temp_dir: PathBuf, downloads: PathBuf) -> Self {
        Self {
            open: Mutex::new(HashMap::new()),
            groups: Mutex::new(HashMap::new()),
            temp_dir,
            downloads,
        }
    }

    pub fn default_dirs() -> (PathBuf, PathBuf) {
        let home = PathBuf::from(crate::paths::home_dir());
        (
            std::env::temp_dir().join("realtimex-remote-transfers"),
            home.join("Downloads"),
        )
    }

    fn open(&self) -> std::sync::MutexGuard<'_, HashMap<String, Open>> {
        self.open.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn groups(&self) -> std::sync::MutexGuard<'_, HashMap<String, Group>> {
        self.groups.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn handles(kind: &str) -> bool {
        kind.starts_with("xfer_")
    }

    fn discard(&self, key: &str) {
        if let Some(entry) = self.open().remove(key) {
            drop(entry.file);
            let _ = fs::remove_file(entry.path);
        }
    }

    pub fn begin(&self, session: &str, message: &Value) -> Result<Value, ApiError> {
        let xfer = text(message, "xfer");
        let purpose = text(message, "purpose");
        if !id_ok(xfer) {
            return Err(err(400, "A transfer id is required"));
        }
        if !["paste_image", "file", "paste_file"].contains(&purpose) {
            return Err(err(400, "Unknown transfer"));
        }
        let Some(size) = message
            .get("size")
            .and_then(Value::as_u64)
            .filter(|s| *s <= (1u64 << 53))
        else {
            return Err(err(400, "A size is required"));
        };
        let limit = if purpose == "paste_image" {
            MAX_IMAGE_BYTES
        } else {
            MAX_FILE_BYTES
        };
        if size > limit {
            return Err(ApiError::coded(
                413,
                "too-large",
                format!("Too large: {size} bytes, at most {limit}"),
            ));
        }
        let group = (purpose == "paste_file").then(|| text(message, "group").to_string());
        if group.as_deref().is_some_and(|g| !id_ok(g)) {
            return Err(err(400, "A group is required"));
        }
        let key = format!("{session}:{xfer}");
        let mut open = self.open();
        if open.contains_key(&key) {
            return Err(err(409, "That transfer is already open"));
        }
        if open
            .values()
            .filter(|entry| entry.session == session)
            .count()
            >= MAX_OPEN_PER_SESSION
        {
            return Err(ApiError::coded(
                429,
                "rate-limited",
                "Too many transfers at once",
            ));
        }
        fs::create_dir_all(&self.temp_dir).map_err(ApiError::internal)?;
        let path = self
            .temp_dir
            .join(format!("{}-{}.part", std::process::id(), uuid()));
        let file = create_private(&path).map_err(ApiError::internal)?;
        open.insert(
            key,
            Open {
                session: session.to_string(),
                purpose: purpose.to_string(),
                group,
                name: safe_name(text(message, "name")),
                size,
                received: 0,
                path,
                file,
                hash: Sha256::new(),
            },
        );
        Ok(json!({}))
    }

    pub fn chunk(&self, session: &str, message: &Value) -> Result<Value, ApiError> {
        let key = format!("{session}:{}", text(message, "xfer"));
        let data = unbase64(text(message, "data")).unwrap_or_default();
        let offset = message.get("offset").and_then(Value::as_u64);
        let problem = {
            let mut open = self.open();
            let entry = open
                .get_mut(&key)
                .ok_or_else(|| err(404, "No such transfer"))?;
            if offset != Some(entry.received) {
                Some(err(409, "Chunks arrived out of order"))
            } else if data.len() > MAX_CHUNK_BYTES
                || entry.received + data.len() as u64 > entry.size
            {
                Some(err(413, "More data than announced"))
            } else {
                entry.file.write_all(&data).map_err(ApiError::internal)?;
                entry.hash.update(&data);
                entry.received += data.len() as u64;
                return Ok(json!({ "received": entry.received }));
            }
        };
        self.discard(&key);
        Err(problem.unwrap_or_else(|| err(500, "transfer")))
    }

    pub fn end(&self, session: &str, message: &Value) -> Result<Ended, ApiError> {
        let key = format!("{session}:{}", text(message, "xfer"));
        let entry = self
            .open()
            .remove(&key)
            .ok_or_else(|| err(404, "No such transfer"))?;
        let Open {
            purpose,
            group,
            name,
            size,
            received,
            path,
            file,
            hash,
            ..
        } = entry;
        drop(file);
        let result = (|| {
            if received != size {
                return Err(err(409, format!("Incomplete: {received} of {size} bytes")));
            }
            let sha256: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
            let expected = text(message, "sha256");
            if !expected.is_empty() && expected != sha256 {
                return Err(ApiError::coded(
                    422,
                    "checksum",
                    "The data changed on the way (checksum mismatch)",
                ));
            }
            match purpose.as_str() {
                "paste_image" => {
                    let png = fs::read(&path).map_err(ApiError::internal)?;
                    if png.get(..8) != Some(&PNG_SIGNATURE[..]) {
                        return Err(err(400, "Not a PNG image"));
                    }
                    Ok(Ended::Image(png))
                }
                "paste_file" => {
                    let group_key = format!("{session}:{}", group.unwrap_or_default());
                    let mut groups = self.groups();
                    if !groups.contains_key(&group_key) {
                        self.prune_pasted();
                        let dir = self.temp_dir.join("pasted").join(uuid());
                        groups.insert(
                            group_key.clone(),
                            Group {
                                session: session.to_string(),
                                dir,
                                paths: Vec::new(),
                                size: 0,
                            },
                        );
                    }
                    let waiting = groups
                        .get_mut(&group_key)
                        .ok_or_else(|| err(500, "group"))?;
                    if waiting.paths.len() >= MAX_FILES_PER_PASTE {
                        return Err(ApiError::coded(
                            413,
                            "too-many",
                            format!("At most {MAX_FILES_PER_PASTE} files at once"),
                        ));
                    }
                    crate::files::ensure_private_dir(&waiting.dir).map_err(ApiError::internal)?;
                    let target = claim_free_path(&waiting.dir, &name, &path)?;
                    waiting.paths.push(target);
                    waiting.size += size;
                    Ok(Ended::Stored)
                }
                _ => {
                    fs::create_dir_all(&self.downloads).map_err(ApiError::internal)?;
                    let target = claim_free_path(&self.downloads, &name, &path)?;
                    let saved = target
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    Ok(Ended::Saved {
                        name: saved,
                        folder: self.downloads.clone(),
                        size,
                    })
                }
            }
        })();
        let _ = fs::remove_file(&path);
        result
    }

    pub fn cancel(&self, session: &str, message: &Value) -> Value {
        self.discard(&format!("{session}:{}", text(message, "xfer")));
        json!({})
    }

    /// The group's files for `xfer_paste`.
    pub fn take_group(&self, session: &str, message: &Value) -> Result<PasteGroup, ApiError> {
        let key = format!("{session}:{}", text(message, "group"));
        let mut groups = self.groups();
        if groups.get(&key).is_none_or(|g| g.paths.is_empty()) {
            return Err(err(404, "No files to paste"));
        }
        let group = groups
            .remove(&key)
            .ok_or_else(|| err(404, "No files to paste"))?;
        Ok(PasteGroup {
            paths: group.paths,
            size: group.size,
        })
    }

    /// Earlier pasted files old enough to go.
    fn prune_pasted(&self) {
        let Ok(entries) = fs::read_dir(self.temp_dir.join("pasted")) else {
            return;
        };
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| SystemTime::now().duration_since(at).ok())
                .is_some_and(|age| age > PASTED_FILES_KEEP);
            if old {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    /// The live view ended: its unfinished transfers go.
    pub fn close_session(&self, session: &str) {
        let keys: Vec<String> = self
            .open()
            .iter()
            .filter(|(_, e)| e.session == session)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            self.discard(&key);
        }
        let mut groups = self.groups();
        let gone: Vec<String> = groups
            .iter()
            .filter(|(_, g)| g.session == session)
            .map(|(k, _)| k.clone())
            .collect();
        for key in gone {
            if let Some(group) = groups.remove(&key) {
                let _ = fs::remove_dir_all(group.dir);
            }
        }
    }
}

fn create_private(path: &Path) -> std::io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Move `from` into `dir` as `name`, or `name (1)`, `name (2)`…, never replacing a file: each
/// name is claimed with a hard link (or an exclusive copy where links are not possible).
pub fn claim_free_path(dir: &Path, name: &str, from: &Path) -> Result<PathBuf, ApiError> {
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    for n in 0..MAX_NAME_TRIES {
        let candidate = dir.join(if n == 0 {
            name.to_string()
        } else {
            format!("{stem} ({n}){extension}")
        });
        match fs::hard_link(from, &candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                let copied = create_private(&candidate)
                    .and_then(|mut target| std::io::copy(&mut File::open(from)?, &mut target));
                match copied {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(ApiError::internal(error)),
                }
            }
        }
        let _ = fs::remove_file(from);
        return Ok(candidate);
    }
    Err(err(409, "No free name for this file"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::base64;

    fn send(
        transfers: &Transfers,
        xfer: &str,
        purpose: &str,
        data: &[u8],
        extra: Value,
    ) -> Result<Ended, ApiError> {
        let mut begin =
            json!({ "xfer": xfer, "purpose": purpose, "size": data.len(), "name": "../a.txt" });
        if let (Some(target), Some(more)) = (begin.as_object_mut(), extra.as_object()) {
            target.extend(more.clone());
        }
        transfers.begin("1", &begin)?;
        transfers.chunk(
            "1",
            &json!({ "xfer": xfer, "offset": 0, "data": base64(data) }),
        )?;
        transfers.end(
            "1",
            &json!({ "xfer": xfer, "sha256": crate::util::sha256_hex(data) }),
        )
    }

    #[test]
    fn files_land_in_downloads_without_replacing() {
        let dir = tempfile::tempdir().unwrap();
        let transfers = Transfers::new(dir.path().join("tmp"), dir.path().join("Downloads"));
        for expected in ["a.txt", "a (1).txt"] {
            match send(&transfers, "x1", "file", b"hello", json!({})).unwrap() {
                Ended::Saved { name, .. } => assert_eq!(name, expected),
                _ => panic!("not saved"),
            }
        }
        assert_eq!(
            fs::read(dir.path().join("Downloads/a (1).txt")).unwrap(),
            b"hello"
        );
        let error = send(&transfers, "x2", "paste_image", b"not a png", json!({}))
            .err()
            .unwrap();
        assert_eq!(error.message, "Not a PNG image");
        let error = transfers
            .begin(
                "1",
                &json!({ "xfer": "x3", "purpose": "paste_image", "size": MAX_IMAGE_BYTES + 1 }),
            )
            .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("too-large"));
    }

    #[test]
    fn grouped_files_wait_for_their_paste() {
        let dir = tempfile::tempdir().unwrap();
        let transfers = Transfers::new(dir.path().join("tmp"), dir.path().join("Downloads"));
        assert!(matches!(
            send(&transfers, "a", "paste_file", b"1", json!({ "group": "g" })).unwrap(),
            Ended::Stored
        ));
        assert!(matches!(
            send(
                &transfers,
                "b",
                "paste_file",
                b"22",
                json!({ "group": "g" })
            )
            .unwrap(),
            Ended::Stored
        ));
        let group = transfers.take_group("1", &json!({ "group": "g" })).unwrap();
        assert_eq!((group.paths.len(), group.size), (2, 3));
        assert!(transfers.take_group("1", &json!({ "group": "g" })).is_err());
    }

    #[test]
    fn names_are_safe() {
        assert_eq!(safe_name("..\\..\\evil.txt"), "evil.txt");
        assert_eq!(safe_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(safe_name("..."), "file");
        assert_eq!(safe_name(".hidden"), "hidden");
    }
}
