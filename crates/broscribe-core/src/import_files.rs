//! File types recognized by import.

use std::path::{Path, PathBuf};

/// Extensions treated as media when importing a folder (lowercase).
pub const MEDIA_EXTENSIONS: &[&str] = &[
    "wav", "mp3", "m4a", "mp4", "aac", "flac", "ogg", "oga", "opus", "mov", "m4v", "mkv", "webm",
    "aif", "aiff", "caf",
];

pub fn is_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MEDIA_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        && !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
}

pub const MANIFEST_FORMAT: &str = "broscribe-import-manifest";

/// Partitions a folder's direct entries the same way the importer selects media.
pub fn folder_media(path: &Path) -> std::io::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let (mut recordings, mut skipped): (Vec<_>, Vec<_>) = std::fs::read_dir(path)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .partition(|path| is_media(path) && path.is_file());
    recordings.sort();
    skipped.sort();
    Ok((recordings, skipped))
}
