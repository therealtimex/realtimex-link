//! Copies on this computer for the viewers controlling it (#2682;
//! the live view's Share clipboard). Viewers that share the clipboard are told each new
//! copy; they fetch what they want with `pull_chunk` and end with `pull_close`. Only what the
//! latest copy announced (or a file picked with Get files) can be pulled: a viewer cannot name a
//! path. The router polls the clipboard and sends the announcements.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::error::ApiError;
use crate::util::{base64, unbase64, uuid};

pub const POLL_MS: u64 = 700;
const MAX_INLINE_TEXT: usize = 40 * 1024;
pub const PULL_CHUNK_BYTES: u64 = 40 * 1024;

enum Source {
    Buffer(Vec<u8>),
    File(PathBuf),
}

struct Pull {
    session: String,
    source: Source,
    size: u64,
    batch: Option<String>,
    from_browse: bool,
    pulled: bool,
}

struct Batch {
    client: Option<Value>,
    pending: HashSet<String>,
    done: Vec<(String, u64)>,
}

/// Files of this computer fetched to the end (one notice per batch).
#[derive(Debug)]
pub struct Pulled {
    pub client: Value,
    pub count: usize,
    pub name: String,
    pub size: u64,
}

#[derive(Default)]
struct State {
    watching: HashSet<String>,
    pulls: HashMap<String, Pull>,
    batches: HashMap<String, Batch>,
    last_seq: Option<Value>,
}

#[derive(Default)]
pub struct ClipboardShare {
    state: Mutex<State>,
}

pub fn handles(kind: &str) -> bool {
    [
        "clipboard_watch",
        "clipboard_get",
        "pull_chunk",
        "pull_close",
    ]
    .contains(&kind)
}

