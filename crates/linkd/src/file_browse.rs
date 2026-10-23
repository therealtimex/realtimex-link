//! Get files (#2682): a viewer browses this computer's
//! Desktop, Downloads and Documents and picks files to download. Paths are relative to one of the
//! three folders, and their real location (links followed) must stay inside it.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::util::iso;

pub const FOLDERS: [&str; 3] = ["Desktop", "Downloads", "Documents"];
const MAX_ENTRIES: usize = 1000;

pub fn handles(kind: &str) -> bool {
    kind == "fs_list" || kind == "fs_pull"
}

/// The real path of `relative` inside `folder`.
pub fn resolve(home: &Path, folder: &str, relative: &str) -> Result<PathBuf, ApiError> {
    if !FOLDERS.contains(&folder) {
        return Err(ApiError::bad_request("Unknown folder"));
    }
    let root = fs::canonicalize(home.join(folder))
        .map_err(|_| ApiError::coded(404, "not-found", "Not found"))?;
    let parts: Vec<&str> = relative
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    if parts.iter().any(|part| *part == ".." || *part == ".") {
        return Err(ApiError::bad_request("Not a path in this folder"));
    }
    let wanted = parts
        .iter()
        .fold(root.clone(), |path, part| path.join(part));
    let real =
        fs::canonicalize(&wanted).map_err(|_| ApiError::coded(404, "not-found", "Not found"))?;
    if !real.starts_with(&root) {
        return Err(ApiError::new(403, "Not a path in this folder"));
    }
    Ok(real)
}

pub fn list(home: &Path, message: &Value) -> Result<Value, ApiError> {
    let folder = message.get("folder").and_then(Value::as_str).unwrap_or("");
    let relative = message.get("path").and_then(Value::as_str).unwrap_or("");
    let dir = resolve(home, folder, relative)?;
    if !dir.is_dir() {
        return Err(ApiError::bad_request("Not a folder"));
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir).map_err(ApiError::internal)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        // Links followed, as stat does.
        if let Ok(meta) = fs::metadata(entry.path()) {
            let modified = meta
                .modified()
                .ok()
                .map(|at| iso(DateTime::<Utc>::from(at)));
            entries.push((
                meta.is_dir(),
                name,
                if meta.is_dir() { 0 } else { meta.len() },
                modified,
            ));
        }
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }
    entries.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
            .then_with(|| a.1.cmp(&b.1))
    });
    let truncated = entries.len() >= MAX_ENTRIES;
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|(dir, name, size, modified)| json!({ "name": name, "dir": dir, "size": size, "modifiedAt": modified }))
        .collect();
    Ok(json!({ "folder": folder, "path": relative, "entries": entries, "truncated": truncated }))
}

/// A file to fetch: (its real path, its name, its size).
pub fn pull_target(home: &Path, message: &Value) -> Result<(PathBuf, String, u64), ApiError> {
    let folder = message.get("folder").and_then(Value::as_str).unwrap_or("");
    let relative = message.get("path").and_then(Value::as_str).unwrap_or("");
    let file = resolve(home, folder, relative)?;
    let meta = fs::metadata(&file).map_err(ApiError::internal)?;
    if !meta.is_file() {
        return Err(ApiError::bad_request("Only files can be downloaded"));
    }
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok((file, name, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_inside_the_three_folders_only() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join("Documents/Sub")).unwrap();
        fs::write(home.path().join("Documents/b.txt"), "bb").unwrap();
        fs::write(home.path().join("Documents/.hidden"), "").unwrap();
        fs::write(home.path().join("secret.txt"), "s").unwrap();
        let listed = list(home.path(), &json!({ "folder": "Documents" })).unwrap();
        let names: Vec<&str> = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Sub", "b.txt"]);
        assert_eq!(
            list(
                home.path(),
                &json!({ "folder": "Documents", "path": "../" })
            )
            .unwrap_err()
            .status,
            400
        );
        assert_eq!(
            list(home.path(), &json!({ "folder": "Home" }))
                .unwrap_err()
                .message,
            "Unknown folder"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                home.path().join("secret.txt"),
                home.path().join("Documents/link"),
            )
            .unwrap();
            assert_eq!(
                pull_target(
                    home.path(),
                    &json!({ "folder": "Documents", "path": "link" })
                )
                .unwrap_err()
                .status,
                403
            );
        }
        let (_, name, size) = pull_target(
            home.path(),
            &json!({ "folder": "Documents", "path": "b.txt" }),
        )
        .unwrap();
        assert_eq!((name.as_str(), size), ("b.txt", 2));
    }
}
