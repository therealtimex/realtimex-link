//! Published artifacts and the embed widget's assets, served by Link itself
//! (realtimex-ai-app#2836 M2, contract §6.1): RealTimeX pushes the list (PUT /v1/artifacts) and
//! Link serves surface `a`'s /ar/<id>/… straight from each artifact's folder, whether or not
//! RealTimeX is running, with the answers RealTimeX's own route gave (desktopPublicArtifacts.js:
//! sendFile behind Cache-Control: no-store and X-Robots-Tag: noindex; anything not servable is
//! 404 {"error":"Artifact not found."}). Surface `e`'s /embed/<file> comes from RealTimeX's
//! dist/embed folder the same way.
//!
//! Deliberately stricter than RealTimeX's route (§6.1 a–e): one URL decode, any dot segment is
//! hidden, no credentialed CORS, X-Content-Type-Options: nosniff, and a containment check on
//! canonical paths that holds on Windows too.
//!
//! Protected sources (realtimex-ai-app#2838, Link 0.2.3): RealTimeX also pushes
//! `restrictedRoots`, the folders that must never be public. Link keeps them with the list, so
//! they hold while RealTimeX is closed, and decides per request about the file it actually opened:
//! that file's own path (from its handle) is inside the artifact's folder as it is now and outside
//! every restricted folder, and the body, a range or a 304 all come from that one handle. A push
//! that changes the list ends the bodies still streaming from the one before (`Fence`).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use serde_json::{json, Value};

use crate::error::ApiError;
use crate::store::Store;

pub const KEY: &str = "artifacts";
const NOT_FOUND: &str = r#"{"error":"Artifact not found."}"#;
const MAX_ITEMS: usize = 10_000;
const MAX_RESTRICTED_ROOTS: usize = 1_000;

/// One published artifact as RealTimeX pushed it.
#[derive(Clone, Debug, PartialEq)]
pub struct Published {
    pub id: String,
    pub dir: PathBuf,
    /// The entry file, relative to `dir` (posix).
    pub entry: String,
    pub status: String,
    /// Milliseconds since the epoch.
    pub expires_at: Option<i64>,
}

impl Published {
    /// Served now: published and not expired.
    fn active(&self, now: i64) -> bool {
        self.status == "published" && self.expires_at.is_none_or(|at| at > now)
    }
}

fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (6..=63).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

/// PUT /v1/artifacts's body, checked: what the store keeps ({ items, embedAssetsDir }).
pub fn parse(body: &Value) -> Result<Value, ApiError> {
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("items must be a list"))?;
    if items.len() > MAX_ITEMS {
        return Err(ApiError::bad_request(format!(
            "at most {MAX_ITEMS} artifacts"
        )));
    }
    let mut kept = Vec::new();
    for item in items {
        let text = |name: &str| item.get(name).and_then(Value::as_str).unwrap_or("");
        let id = text("id").to_ascii_lowercase();
        if !valid_id(&id) {
            return Err(ApiError::bad_request(format!(
                "{id:?} is not an artifact id"
            )));
        }
        let dir = text("dir");
        if dir.is_empty() || !Path::new(dir).is_absolute() || dir.contains('\0') {
            return Err(ApiError::bad_request(format!(
                "{id}: dir must be an absolute path"
            )));
        }
        let entry = text("entry");
        if entry.is_empty() || entry.starts_with('/') || entry.contains(['\0', '\\']) {
            return Err(ApiError::bad_request(format!(
                "{id}: entry must be a path inside dir"
            )));
        }
        let status = text("status");
        if !matches!(status, "published" | "paused" | "revoked") {
            return Err(ApiError::bad_request(format!(
                "{id}: status must be published, paused or revoked"
            )));
        }
        let expires_at = match item.get("expiresAt") {
            None | Some(Value::Null) => Value::Null,
            Some(Value::String(at)) => chrono::DateTime::parse_from_rfc3339(at)
                .map(|at| json!(at.timestamp_millis()))
                .map_err(|_| ApiError::bad_request(format!("{id}: expiresAt must be ISO 8601")))?,
            Some(_) => {
                return Err(ApiError::bad_request(format!(
                    "{id}: expiresAt must be ISO 8601"
                )))
            }
        };
        kept.push(json!({
            "id": id,
            "dir": dir,
            "entry": entry,
            "status": status,
            "expiresAt": expires_at,
        }));
    }
    let restricted = match body.get("restrictedRoots") {
        // An older RealTimeX: nothing restricted.
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(roots)) if roots.len() <= MAX_RESTRICTED_ROOTS => roots
            .iter()
            .map(|root| match root.as_str() {
                Some(root) if Path::new(root).is_absolute() && !root.contains('\0') => {
                    Ok(json!(root))
                }
                _ => Err(ApiError::bad_request(
                    "restrictedRoots must be absolute paths",
                )),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(ApiError::bad_request(format!(
                "restrictedRoots must be a list of at most {MAX_RESTRICTED_ROOTS} paths"
            )))
        }
    };
    let assets = match body.get("embedAssetsDir") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(dir)) if Path::new(dir).is_absolute() && !dir.contains('\0') => {
            json!(dir)
        }
        Some(_) => {
            return Err(ApiError::bad_request(
                "embedAssetsDir must be an absolute path",
            ))
        }
    };
    Ok(json!({ "items": kept, "embedAssetsDir": assets, "restrictedRoots": restricted }))
}

