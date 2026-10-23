//! Files only this user may read, written whole.

use std::fs;
use std::io::Write;
use std::path::Path;

/// The folder, made private (0700) where it is new.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Create `file` with `text` only if it does not exist yet (0600). Err(AlreadyExists) otherwise.
pub fn create_new(file: &Path, text: &str) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut handle = options.open(file)?;
    handle.write_all(text.as_bytes())
}

pub fn make_private(file: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(file, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

/// Write a file whole: readers never see half of it.
pub fn write_atomically(file: &Path, text: &str) -> std::io::Result<()> {
    let temp = file.with_extension(format!("{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temp);
    create_new(&temp, text)?;
    fs::rename(&temp, file)
}
