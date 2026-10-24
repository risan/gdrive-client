//! Temporary files next to their target, so a rename over the target stays on one file system.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The directory a file lives in; a bare file name lives in the current directory.
pub(crate) fn directory_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// `.<name>.<pid>.<counter>.<suffix>` beside `target`.
pub(crate) fn sibling(target: &Path, suffix: &str) -> io::Result<PathBuf> {
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the path has no file name"))?;

    Ok(directory_of(target).join(format!(
        ".{}.{}.{}.{suffix}",
        name.to_string_lossy(),
        process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )))
}

/// Makes a rename inside the directory durable.
pub(crate) fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}