/// Keep a checked push. The same list again (RealTimeX pushes on every start) changes nothing,
/// so the bodies streaming from it go on.
pub fn set(store: &Store, value: &Value) -> Result<(), ApiError> {
    if store.get(KEY).as_ref() != Some(value) {
        store.set(KEY, value)?;
    }
    Ok(())
}

/// The restricted folders as RealTimeX pushed them.
fn restricted_roots(kept: &Value) -> Vec<PathBuf> {
    kept.get("restrictedRoots")
        .and_then(Value::as_array)
        .map(|roots| {
            roots
                .iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

fn find(kept: &Value, id: &str) -> Option<Published> {
    let item = kept
        .get("items")?
        .as_array()?
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))?;
    Some(Published {
        id: id.to_string(),
        dir: PathBuf::from(item.get("dir")?.as_str()?),
        entry: item.get("entry")?.as_str()?.to_string(),
        status: item.get("status")?.as_str()?.to_string(),
        expires_at: item.get("expiresAt").and_then(Value::as_i64),
    })
}

/// How many artifacts are up now, for the window's Shared card.
pub fn published_count(store: &Store) -> u64 {
    let now = crate::util::now_ms();
    store
        .get(KEY)
        .and_then(|kept| kept.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter(|item| {
            item.get("status").and_then(Value::as_str) == Some("published")
                && item
                    .get("expiresAt")
                    .and_then(Value::as_i64)
                    .is_none_or(|at| at > now)
        })
        .count() as u64
}

pub fn embed_assets_dir(store: &Store) -> Option<PathBuf> {
    embed_dir(&store.get(KEY)?)
}

fn embed_dir(kept: &Value) -> Option<PathBuf> {
    kept.get("embedAssetsDir")?.as_str().map(PathBuf::from)
}

/// What a body was decided from: the list as it was when the request came in. It no longer holds
/// once a push changed the list, and a body still streaming then ends (relay/origin.rs).
#[derive(Clone, Debug)]
pub struct Fence {
    changes: Arc<AtomicU64>,
    seen: u64,
}

impl Fence {
    /// Taken before the list is read, so a push in between ends the body too.
    fn now(store: &Store) -> Self {
        let changes = store.changes(KEY);
        let seen = changes.load(Ordering::SeqCst);
        Self { changes, seen }
    }

    pub fn holds(&self) -> bool {
        self.changes.load(Ordering::SeqCst) == self.seen
    }
}

/// Whether Link answers this request itself: surface a's artifacts, and surface e's widget
/// assets once RealTimeX said where they are.
pub fn serves(store: &Store, surface: &str, pathname: &str) -> bool {
    match surface {
        "a" => pathname == "/ar" || pathname.starts_with("/ar/"),
        "e" => {
            (pathname == "/embed" || pathname.starts_with("/embed/"))
                && embed_assets_dir(store).is_some()
        }
        _ => false,
    }
}

/// An answer Link gives itself: status, headers and body (a whole buffer, or part of a file).
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

#[derive(Debug)]
pub enum Body {
    Bytes(Vec<u8>),
    /// `length` bytes of the opened file from `start`, while `fence` holds.
    File {
        file: File,
        start: u64,
        length: u64,
        fence: Fence,
    },
}

impl PartialEq for Body {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Body::Bytes(a), Body::Bytes(b)) => a == b,
            (
                Body::File { start, length, .. },
                Body::File {
                    start: other_start,
                    length: other_length,
                    ..
                },
            ) => (start, length) == (other_start, other_length),
            _ => false,
        }
    }
}

impl Answer {
    fn json(status: u16, text: &str) -> Self {
        Self {
            status,
            headers: vec![
                (
                    "content-type".into(),
                    "application/json; charset=utf-8".into(),
                ),
                ("cache-control".into(), "no-store".into()),
                ("x-robots-tag".into(), "noindex".into()),
            ],
            body: Body::Bytes(text.as_bytes().to_vec()),
        }
    }

    fn not_found() -> Self {
        Self::json(404, NOT_FOUND)
    }

