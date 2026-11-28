//! Installed models: `<data_dir>/models/<id>/`, marked installed by a
//! `.verified` file written after every downloaded file's SHA-256 matched.
//! The marker lists every installed file with its size and SHA-256, so
//! `doctor` can check an install without the (deleted) archives.

use std::fs::{self, File, TryLockError};
use std::io::Read;
use std::path::{Path, PathBuf};

use broscribe_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::registry::{ModelEntry, validate_model_id};

pub const MODELS_DIR: &str = "models";
pub const VERIFIED_MARKER: &str = ".verified";
/// Download and extraction staging folder inside a model folder.
pub const PARTIAL_DIR: &str = ".partial";

/// Contents of the `.verified` marker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedMarker {
    pub id: String,
    /// The pinned downloads this install came from.
    pub sources: Vec<SourceHash>,
    /// Every installed file, relative to the model folder (`/`-separated).
    pub files: Vec<InstalledFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceHash {
    pub file: String,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

impl VerifiedMarker {
    /// Whether this install came from exactly the entry's pinned downloads.
    pub fn matches(&self, entry: &ModelEntry) -> bool {
        self.id == entry.id
            && self.sources.len() == entry.files.len()
            && entry.files.iter().all(|f| {
                self.sources
                    .iter()
                    .any(|s| s.file == f.file_name() && s.sha256.eq_ignore_ascii_case(&f.sha256))
            })
    }
}

/// Result of [`ModelStore::doctor`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Installed,
    Missing,
    Corrupt { reason: String },
}

#[derive(Clone, Debug)]
pub struct ModelStore {
    root: PathBuf,
}