fn name_of(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl State {
    /// A file pull is over: tell once its batch has no file still to come.
    fn settle(&mut self, id: &str, fetched: bool) -> Option<Pulled> {
        let pull = self.pulls.get(id)?;
        let Source::File(path) = &pull.source else {
            return None;
        };
        let key = format!("{}:{}", pull.session, pull.batch.as_ref()?);
        let entry = (name_of(path), pull.size);
        let batch = self.batches.get_mut(&key)?;
        if !batch.pending.remove(id) {
            return None;
        }
        if fetched {
            batch.done.push(entry);
        }
        if !batch.pending.is_empty() {
            return None;
        }
        let batch = self.batches.remove(&key)?;
        let (first, _) = batch.done.first()?.clone();
        Some(Pulled {
            client: batch.client?,
            count: batch.done.len(),
            name: first,
            size: batch.done.iter().map(|(_, size)| size).sum(),
        })
    }

    fn remove_pull(&mut self, id: &str) -> Option<Pulled> {
        let pulled = self.pulls.get(id).is_some_and(|p| p.pulled);
        let notice = if pulled { None } else { self.settle(id, false) };
        self.pulls.remove(id);
        notice
    }

    /// A new copy replaces the last one's pulls; files picked with Get files stay until fetched.
    fn drop_pulls(&mut self, session: &str, all: bool) -> Vec<Pulled> {
        let ids: Vec<String> = self
            .pulls
            .iter()
            .filter(|(_, p)| p.session == session && (all || !p.from_browse))
            .map(|(id, _)| id.clone())
            .collect();
        ids.iter().filter_map(|id| self.remove_pull(id)).collect()
    }

    fn add_pull(
        &mut self,
        session: &str,
        source: Source,
        size: u64,
        batch: Option<String>,
        from_browse: bool,
    ) -> String {
        let id = uuid();
        if let (Source::File(_), Some(batch)) = (&source, &batch) {
            let key = format!("{session}:{batch}");
            let entry = self.batches.entry(key).or_insert_with(|| Batch {
                client: None,
                pending: HashSet::new(),
                done: Vec::new(),
            });
            entry.pending.insert(id.clone());
        }
        self.pulls.insert(
            id.clone(),
            Pull {
                session: session.to_string(),
                source,
                size,
                batch,
                from_browse,
                pulled: false,
            },
        );
        id
    }
}

impl ClipboardShare {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// One viewer's announcement for this content (`clipboard_read`'s answer), with its own pulls.
    pub fn announce(&self, session: &str, content: &Value) -> Option<Value> {
        let mut state = self.state();
        state.drop_pulls(session, false);
        match content.get("kind").and_then(Value::as_str)? {
            "text" => {
                let text = content.get("text").and_then(Value::as_str).unwrap_or("");
                if text.len() <= MAX_INLINE_TEXT {
                    return Some(json!({ "type": "clipboard", "kind": "text", "text": text }));
                }
                let data = text.as_bytes().to_vec();
                let size = data.len() as u64;
                let pull = state.add_pull(session, Source::Buffer(data), size, None, false);
                Some(json!({ "type": "clipboard", "kind": "text", "size": size, "pull": pull }))
            }
            "image" => {
                let mime = content.get("mime").cloned().unwrap_or(Value::Null);
                let data = content
                    .get("data")
                    .and_then(Value::as_str)
                    .and_then(unbase64);
                match data {
                    Some(data) if content.get("tooLarge") != Some(&Value::Bool(true)) => {
                        let size = data.len() as u64;
                        let pull = state.add_pull(session, Source::Buffer(data), size, None, false);
                        Some(
                            json!({ "type": "clipboard", "kind": "image", "mime": mime, "size": size, "pull": pull }),
                        )
                    }
                    _ => Some(
                        json!({ "type": "clipboard", "kind": "image", "size": content.get("size"), "mime": mime, "tooLarge": true }),
                    ),
                }
            }
            "files" => {
                // One copy is one batch: one notice once its files were fetched.
                let batch = uuid();
                let files: Vec<Value> = content
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .map(|file| {
                                let size = file.get("size").and_then(Value::as_u64).unwrap_or(0);
                                let dir =
                                    file.get("dir").is_some_and(|d| d.as_bool() == Some(true));
                                let mut shown =
                                    json!({ "name": file.get("name"), "size": size, "dir": dir });
                                // Folders are listed, not sent.
                                if let (false, Some(path)) =
                                    (dir, file.get("path").and_then(Value::as_str))
                                {
                                    let pull = state.add_pull(
                                        session,
                                        Source::File(PathBuf::from(path)),
                                        size,
                                        Some(batch.clone()),
                                        false,
                                    );
                                    shown["pull"] = json!(pull);
                                }
                                shown
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(json!({ "type": "clipboard", "kind": "files", "files": files }))
            }
            _ => None,
        }
    }

    pub fn watching(&self) -> Vec<String> {
        self.state().watching.iter().cloned().collect()
    }

    pub fn is_watching(&self, session: &str) -> bool {
        self.state().watching.contains(session)
    }

    /// Start (with the current sequence: what is copied already is not new) or stop watching.
    pub fn watch(&self, session: &str, on: bool, current_seq: Option<Value>) -> Value {
        let mut state = self.state();
        if on {
            if state.watching.is_empty() {
                state.last_seq = current_seq;
            }
            state.watching.insert(session.to_string());
        } else {
            state.watching.remove(session);
            state.drop_pulls(session, false);
        }
        if state.watching.is_empty() {
            state.last_seq = None;
        }
        json!({ "watching": state.watching.contains(session) })
    }

    pub fn needs_first_seq(&self) -> bool {
        self.state().watching.is_empty()
    }

    /// A new sequence number: true when it is a change worth reading.
    pub fn changed(&self, seq: &Value) -> bool {
        let mut state = self.state();
        let previous = state.last_seq.replace(seq.clone());
        previous.is_some_and(|previous| &previous != seq)
    }

    /// Stop sending to a session that lost its permission.
    pub fn unwatch(&self, session: &str) {
        let mut state = self.state();
        state.watching.remove(session);
        state.drop_pulls(session, true);
    }

    pub fn chunk(
        &self,
        session: &str,
        client: Option<&Value>,
        message: &Value,
    ) -> Result<(Value, Option<Pulled>), ApiError> {
        let id = message
            .get("pull")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut state = self.state();
        let pull = state
            .pulls
            .get(&id)
            .filter(|p| p.session == session)
            .ok_or_else(|| {
                ApiError::coded(404, "gone", "Nothing to fetch (copied again since?)")
            })?;
        let offset = message
            .get("offset")
            .and_then(Value::as_u64)
            .filter(|o| *o <= pull.size)
            .ok_or_else(|| ApiError::bad_request("Bad offset"))?;
        let data = match &pull.source {
            Source::Buffer(bytes) => {
                let start = offset as usize;
                let end = (start + PULL_CHUNK_BYTES as usize).min(bytes.len());
                bytes.get(start..end).unwrap_or_default().to_vec()
            }
            Source::File(path) => {
                let length = PULL_CHUNK_BYTES.min(pull.size.saturating_sub(offset));
                let mut buffer = vec![0u8; length as usize];
                let mut file = File::open(path).map_err(ApiError::internal)?;
                file.seek(SeekFrom::Start(offset))
                    .map_err(ApiError::internal)?;
                let read = read_up_to(&mut file, &mut buffer).map_err(ApiError::internal)?;
                buffer.truncate(read);
                buffer
            }
        };
        let size = pull.size;
        let eof = offset + data.len() as u64 >= size;
        let is_file = matches!(pull.source, Source::File(_));
        let already = pull.pulled;
        if let (Some(batch), Some(client)) = (pull.batch.clone(), client) {
            if let Some(entry) = state.batches.get_mut(&format!("{session}:{batch}")) {
                entry.client = Some(client.clone());
            }
        }
        let mut notice = None;
        if eof && is_file && !already {
            notice = state.settle(&id, true);
            if let Some(pull) = state.pulls.get_mut(&id) {
                pull.pulled = true;
            }
        }
        Ok((
            json!({ "data": base64(&data), "size": size, "eof": eof }),
            notice,
        ))
    }

    pub fn close(&self, session: &str, message: &Value) -> Option<Pulled> {
        let id = message.get("pull").and_then(Value::as_str).unwrap_or("");
        let mut state = self.state();
        if state.pulls.get(id).is_some_and(|p| p.session == session) {
            return state.remove_pull(id);
        }
        None
    }

    pub fn close_session(&self, session: &str) {
        let mut state = self.state();
        state.watching.remove(session);
        state.drop_pulls(session, true);
        if state.watching.is_empty() {
            state.last_seq = None;
        }
    }

    /// A file picked with Get files, in its selection's batch (one notice for all of it).
    pub fn offer_file(
        &self,
        session: &str,
        path: PathBuf,
        size: u64,
        batch: Option<&str>,
    ) -> String {
        let batch: String = batch
            .filter(|b| !b.is_empty())
            .map_or_else(uuid, |b| b.chars().take(64).collect());
        self.state()
            .add_pull(session, Source::File(path), size, Some(batch), true)
    }
}

fn read_up_to(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announces_and_serves_only_the_latest_copy() {
        let share = ClipboardShare::default();
        assert_eq!(
            share
                .announce("1", &json!({ "kind": "text", "text": "hi" }))
                .unwrap()["text"],
            "hi"
        );
        let long = "x".repeat(MAX_INLINE_TEXT + 1);
        let message = share
            .announce("1", &json!({ "kind": "text", "text": long }))
            .unwrap();
        let pull = message["pull"].as_str().unwrap().to_string();
        let (chunk, _) = share
            .chunk("1", None, &json!({ "pull": pull, "offset": 0 }))
            .unwrap();
        assert_eq!(chunk["eof"], false);
        assert_eq!(
            unbase64(chunk["data"].as_str().unwrap()).unwrap().len() as u64,
            PULL_CHUNK_BYTES
        );
        // Another viewer can't fetch it; a new copy replaces it.
        assert_eq!(
            share
                .chunk("2", None, &json!({ "pull": pull, "offset": 0 }))
                .unwrap_err()
                .code
                .as_deref(),
            Some("gone")
        );
        share.announce("1", &json!({ "kind": "text", "text": "new" }));
        assert!(share
            .chunk("1", None, &json!({ "pull": pull, "offset": 0 }))
            .is_err());
    }

    #[test]
    fn a_fetched_selection_is_told_once() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
        std::fs::write(&a, "aa").unwrap();
        std::fs::write(&b, "bbb").unwrap();
        let share = ClipboardShare::default();
        let first = share.offer_file("1", a, 2, Some("sel"));
        let second = share.offer_file("1", b, 3, Some("sel"));
        let client = json!({ "name": "Laptop" });
        let (_, notice) = share
            .chunk("1", Some(&client), &json!({ "pull": first, "offset": 0 }))
            .unwrap();
        assert!(notice.is_none());
        let (_, notice) = share
            .chunk("1", Some(&client), &json!({ "pull": second, "offset": 0 }))
            .unwrap();
        let notice = notice.unwrap();
        assert_eq!(
            (notice.count, notice.size, notice.name.as_str()),
            (2, 5, "a.txt")
        );
    }

    #[test]
    fn watching_starts_from_the_current_copy() {
        let share = ClipboardShare::default();
        assert!(share.needs_first_seq());
        share.watch("1", true, Some(json!(5)));
        assert!(!share.changed(&json!(5)));
        assert!(share.changed(&json!(6)));
        share.watch("1", false, None);
        assert!(share.watching().is_empty());
    }
}