    fn method_not_allowed() -> Self {
        let mut answer = Self::json(405, r#"{"error":"Method not allowed."}"#);
        answer.headers.push(("allow".into(), "GET, HEAD".into()));
        answer
    }
}

/// The request's path segments after `prefix`, decoded once. None: not servable (an encoding
/// that isn't valid, a NUL or backslash, or a dot segment: `.`, `..`, `.git`, `.env`).
fn segments(rest: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for raw in rest.split('/').filter(|s| !s.is_empty()) {
        let decoded = percent_decode(raw)?;
        if decoded.is_empty()
            || decoded.starts_with('.')
            || decoded.contains(['\0', '\\', '/'])
            || decoded.chars().any(char::is_control)
        {
            return None;
        }
        out.push(decoded);
    }
    Some(out)
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A path as compared with a restricted folder: Windows' and macOS' usual volumes ignore case, and
/// on Windows a handle's path is verbatim (`\\?\C:\…`) where RealTimeX sends `C:\…`. Only ever
/// to deny: on a case-sensitive volume it can take two folders for one, which is safe for a
/// denial and never for an allowance (`open` decides containment on exact handle paths).
fn comparable(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        let text = text.replace('/', "\\");
        let text = match text.strip_prefix(r"\\?\UNC\") {
            Some(share) => format!(r"\\{share}"),
            None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
        };
        PathBuf::from(text.to_lowercase())
    } else if cfg!(target_os = "macos") {
        PathBuf::from(text.to_lowercase())
    } else {
        path.to_path_buf()
    }
}

/// Whether `path` may be inside `root`, case aside (see `comparable`).
fn maybe_inside(path: &Path, root: &Path) -> bool {
    comparable(path).starts_with(comparable(root))
}

fn overlap(a: &Path, b: &Path) -> bool {
    maybe_inside(a, b) || maybe_inside(b, a)
}

/// Every form a path has now: as given, canonical, and its handle's own path. One that no longer
/// exists is only as given.
fn forms(path: &Path) -> Vec<PathBuf> {
    let mut forms = vec![path.to_path_buf()];
    forms.extend(std::fs::canonicalize(path).ok());
    forms.extend(open_path(path, true).as_ref().and_then(opened_path));
    forms
}

/// `path` opened for reading without following it anywhere else later; a folder too when `dir`.
/// Never blocks on a FIFO.
fn open_path(path: &Path, dir: bool) -> Option<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = dir;
        options.custom_flags(libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    if dir {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS);
    }
    options.open(path).ok()
}

/// Where an open file or folder actually is, asked of its handle, not of a path that may have
/// changed since.
#[cfg(target_os = "macos")]
fn opened_path(file: &File) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most MAXPATHLEN (PATH_MAX) bytes, NUL included.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) } == -1 {
        return None;
    }
    let end = buffer.iter().position(|b| *b == 0)?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..end])))
}

#[cfg(target_os = "linux")]
fn opened_path(file: &File) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()
}

#[cfg(windows)]
fn opened_path(file: &File) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS,
    };
    let mut buffer = vec![0u16; 1024];
    loop {
        // SAFETY: the handle is open for the call; the buffer's length is passed with it.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle() as _,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if length == 0 {
            return None;
        }
        // Too small: the length needed, NUL included.
        if length >= buffer.len() {
            buffer.resize(length + 1, 0);
            continue;
        }
        buffer.truncate(length);
        return Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer)));
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn opened_path(_file: &File) -> Option<PathBuf> {
    None
}

/// The file a request names, opened once: a regular file whose own path is inside `root` as it is
/// now and outside every restricted folder, with that path (its type comes from it). A folder
/// gives its index.html. None for anything else, and for a root that is, holds or lies in a
/// restricted folder.
fn open(root: &Path, segments: &[String], restricted: &[PathBuf]) -> Option<(File, PathBuf)> {
    let restricted: Vec<PathBuf> = restricted
        .iter()
        .map(PathBuf::as_path)
        .flat_map(forms)
        .collect();
    let real_root = open_path(root, true).as_ref().and_then(opened_path)?;
    if forms(root)
        .iter()
        .any(|form| restricted.iter().any(|r| overlap(form, r)))
    {
        return None;
    }
    let mut target = real_root.clone();
    target.extend(segments);
    if std::fs::metadata(&target).ok()?.is_dir() {
        target.push("index.html");
    }
    let file = open_path(&target, false)?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let opened = opened_path(&file)?;
    // Inside: both paths come from the system's handles in the same form, so they compare
    // exactly, the way the volume tells its folders apart.
    (opened.starts_with(&real_root) && !restricted.iter().any(|r| maybe_inside(&opened, r)))
        .then_some((file, opened))
}

/// GET or HEAD /ar/<id>[/<path>] (surface a), as RealTimeX's route answered it.
pub fn artifact(
    store: &Store,
    method: &str,
    pathname: &str,
    headers: &[(String, String)],
) -> Answer {
    if !matches!(method, "GET" | "HEAD") {
        return Answer::method_not_allowed();
    }
    let rest = pathname.strip_prefix("/ar").unwrap_or("");
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let (id, path) = rest.split_once('/').unwrap_or((rest, ""));
    let id = id.to_ascii_lowercase();
    if !valid_id(&id) {
        return Answer::not_found();
    }
    let fence = Fence::now(store);
    let Some(kept) = store.get(KEY) else {
        return Answer::not_found();
    };
    let Some(published) = find(&kept, &id).filter(|p| p.active(crate::util::now_ms())) else {
        return Answer::not_found();
    };
    let requested = if path.trim_matches('/').is_empty() {
        segments(&published.entry)
    } else {
        segments(path)
    };
    let restricted = restricted_roots(&kept);
    let Some(opened) = requested.and_then(|segments| open(&published.dir, &segments, &restricted))
    else {
        return Answer::not_found();
    };
    let mut answer = file_answer(opened, fence, method, headers);
    answer
        .headers
        .push(("cache-control".into(), "no-store".into()));
    answer
        .headers
        .push(("x-robots-tag".into(), "noindex".into()));
    answer
}

