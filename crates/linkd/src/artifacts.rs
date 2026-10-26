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

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

use crate::error::ApiError;
use crate::store::Store;

pub const KEY: &str = "artifacts";
const NOT_FOUND: &str = r#"{"error":"Artifact not found."}"#;
const MAX_ITEMS: usize = 10_000;

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
    Ok(json!({ "items": kept, "embedAssetsDir": assets }))
}

pub fn set(store: &Store, value: &Value) -> Result<(), ApiError> {
    store.set(KEY, value)?;
    Ok(())
}

fn find(store: &Store, id: &str) -> Option<Published> {
    let kept = store.get(KEY)?;
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
    store
        .get(KEY)?
        .get("embedAssetsDir")?
        .as_str()
        .map(PathBuf::from)
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

#[derive(Debug, PartialEq)]
pub enum Body {
    Bytes(Vec<u8>),
    /// `length` bytes of the file from `start`.
    File {
        path: PathBuf,
        start: u64,
        length: u64,
    },
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

/// A regular file under `root` (canonical paths compared, so symlinks may point inside the root
/// but not out of it); a folder gives its index.html.
fn resolve(root: &Path, segments: &[String]) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    let mut target = root.clone();
    for segment in segments {
        target.push(segment);
    }
    let mut target = std::fs::canonicalize(&target).ok()?;
    if !target.starts_with(&root) {
        return None;
    }
    if target.is_dir() {
        target = std::fs::canonicalize(target.join("index.html")).ok()?;
        if !target.starts_with(&root) {
            return None;
        }
    }
    target.is_file().then_some(target)
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
    let Some(published) = find(store, &id).filter(|p| p.active(crate::util::now_ms())) else {
        return Answer::not_found();
    };
    let requested = if path.trim_matches('/').is_empty() {
        segments(&published.entry)
    } else {
        segments(path)
    };
    let Some(file) = requested.and_then(|segments| resolve(&published.dir, &segments)) else {
        return Answer::not_found();
    };
    let mut answer = file_answer(&file, method, headers);
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
    let file = embed_assets_dir(store).and_then(|dir| {
        segments(rest)
            .filter(|s| !s.is_empty())
            .and_then(|segments| resolve(&dir, &segments))
    });
    let Some(file) = file else {
        return text(404, "Not found.");
    };
    let mut answer = file_answer(&file, method, headers);
    answer
        .headers
        .push(("cache-control".into(), "public, max-age=300".into()));
    answer
}

/// A file as `send` (Express's sendFile) answers it: its type, Last-Modified, a weak ETag,
/// Accept-Ranges, 304 for a fresh copy, 206 for one range and 416 for one out of bounds.
fn file_answer(file: &Path, method: &str, headers: &[(String, String)]) -> Answer {
    let Ok(meta) = std::fs::metadata(file) else {
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
        ("content-type".to_string(), content_type(file).to_string()),
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
            path: file.to_path_buf(),
            start,
            length,
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
        let data = tempfile::tempdir().unwrap();
        let store = Store::open(data.path()).unwrap();
        let mut body = json!({ "items": items, "embedAssetsDir": dir.join("embed").to_str() });
        if let Some(list) = body["items"].as_array_mut() {
            for item in list {
                item["dir"] = json!(dir.join(item["dir"].as_str().unwrap()).to_str());
            }
        }
        set(&store, &parse(&body).unwrap()).unwrap();
        (data, store)
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