impl ModelStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join(MODELS_DIR),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<id>`; an `id` that is not a plain model ID (see
    /// [`validate_model_id`]) is refused so no path can leave the store.
    pub fn model_dir(&self, id: &str) -> Result<PathBuf> {
        validate_model_id(id)?;
        Ok(self.root.join(id))
    }

    /// `<root>/<id>.lock`, the per-model lock held by `models pull`.
    pub fn lock_path(&self, id: &str) -> Result<PathBuf> {
        validate_model_id(id)?;
        Ok(self.root.join(format!("{id}.lock")))
    }

    pub(crate) fn open_lock(&self, id: &str) -> Result<File> {
        let path = self.lock_path(id)?;
        fs::create_dir_all(&self.root).map_err(|e| Error::io_at(&self.root, e))?;
        broscribe_platform::open_regular_nofollow(
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false),
            &path,
        )
        .map_err(|e| Error::io_at(&path, e))
    }

    /// Refuses an install in progress. A missing cache needs no lock and
    /// must stay untouched even if a download starts after this probe.
    pub fn lock_for_removal(&self, entry: &ModelEntry) -> Result<Option<ModelLock>> {
        if !self.clean_model(entry, false)?.0 {
            return Ok(None);
        }
        let id = &entry.id;
        let path = self.lock_path(id)?;
        let file = self.open_lock(id)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(ModelLock(file))),
            Err(TryLockError::WouldBlock) => Err(Error::Service {
                code: "model_busy",
                message: format!(
                    "Model {id} is being downloaded or removed. Try again when it finishes."
                ),
            }),
            Err(TryLockError::Error(e)) => Err(Error::io_at(&path, e)),
        }
    }

    /// Whether this cache contains known downloadable model files.
    pub fn has_model_files(&self, entry: &ModelEntry) -> bool {
        self.clean_model(entry, false)
            .is_ok_and(|(has_files, _)| has_files)
    }

    /// Removes only files identified by the install, download or registry.
    /// The caller holds the model and config locks.
    pub fn remove_model_files(&self, entry: &ModelEntry) -> Result<bool> {
        self.clean_model(entry, true)
            .map(|(_, report)| report.changed)
    }

    pub(crate) fn mutable_model_dir(&self, id: &str) -> Result<PathBuf> {
        if std::env::consts::OS == "windows" && id.ends_with('.') {
            return Err(Error::InvalidInput(
                "Cannot change cached files for a model ID ending in a period on Windows.".into(),
            ));
        }
        self.model_dir(id)
    }

    fn clean_model(
        &self,
        entry: &ModelEntry,
        apply: bool,
    ) -> Result<(bool, broscribe_platform::profile_cleanup::Cleanup)> {
        let dir = self.mutable_model_dir(&entry.id)?;
        if !require_directory(&self.root)? || !require_directory(&dir)? {
            return Ok((false, Default::default()));
        }
        use broscribe_platform::profile_cleanup::{ModelCleanup, clean_model};
        clean_model(
            &dir,
            if apply {
                ModelCleanup::Remove
            } else {
                ModelCleanup::Inspect
            },
        )
    }

    /// The folder holding the model's files (the archive root when the model
    /// ships as an archive).
    pub fn content_dir(&self, entry: &ModelEntry) -> Result<PathBuf> {
        let dir = self.model_dir(&entry.id)?;
        Ok(match entry.content_root() {
            Some(root) => dir.join(root),
            None => dir,
        })
    }

    /// Whether the model has a `.verified` marker; an invalid ID is never
    /// installed.
    pub fn is_installed(&self, id: &str) -> bool {
        self.model_dir(id)
            .is_ok_and(|dir| dir.join(VERIFIED_MARKER).is_file())
    }

    /// Records that the model's files were downloaded and verified.
    pub fn mark_verified(&self, marker: &VerifiedMarker) -> Result<()> {
        let dir = self.model_dir(&marker.id)?;
        std::fs::create_dir_all(&dir).map_err(|e| Error::io_at(&dir, e))?;
        broscribe_core::fsutil::write_json_atomic(&dir.join(VERIFIED_MARKER), marker)
    }

    /// The marker, or `None` when the model is not installed.
    pub fn read_marker(&self, id: &str) -> Result<Option<VerifiedMarker>> {
        let path = self.model_dir(id)?.join(VERIFIED_MARKER);
        if !path.is_file() {
            return Ok(None);
        }
        broscribe_core::fsutil::read_json(&path).map(Some)
    }

    /// Checks an install: the marker must match the entry's pinned downloads
    /// and every listed file must exist with its recorded size; with `rehash`
    /// every file's SHA-256 is recomputed too.
    pub fn doctor(&self, entry: &ModelEntry, rehash: bool) -> Result<Health> {
        let dir = self.model_dir(&entry.id)?;
        let marker = match self.read_marker(&entry.id) {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(Health::Missing),
            Err(e) => {
                return Ok(Health::Corrupt {
                    reason: format!("unreadable {VERIFIED_MARKER} marker: {e}"),
                });
            }
        };
        if !marker.matches(entry) {
            return Ok(Health::Corrupt {
                reason: "installed files do not match the registry's pinned downloads".into(),
            });
        }
        for f in &marker.files {
            let path = dir.join(&f.path);
            let meta = match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => m,
                Ok(_) | Err(_) => {
                    return Ok(Health::Corrupt {
                        reason: format!("{} is missing", f.path),
                    });
                }
            };
            if meta.len() != f.bytes {
                return Ok(Health::Corrupt {
                    reason: format!("{} has {} bytes, expected {}", f.path, meta.len(), f.bytes),
                });
            }
            if rehash {
                let file = std::fs::File::open(&path).map_err(|e| Error::io_at(&path, e))?;
                let (_, sha) = hash_reader(file, |_| {}).map_err(|e| Error::io_at(&path, e))?;
                if !sha.eq_ignore_ascii_case(&f.sha256) {
                    return Ok(Health::Corrupt {
                        reason: format!("{} does not match its recorded SHA-256", f.path),
                    });
                }
            }
        }
        Ok(Health::Installed)
    }
}

fn require_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_dir() && !broscribe_platform::release_layout::is_reparse(&metadata) =>
        {
            Ok(true)
        }
        Ok(_) => Err(Error::InvalidInput(format!(
            "model cache path is not a directory: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::io_at(path, error)),
    }
}

/// The per-model lock shared with downloads. Unlock explicitly because a
/// forked child can keep the open file description alive until its exec.
pub struct ModelLock(pub(crate) File);

impl Drop for ModelLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Reads `r` to the end, returning its length and lowercase hex SHA-256;
/// `on_bytes` sees every chunk's length.
pub(crate) fn hash_reader(
    r: impl Read,
    on_bytes: impl FnMut(u64),
) -> std::io::Result<(u64, String)> {
    copy_hashing(r, &mut std::io::sink(), on_bytes)
}

/// Copies `r` into `w`, returning the byte count and lowercase hex SHA-256.
pub(crate) fn copy_hashing(
    mut r: impl Read,
    w: &mut impl std::io::Write,
    mut on_bytes: impl FnMut(u64),
) -> std::io::Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        w.write_all(&buf[..n])?;
        total += n as u64;
        on_bytes(n as u64);
    }
    w.flush()?;
    Ok((total, hex(&hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(id: &str) -> VerifiedMarker {
        VerifiedMarker {
            id: id.into(),
            sources: Vec::new(),
            files: Vec::new(),
        }
    }

    #[test]
    fn installed_only_after_marker() {
        let dir = tempfile::tempdir().unwrap();
        let s = ModelStore::new(dir.path());
        assert!(!s.is_installed("silero-vad"));
        assert_eq!(
            s.model_dir("silero-vad").unwrap(),
            dir.path().join("models/silero-vad")
        );
        assert_eq!(s.read_marker("silero-vad").unwrap(), None);
        s.mark_verified(&marker("silero-vad")).unwrap();
        assert!(s.is_installed("silero-vad"));
        assert_eq!(
            s.read_marker("silero-vad").unwrap(),
            Some(marker("silero-vad"))
        );
    }

    #[test]
    fn hashes_match_sha256() {
        let (n, sha) = hash_reader(&b"abc"[..], |_| {}).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            sha,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn content_dir_follows_archive_root() {
        let r = crate::Registry::builtin().unwrap();
        let s = ModelStore::new(Path::new("/data"));
        assert_eq!(
            s.content_dir(r.get("silero-vad").unwrap()).unwrap(),
            Path::new("/data/models/silero-vad")
        );
        assert_eq!(
            s.content_dir(r.get("pyannote-segmentation-3.0").unwrap())
                .unwrap(),
            Path::new(
                "/data/models/pyannote-segmentation-3.0/sherpa-onnx-pyannote-segmentation-3-0"
            )
        );
    }

    #[test]
    fn ids_that_could_leave_the_store_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = ModelStore::new(dir.path());
        for id in ["", "..", ".", "a/b", "/etc", "../x"] {
            assert_eq!(
                s.model_dir(id).unwrap_err().code(),
                "invalid_input",
                "{id:?}"
            );
            assert_eq!(
                s.lock_path(id).unwrap_err().code(),
                "invalid_input",
                "{id:?}"
            );
            assert_eq!(s.read_marker(id).unwrap_err().code(), "invalid_input");
            assert_eq!(
                s.mark_verified(&marker(id)).unwrap_err().code(),
                "invalid_input"
            );
            assert!(!s.is_installed(id));
        }
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}
