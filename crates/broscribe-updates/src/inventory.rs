//! The authenticated extraction's contents, checked again before replacement.

use std::collections::BTreeMap;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};

use broscribe_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Cancel, Limits, codes, fail, manifest::hex_digest};

pub(crate) type Inventory = BTreeMap<PathBuf, Entry>;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Clone)]
pub(crate) struct Entry {
    mode: u32,
    #[serde(flatten)]
    kind: Kind,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Kind {
    Directory,
    File { size: u64, sha256: String },
    Symlink { size: u64, sha256: String },
}

/// Inventory checks on installed and retired trees may omit only additional,
/// regular Windows FFmpeg executables or macOS Finder metadata. An artifact's own entries stay exact.
pub(crate) fn verify(
    root: &Path,
    expected: &Inventory,
    limits: &Limits,
    cancel: &Cancel,
    exceptions: &[&str],
    finder_metadata: bool,
) -> Result<()> {
    let actual =
        capture_with_exceptions(root, limits, cancel, expected, exceptions, finder_metadata)?;
    if actual == *expected {
        return Ok(());
    }
    let changed = expected
        .keys()
        .chain(actual.keys())
        .find(|path| expected.get(*path) != actual.get(*path))
        .expect("different inventories have a differing entry");
    Err(fail(
        codes::RELEASE_TREE_UNSAFE,
        format!(
            "The release contents at {} changed: {}. Restore the release files and move out any additional files before retrying.",
            root.display(),
            changed.display()
        ),
    ))
}

pub(crate) fn capture(root: &Path, limits: &Limits, cancel: &Cancel) -> Result<Inventory> {
    capture_with_exceptions(root, limits, cancel, &Inventory::new(), &[], false)
}

fn capture_with_exceptions(
    root: &Path,
    limits: &Limits,
    cancel: &Cancel,
    expected: &Inventory,
    exceptions: &[&str],
    finder_metadata: bool,
) -> Result<Inventory> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![(root.to_path_buf(), 0)];
    let mut total = 0u64;
    let mut buffer = [0; 64 * 1024];
    while let Some((path, depth)) = pending.pop() {
        cancel.check()?;
        let relative = path
            .strip_prefix(root)
            .expect("inventory stays in its root");
        // Count the root and all implicit directories as well as archive entries.
        if entries.len() as u64 >= limits.max_entries
            || depth >= limits.max_depth
            || relative.as_os_str().as_encoded_bytes().len() > limits.max_path_bytes
        {
            return Err(fail(
                codes::ARCHIVE_TOO_LARGE,
                "the staged tree exceeds the archive limits",
            ));
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| Error::io_at(&path, e))?;
        let reparse = broscribe_platform::release_layout::is_reparse(&meta);
        let side_by_side = depth == 1
            && relative.to_str().is_some_and(|name| {
                exceptions
                    .iter()
                    .any(|file| name.eq_ignore_ascii_case(file))
            });
        let finder =
            finder_metadata && relative.file_name().is_some_and(|name| name == ".DS_Store");
        if (side_by_side || finder)
            && meta.is_file()
            && !reparse
            && !expected.contains_key(relative)
        {
            continue;
        }
        let kind = if meta.is_symlink() && depth != 0 {
            let target = fs::read_link(&path).map_err(|e| Error::io_at(&path, e))?;
            let bytes = target.as_os_str().as_encoded_bytes();
            Kind::Symlink {
                size: bytes.len() as u64,
                sha256: hex_digest(&Sha256::digest(bytes)[..]),
            }
        } else if meta.is_dir() && !reparse {
            for child in fs::read_dir(&path).map_err(|e| Error::io_at(&path, e))? {
                if (entries.len() + pending.len()) as u64 + 1 >= limits.max_entries {
                    return Err(fail(
                        codes::ARCHIVE_TOO_LARGE,
                        "the staged tree has too many entries",
                    ));
                }
                pending.push((child.map_err(|e| Error::io_at(&path, e))?.path(), depth + 1));
            }
            Kind::Directory
        } else if meta.is_file() && !reparse && depth != 0 {
            let size = meta.len();
            total = total.saturating_add(size);
            if total > limits.max_installed_bytes {
                return Err(fail(
                    codes::ARCHIVE_TOO_LARGE,
                    "the staged tree has too many bytes",
                ));
            }
            let file = File::open(&path).map_err(|e| Error::io_at(&path, e))?;
            let mut file = file.take(size.saturating_add(1));
            let mut read = 0;
            let mut digest = Sha256::new();
            loop {
                cancel.check()?;
                let length = file.read(&mut buffer).map_err(|e| Error::io_at(&path, e))?;
                if length == 0 {
                    break;
                }
                read += length as u64;
                digest.update(&buffer[..length]);
            }
            if size != read {
                return Err(fail(
                    codes::VERIFICATION_FAILED,
                    "a staged file changed while being checked",
                ));
            }
            Kind::File {
                size,
                sha256: hex_digest(&digest.finalize()[..]),
            }
        } else {
            return Err(fail(
                codes::ARCHIVE_UNSAFE,
                "the staged tree contains a special file",
            ));
        };
        entries.insert(
            relative.to_path_buf(),
            Entry {
                mode: mode(&meta),
                kind,
            },
        );
    }
    Ok(entries)
}

/// PowerShell receives the same records, including the root metadata.
pub(crate) fn windows_plan(inventory: &Inventory) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Planned<'a> {
        path: &'a Path,
        #[serde(flatten)]
        entry: &'a Entry,
    }
    let entries: Vec<_> = inventory
        .iter()
        .map(|(path, entry)| Planned { path, entry })
        .collect();
    serde_json::to_vec(&entries).map_err(|error| Error::Internal(error.to_string()))
}

fn mode(meta: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        u32::from(meta.permissions().readonly())
    }
}

#[cfg(test)]
#[test]
fn finder_metadata_exception_covers_only_additional_regular_exact_names() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("Contents");
    fs::create_dir(&folder).unwrap();
    let limits = Limits::default();
    let cancel = Cancel::new();
    let expected = capture(tmp.path(), &limits, &cancel).unwrap();
    let file = folder.join(".DS_Store");
    fs::write(&file, "synthetic Finder file").unwrap();
    assert!(verify(tmp.path(), &expected, &limits, &cancel, &[], false).is_err());
    verify(tmp.path(), &expected, &limits, &cancel, &[], true).unwrap();
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    assert!(verify(tmp.path(), &expected, &limits, &cancel, &[], true).is_err());
}