/// GET or HEAD /embed/<file> (surface e) from RealTimeX's widget assets.
pub fn embed_asset(
    store: &Store,
    method: &str,
    pathname: &str,
    headers: &[(String, String)],
) -> Answer {
    let text = |status: u16, body: &str| Answer {
        status,
        headers: vec![
            ("content-type".into(), "text/plain; charset=utf-8".into()),
            ("cache-control".into(), "no-store".into()),
        ],
        body: Body::Bytes(body.as_bytes().to_vec()),
    };
    if !matches!(method, "GET" | "HEAD") {
        let mut answer = text(405, "Method not allowed.");
        answer.headers.push(("allow".into(), "GET, HEAD".into()));
        return answer;
    }
    let rest = pathname.strip_prefix("/embed").unwrap_or("");
    let fence = Fence::now(store);
    let opened = store.get(KEY).and_then(|kept| {
        let dir = embed_dir(&kept)?;
        let segments = segments(rest).filter(|s| !s.is_empty())?;
        open(&dir, &segments, &restricted_roots(&kept))
    });
    let Some(opened) = opened else {
        return text(404, "Not found.");
    };
    let mut answer = file_answer(opened, fence, method, headers);
    answer
        .headers
        .push(("cache-control".into(), "public, max-age=300".into()));
    answer
}

/// A file as `send` (Express's sendFile) answers it: its type, Last-Modified, a weak ETag,
/// Accept-Ranges, 304 for a fresh copy, 206 for one range and 416 for one out of bounds. All of
/// it from the opened file, never its path again.
fn file_answer(
    (file, path): (File, PathBuf),
    fence: Fence,
    method: &str,
    headers: &[(String, String)],
) -> Answer {
    let Ok(meta) = file.metadata() else {
        return Answer::not_found();
    };
    let size = meta.len();
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let modified_ms = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let etag = format!("W/\"{size:x}-{modified_ms:x}\"");
    let last_modified = chrono::DateTime::<chrono::Utc>::from(modified)
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let mut out = vec![
        ("content-type".to_string(), content_type(&path).to_string()),
        ("accept-ranges".to_string(), "bytes".to_string()),
        ("last-modified".to_string(), last_modified.clone()),
        ("etag".to_string(), etag.clone()),
        ("x-content-type-options".to_string(), "nosniff".to_string()),
    ];
    let fresh = match header("if-none-match") {
        Some(tags) => tags.split(',').any(|tag| {
            tag.trim() == "*"
                || tag.trim().trim_start_matches("W/") == etag.trim_start_matches("W/")
        }),
        None => header("if-modified-since").is_some_and(|since| {
            chrono::DateTime::parse_from_rfc2822(since)
                .map(|since| since.timestamp() >= (modified_ms / 1000) as i64)
                .unwrap_or(false)
        }),
    };
    if fresh {
        out.retain(|(name, _)| name != "content-type");
        return Answer {
            status: 304,
            headers: out,
            body: Body::Bytes(Vec::new()),
        };
    }
    let (status, start, length) = match header("range").map(|r| range(r, size)) {
        Some(Range::One(start, end)) => {
            out.push((
                "content-range".into(),
                format!("bytes {start}-{end}/{size}"),
            ));
            (206, start, end - start + 1)
        }
        Some(Range::Unsatisfiable) => {
            out.push(("content-range".into(), format!("bytes */{size}")));
            return Answer {
                status: 416,
                headers: out,
                body: Body::Bytes(Vec::new()),
            };
        }
        Some(Range::Whole) | None => (200, 0, size),
    };
    out.push(("content-length".into(), length.to_string()));
    let body = if method == "HEAD" || length == 0 {
        Body::Bytes(Vec::new())
    } else {
        Body::File {
            file,
            start,
            length,
            fence,
        }
    };
    Answer {
        status,
        headers: out,
        body,
    }
}

#[derive(Debug, PartialEq)]
enum Range {
    /// First and last byte, inclusive.
    One(u64, u64),
    Unsatisfiable,
    /// Several ranges or one not understood: the whole file, as `send` does.
    Whole,
}

fn range(header: &str, size: u64) -> Range {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return Range::Whole;
    };
    if spec.contains(',') {
        return Range::Whole;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return Range::Whole;
    };
    let parse = |text: &str| text.trim().parse::<u64>().ok();
    let (start, end) = match (first.trim().is_empty(), last.trim().is_empty()) {
        // bytes=-N: the last N bytes.
        (true, false) => match parse(last) {
            Some(0) | None => return Range::Unsatisfiable,
            Some(n) => (size.saturating_sub(n), size.saturating_sub(1)),
        },
        (false, true) => match parse(first) {
            Some(start) => (start, size.saturating_sub(1)),
            None => return Range::Whole,
        },
        (false, false) => match (parse(first), parse(last)) {
            (Some(start), Some(end)) if start <= end => (start, end.min(size.saturating_sub(1))),
            _ => return Range::Whole,
        },
        (true, true) => return Range::Whole,
    };
    if size == 0 || start >= size {
        Range::Unsatisfiable
    } else {
        Range::One(start, end)
    }
}

/// The type `send` (mime 1.6) gives a file by its extension; text types get charset=UTF-8.
pub fn content_type(file: &Path) -> &'static str {
    let extension = file
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=UTF-8",
        "css" => "text/css; charset=UTF-8",
        "js" | "mjs" => "application/javascript; charset=UTF-8",
        "json" | "map" => "application/json; charset=UTF-8",
        "txt" | "text" => "text/plain; charset=UTF-8",
        "md" | "markdown" => "text/markdown; charset=UTF-8",
        "csv" => "text/csv; charset=UTF-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "pdf" => "application/pdf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "m4a" => "audio/mp4",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(dir: &Path, items: Value) -> (tempfile::TempDir, Store) {
        store_restricting(dir, items, &[])
    }

    /// `items`' dirs and the `restricted` folders are relative to `dir`.
    fn store_restricting(
        dir: &Path,
        items: Value,
        restricted: &[&str],
    ) -> (tempfile::TempDir, Store) {
        let data = tempfile::tempdir().unwrap();
        let store = Store::open(data.path()).unwrap();
        set(&store, &push(dir, items, restricted)).unwrap();
        (data, store)
    }

    fn push(dir: &Path, items: Value, restricted: &[&str]) -> Value {
        let mut body = json!({
            "items": items,
            "embedAssetsDir": dir.join("embed").to_str(),
            "restrictedRoots": restricted.iter().map(|r| dir.join(r).to_str().unwrap().to_string()).collect::<Vec<_>>(),
        });
        if let Some(list) = body["items"].as_array_mut() {
            for item in list {
                item["dir"] = json!(dir.join(item["dir"].as_str().unwrap()).to_str());
            }
        }
        parse(&body).unwrap()
    }

    /// What the body sends: its bytes of the opened file.
    fn sent(answer: Answer) -> Vec<u8> {
        use std::io::{Read, Seek};
        match answer.body {
            Body::Bytes(bytes) => bytes,
            Body::File {
                mut file,
                start,
                length,
                ..
            } => {
                file.seek(std::io::SeekFrom::Start(start)).unwrap();
                let mut bytes = vec![0; length as usize];
                file.read_exact(&mut bytes).unwrap();
                bytes
            }
        }
    }

    fn get(store: &Store, path: &str, headers: &[(&str, &str)]) -> Answer {
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
        artifact(store, "GET", path, &headers)
    }

    fn header<'a>(answer: &'a Answer, name: &str) -> Option<&'a str> {
        answer
            .headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn site() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let site = root.path().join("site");
        std::fs::create_dir_all(site.join("assets")).unwrap();
        std::fs::create_dir_all(site.join(".git")).unwrap();
        std::fs::write(site.join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(site.join("assets/app.js"), "console.log(1)").unwrap();
        std::fs::write(site.join("assets/data.bin"), [0u8; 100]).unwrap();
        std::fs::write(site.join(".git/config"), "secret").unwrap();
        std::fs::write(site.join(".env"), "secret").unwrap();
        std::fs::write(root.path().join("outside.txt"), "outside").unwrap();
        std::fs::create_dir_all(root.path().join("single")).unwrap();
        std::fs::write(root.path().join("single/report.pdf"), "%PDF").unwrap();
        std::fs::create_dir_all(root.path().join("embed")).unwrap();
        std::fs::write(
            root.path().join("embed/realtimex-chat-widget.min.js"),
            "widget",
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path().join("outside.txt"), site.join("escape")).unwrap();
        root
    }

    #[test]
    fn a_published_artifact_is_served_like_realtimexs_route() {
        let root = site();
        let (_data, store) = store_with(
            root.path(),
            json!([
                { "id": "0123456789abcdef", "dir": "site", "entry": "index.html", "status": "published" },
                { "id": "report-file", "dir": "single", "entry": "report.pdf", "status": "published" },
            ]),
        );
        for path in [
            "/ar/0123456789abcdef",
            "/ar/0123456789abcdef/",
            "/ar/0123456789ABCDEF/",
        ] {
            let answer = get(&store, path, &[]);
            assert_eq!(answer.status, 200, "{path}");
            assert_eq!(
                header(&answer, "content-type"),
                Some("text/html; charset=UTF-8")
            );
            assert_eq!(header(&answer, "cache-control"), Some("no-store"));
            assert_eq!(header(&answer, "x-robots-tag"), Some("noindex"));
            assert_eq!(header(&answer, "content-length"), Some("11"));
            assert!(matches!(
                answer.body,
                Body::File {
                    start: 0,
                    length: 11,
                    ..
                }
            ));
        }
        let js = get(&store, "/ar/0123456789abcdef/assets/app.js", &[]);
        assert_eq!(
            header(&js, "content-type"),
            Some("application/javascript; charset=UTF-8")
        );
        assert_eq!(get(&store, "/ar/0123456789abcdef/assets", &[]).status, 404);
        // A folder serves its index.html.
        assert_eq!(
            get(&store, "/ar/0123456789abcdef/%61ssets/app.js", &[]).status,
            200
        );
        // A single published file: its parent is the root, the entry is the file.
        let pdf = get(&store, "/ar/report-file", &[]);
        assert_eq!(header(&pdf, "content-type"), Some("application/pdf"));
        // Everything not servable is the same 404.
        for path in [
            "/ar/0123456789abcdef/missing.html",
            "/ar/0123456789abcdef/.env",
            "/ar/0123456789abcdef/.git/config",
            "/ar/0123456789abcdef/../outside.txt",
            "/ar/0123456789abcdef/%2e%2e/outside.txt",
            "/ar/0123456789abcdef/%252e%252e/outside.txt",
            "/ar/0123456789abcdef/assets%2fapp.js",
            "/ar/0123456789abcdef/escape",
            "/ar/0123456789abcdef/%zz",
            "/ar/unknown-artifact",
            "/ar/bad",
            "/ar",
        ] {
            let answer = get(&store, path, &[]);
            assert_eq!(answer.status, 404, "{path}");
            assert_eq!(answer.body, Body::Bytes(NOT_FOUND.as_bytes().to_vec()));
        }
        assert_eq!(
            artifact(&store, "POST", "/ar/0123456789abcdef", &[]).status,
            405
        );
        let head = artifact(&store, "HEAD", "/ar/0123456789abcdef", &[]);
        assert_eq!((head.status, &head.body), (200, &Body::Bytes(Vec::new())));
    }

    #[test]
    fn paused_revoked_and_expired_artifacts_are_not_found() {
        let root = site();
        let (_data, store) = store_with(
            root.path(),
            json!([
                { "id": "paused-one", "dir": "site", "entry": "index.html", "status": "paused" },
                { "id": "revoked-one", "dir": "site", "entry": "index.html", "status": "revoked" },
                { "id": "expired-one", "dir": "site", "entry": "index.html", "status": "published", "expiresAt": "2020-01-01T00:00:00Z" },
                { "id": "later-one", "dir": "site", "entry": "index.html", "status": "published", "expiresAt": "2999-01-01T00:00:00Z" },
            ]),
        );
        for id in ["paused-one", "revoked-one", "expired-one"] {
            assert_eq!(get(&store, &format!("/ar/{id}"), &[]).status, 404, "{id}");
        }
        assert_eq!(get(&store, "/ar/later-one", &[]).status, 200);
        assert_eq!(published_count(&store), 1);
    }

    #[test]
    fn ranges_and_conditional_requests() {
        let root = site();
        let (_data, store) = store_with(
            root.path(),
            json!([{ "id": "0123456789abcdef", "dir": "site", "entry": "index.html", "status": "published" }]),
        );
        let path = "/ar/0123456789abcdef/assets/data.bin";
        let part = get(&store, path, &[("Range", "bytes=10-19")]);
        assert_eq!(part.status, 206);
        assert_eq!(header(&part, "content-range"), Some("bytes 10-19/100"));
        assert!(matches!(
            part.body,
            Body::File {
                start: 10,
                length: 10,
                ..
            }
        ));
        let tail = get(&store, path, &[("Range", "bytes=-5")]);
        assert!(matches!(
            tail.body,
            Body::File {
                start: 95,
                length: 5,
                ..
            }
        ));
        let open = get(&store, path, &[("Range", "bytes=90-")]);
        assert!(matches!(
            open.body,
            Body::File {
                start: 90,
                length: 10,
                ..
            }
        ));
        let beyond = get(&store, path, &[("Range", "bytes=200-300")]);
        assert_eq!(
            (beyond.status, header(&beyond, "content-range")),
            (416, Some("bytes */100"))
        );
        assert_eq!(get(&store, path, &[("Range", "bytes=0-1,5-6")]).status, 200);
        let whole = get(&store, path, &[]);
        let etag = header(&whole, "etag").unwrap().to_string();
        assert!(etag.starts_with("W/\""));
        assert_eq!(get(&store, path, &[("If-None-Match", &etag)]).status, 304);
        let modified = header(&whole, "last-modified").unwrap().to_string();
        assert_eq!(
            get(&store, path, &[("If-Modified-Since", &modified)]).status,
            304
        );
        assert_eq!(
            get(&store, path, &[("If-None-Match", "W/\"other\"")]).status,
            200
        );
    }

    #[test]
    fn the_widgets_assets_come_from_realtimexs_folder() {
        let root = site();
        let (_data, store) = store_with(root.path(), json!([]));
        assert!(serves(&store, "e", "/embed/realtimex-chat-widget.min.js"));
        assert!(!serves(&store, "e", "/api/embed/x/stream-chat"));
        assert!(serves(&store, "a", "/ar/x"));
        assert!(!serves(&store, "a", "/api/ping"));
        let js = embed_asset(&store, "GET", "/embed/realtimex-chat-widget.min.js", &[]);
        assert_eq!(js.status, 200);
        assert_eq!(header(&js, "cache-control"), Some("public, max-age=300"));
        assert_eq!(
            header(&js, "content-type"),
            Some("application/javascript; charset=UTF-8")
        );
        assert_eq!(
            embed_asset(&store, "GET", "/embed/../outside.txt", &[]).status,
            404
        );
        assert_eq!(embed_asset(&store, "GET", "/embed", &[]).status, 404);
        assert_eq!(embed_asset(&store, "POST", "/embed/x.js", &[]).status, 405);
    }

    const SITE: &str = "0123456789abcdef";

    fn one_site(dir: &str) -> Value {
        json!([{ "id": SITE, "dir": dir, "entry": "index.html", "status": "published" }])
    }

    #[test]
    fn an_artifact_clear_of_every_restricted_folder_is_served_whole() {
        let root = site();
        std::fs::create_dir_all(root.path().join("protected")).unwrap();
        let (_data, store) = store_restricting(
            root.path(),
            one_site("site"),
            &["protected", "gone-since", "single"],
        );
        let page = get(&store, &format!("/ar/{SITE}"), &[]);
        assert_eq!(page.status, 200);
        assert_eq!(sent(page), b"<h1>hi</h1>");
        let head = artifact(&store, "HEAD", &format!("/ar/{SITE}"), &[]);
        assert_eq!((head.status, sent(head)), (200, Vec::new()));
        let path = format!("/ar/{SITE}/assets/data.bin");
        let part = get(&store, &path, &[("Range", "bytes=10-19")]);
        assert_eq!((part.status, sent(part)), (206, vec![0u8; 10]));
        let etag = header(&get(&store, &path, &[]), "etag")
            .unwrap()
            .to_string();
        assert_eq!(get(&store, &path, &[("If-None-Match", &etag)]).status, 304);
        let embed = embed_asset(&store, "GET", "/embed/realtimex-chat-widget.min.js", &[]);
        assert_eq!(sent(embed), b"widget");
    }

    #[test]
    fn a_root_that_is_holds_or_lies_in_a_restricted_folder_is_not_found() {
        let root = site();
        for (dir, restricted) in [
            ("site", "site"),
            ("site", "site/assets"),
            ("site/assets", "site"),
            // Only as stored: it no longer exists, and still counts.
            ("site", "site/removed-later"),
        ] {
            let (_data, store) = store_restricting(root.path(), one_site(dir), &[restricted]);
            for path in [format!("/ar/{SITE}"), format!("/ar/{SITE}/app.js")] {
                let answer = get(&store, &path, &[]);
                assert_eq!(
                    answer.status, 404,
                    "{dir} with {restricted} restricted: {path}"
                );
                assert_eq!(answer.body, Body::Bytes(NOT_FOUND.as_bytes().to_vec()));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_root_swapped_for_a_link_into_a_restricted_folder_is_not_found() {
        let root = site();
        let protected = root.path().join("protected");
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::write(protected.join("index.html"), "secret").unwrap();
        let (_data, store) = store_restricting(root.path(), one_site("site"), &["protected"]);
        assert_eq!(get(&store, &format!("/ar/{SITE}"), &[]).status, 200);
        // After the push, with RealTimeX closed: the folder becomes a link to the protected one.
        std::fs::rename(root.path().join("site"), root.path().join("site-before")).unwrap();
        std::os::unix::fs::symlink(&protected, root.path().join("site")).unwrap();
        let answer = get(&store, &format!("/ar/{SITE}"), &[]);
        assert_eq!(answer.status, 404);
        assert_eq!(answer.body, Body::Bytes(NOT_FOUND.as_bytes().to_vec()));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_links_into_a_restricted_folder_is_not_found() {
        let root = site();
        let protected = root.path().join("protected");
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::write(protected.join("notes.html"), "secret").unwrap();
        let site_dir = root.path().join("site");
        std::os::unix::fs::symlink(protected.join("notes.html"), site_dir.join("notes.html"))
            .unwrap();
        std::os::unix::fs::symlink(&protected, site_dir.join("docs")).unwrap();
        let (_data, store) = store_restricting(root.path(), one_site("site"), &["protected"]);
        for path in ["notes.html", "docs/notes.html", "docs"] {
            assert_eq!(
                get(&store, &format!("/ar/{SITE}/{path}"), &[]).status,
                404,
                "{path}"
            );
        }
        assert_eq!(get(&store, &format!("/ar/{SITE}"), &[]).status, 200);
    }

    /// A sibling folder that differs only in case (a case-sensitive volume: Linux, or a
    /// case-sensitive APFS one) is outside the root, even with nothing restricted.
    #[cfg(unix)]
    #[test]
    fn a_link_into_a_folder_that_differs_only_in_case_is_not_found() {
        let root = site();
        let other = root.path().join("Site");
        if std::fs::create_dir(&other).is_err() {
            // A case-insensitive volume: "Site" is "site", there is no second folder.
            return;
        }
        std::fs::write(other.join("private.txt"), "private").unwrap();
        std::os::unix::fs::symlink("../Site/private.txt", root.path().join("site/leak.txt"))
            .unwrap();
        let (_data, store) = store_with(root.path(), one_site("site"));
        let answer = get(&store, &format!("/ar/{SITE}/leak.txt"), &[]);
        assert_eq!(answer.status, 404);
        assert_eq!(answer.body, Body::Bytes(NOT_FOUND.as_bytes().to_vec()));
        assert_eq!(get(&store, &format!("/ar/{SITE}"), &[]).status, 200);
    }

    #[cfg(unix)]
    #[test]
    fn a_path_replaced_after_its_file_was_opened_sends_only_the_opened_file() {
        let root = site();
        let (_data, store) = store_with(root.path(), one_site("site"));
        let answer = get(&store, &format!("/ar/{SITE}"), &[]);
        assert_eq!(answer.status, 200);
        let page = root.path().join("site/index.html");
        let swap = root.path().join("swap.html");
        std::fs::write(&swap, "<h1>replaced</h1>").unwrap();
        std::fs::rename(&swap, &page).unwrap();
        assert_eq!(sent(answer), b"<h1>hi</h1>");
        let part = get(&store, &format!("/ar/{SITE}"), &[("Range", "bytes=4-11")]);
        std::fs::remove_file(&page).unwrap();
        assert_eq!(sent(part), b"replaced");
    }

    #[test]
    fn only_a_push_that_changes_the_list_ends_the_bodies_from_the_last_one() {
        let root = site();
        let (_data, store) = store_with(root.path(), one_site("site"));
        let fence = |answer: Answer| match answer.body {
            Body::File { fence, .. } => fence,
            body => panic!("{body:?}"),
        };
        let streaming = fence(get(&store, &format!("/ar/{SITE}"), &[]));
        // RealTimeX starting again pushes the same list.
        set(&store, &push(root.path(), one_site("site"), &[])).unwrap();
        assert!(streaming.holds());
        set(&store, &push(root.path(), one_site("site"), &["elsewhere"])).unwrap();
        assert!(!streaming.holds());
        let next = fence(get(&store, &format!("/ar/{SITE}"), &[]));
        assert!(next.holds());
        set(&store, &push(root.path(), json!([]), &["elsewhere"])).unwrap();
        assert!(!next.holds());
    }

    #[test]
    fn restricted_folders_still_hold_after_a_restart_without_realtimex() {
        let root = site();
        let data = tempfile::tempdir().unwrap();
        {
            let store = Store::open(data.path()).unwrap();
            set(&store, &push(root.path(), one_site("site"), &["site"])).unwrap();
        }
        let store = Store::open(data.path()).unwrap();
        assert_eq!(get(&store, &format!("/ar/{SITE}"), &[]).status, 404);
        assert_eq!(
            store.get(KEY).unwrap()["restrictedRoots"],
            json!([root.path().join("site").to_str()])
        );
    }

    #[test]
    fn restricted_roots_are_absolute_paths_and_optional() {
        let body = json!({ "items": [] });
        assert_eq!(parse(&body).unwrap()["restrictedRoots"], json!([]));
        let too_many: Vec<String> = (0..=MAX_RESTRICTED_ROOTS)
            .map(|n| format!("/r/{n}"))
            .collect();
        for roots in [
            json!("/one"),
            json!(["relative"]),
            json!([1]),
            json!(["/nul\0"]),
            json!(too_many),
        ] {
            assert!(
                parse(&json!({ "items": [], "restrictedRoots": roots })).is_err(),
                "{roots}"
            );
        }
    }

    #[test]
    fn bad_pushes_are_refused() {
        for body in [
            json!({}),
            json!({ "items": [{ "id": "short", "dir": "/x", "entry": "a.html", "status": "published" }] }),
            json!({ "items": [{ "id": "valid-id", "dir": "relative", "entry": "a.html", "status": "published" }] }),
            json!({ "items": [{ "id": "valid-id", "dir": "/x", "entry": "/abs.html", "status": "published" }] }),
            json!({ "items": [{ "id": "valid-id", "dir": "/x", "entry": "a.html", "status": "live" }] }),
            json!({ "items": [{ "id": "valid-id", "dir": "/x", "entry": "a.html", "status": "published", "expiresAt": "soon" }] }),
            json!({ "items": [], "embedAssetsDir": "relative" }),
        ] {
            assert!(parse(&body).is_err(), "{body}");
        }
    }
}
