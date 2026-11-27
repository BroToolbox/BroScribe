//! `models pull`: download each pinned file into `<model>/.partial/` while
//! hashing it, check the size and SHA-256, extract `.tar.bz2` archives, then
//! move the files into place and write the `.verified` marker last. A local
//! mirror folder (`BROSCRIBE_MODEL_MIRROR`, files named by their URL's last
//! segment) is used instead of the network when it holds the file. No file
//! is read past its pinned size. One stop state, made when the pull starts,
//! holds a finite overall deadline ([`PULL_DEADLINE`] by default) and an
//! optional cancel flag; it is checked while waiting for the model's lock,
//! before every read while fetching and unpacking, and right before the
//! install, so a stopped pull removes what it staged and keeps the earlier
//! install.

use std::fs::{File, TryLockError};
use std::io::{BufReader, BufWriter, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use broscribe_core::{Error, Result};
use broscribe_platform::profile_cleanup::{MODEL_PARTIAL_RECORD, ModelPartialEntries};

use crate::execution::ExecutionContext;
use crate::registry::{ModelEntry, ModelFile};
use crate::store::{
    Health, InstalledFile, ModelStore, PARTIAL_DIR, SourceHash, VERIFIED_MARKER, VerifiedMarker,
    copy_hashing,
};

/// Local folder checked for a file before downloading it.
pub const MIRROR_ENV: &str = "BROSCRIBE_MODEL_MIRROR";

/// Default time allowed for pulling one model, from the call to the install:
/// waiting for the model's lock, fetching and unpacking every file. Generous
/// for the pinned models (hundreds of MiB) on a slow link; connect and
/// per-read timeouts still apply within it.
pub const PULL_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Longest deadline a caller may set; [`pull`] refuses a longer one.
pub const MAX_PULL_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug)]
pub struct PullOptions {
    /// `--allow-unverified-license`.
    pub allow_unverified_license: bool,
    /// Reinstall even when the model is already installed.
    pub force: bool,
    /// Local mirror folder; files are looked up by their URL's last segment.
    pub mirror: Option<PathBuf>,
    /// Time allowed for the whole pull, measured from the call: waiting for
    /// the model's lock, fetching and unpacking every file, up to the
    /// install. [`PULL_DEADLINE`] by default; at most [`MAX_PULL_DEADLINE`].
    pub deadline: Duration,
    /// Set from another thread to stop the pull. Checked while waiting for
    /// the model's lock, before every read and right before the install; a
    /// read already blocked ends at the deadline or the per-read timeout.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for PullOptions {
    fn default() -> Self {
        Self {
            allow_unverified_license: false,
            force: false,
            mirror: None,
            deadline: PULL_DEADLINE,
            cancel: None,
        }
    }
}

impl PullOptions {
    /// Options with the mirror taken from `BROSCRIBE_MODEL_MIRROR` (unset or
    /// empty: no mirror) and the default [`PULL_DEADLINE`].
    pub fn from_env(allow_unverified_license: bool) -> Self {
        Self {
            allow_unverified_license,
            mirror: std::env::var_os(MIRROR_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullOutcome {
    /// Already installed and matching the registry; nothing was fetched.
    AlreadyInstalled,
    Installed,
}

/// Installs `entry` into `<data_dir>/models/<id>/`. `progress(done, total)`
/// reports fetched bytes over the entry's pinned total. A SHA-256 mismatch
/// deletes the staged files and fails; the previous install, if any, is kept
/// until every file has been verified. Concurrent pulls of the same model,
/// from any process, are serialized by an exclusive lock on
/// `<data_dir>/models/<id>.lock`; the later one finds the model installed.
/// A pull stopped by its deadline or cancel flag, including while it waits
/// for that lock, installs nothing and keeps the earlier install. A deadline
/// above [`MAX_PULL_DEADLINE`] is refused before anything is touched.
/// Downloads use the context's shared agent and OS trust roots: without them
/// an HTTPS file fails with `trust_store_unavailable` before connecting,
/// while files found in the mirror still install offline.
pub fn pull(
    entry: &ModelEntry,
    data_dir: &Path,
    opts: &PullOptions,
    progress: &mut dyn FnMut(u64, u64),
    execution: &ExecutionContext,
) -> Result<PullOutcome> {
    if opts.deadline > MAX_PULL_DEADLINE {
        return Err(Error::InvalidInput(format!(
            "a model pull deadline of {} s exceeds the {} s limit",
            opts.deadline.as_secs(),
            MAX_PULL_DEADLINE.as_secs()
        )));
    }
    let stop = Stop {
        deadline: Instant::now() + opts.deadline,
        cancel: opts.cancel.as_deref(),
    };
    entry.check_license(opts.allow_unverified_license)?;
    if entry.files.is_empty() {
        return Err(Error::Unsupported(format!(
            "model {} has no files to download",
            entry.id
        )));
    }
    for f in &entry.files {
        check_file_name(entry, f)?;
    }
    let store = ModelStore::new(data_dir);
    let dir = store.model_dir(&entry.id)?;
    let _lock = lock_model(&store, &entry.id, stop)?;
    if !opts.force && store.doctor(entry, false)? == Health::Installed {
        return Ok(PullOutcome::AlreadyInstalled);
    }

    let partial = dir.join(PARTIAL_DIR);
    remove_dir_if_exists(&partial)?;
    std::fs::create_dir_all(&partial).map_err(|e| Error::io_at(&partial, e))?;

    // The last stop check: `install` replaces the earlier install.
    let staged = stage(entry, &partial, opts, stop, progress, execution).and_then(|files| {
        stop.check()
            .map_err(|e| Error::io(format!("installing model {}", entry.id), e))?;
        Ok(files)
    });
    let files = match staged {
        Ok(files) => files,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(e);
        }
    };
    install(&store, entry, &dir, &partial, files)?;
    Ok(PullOutcome::Installed)
}

/// How often a pull waiting for another pull's lock checks its stop state.
const LOCK_POLL: Duration = Duration::from_millis(50);

/// Waits until this process holds the model's exclusive pull lock, giving up
/// when `stop` fires; the lock is released when the returned guard is
/// dropped (or the process exits).
fn lock_model(store: &ModelStore, id: &str, stop: Stop<'_>) -> Result<PullLock> {
    let path = store.lock_path(id)?;
    std::fs::create_dir_all(store.root()).map_err(|e| Error::io_at(store.root(), e))?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| Error::io_at(&path, e))?;
    loop {
        stop.check()
            .map_err(|e| Error::io(format!("waiting for the pull lock of model {id}"), e))?;
        match file.try_lock() {
            Ok(()) => return Ok(PullLock(file)),
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(stop.remaining().min(LOCK_POLL));
            }
            Err(TryLockError::Error(e)) => return Err(Error::io_at(&path, e)),
        }
    }
}

/// A held model pull lock. Unlocks explicitly on drop: flock locks belong to
/// the open file description, and a child forked by another thread holds a
/// copy of it until its exec closes it (close-on-exec). Closing only this
/// handle would leave the lock held.
struct PullLock(File);

impl Drop for PullLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Why a pull stops early: its deadline passed or it was cancelled.
#[derive(Clone, Copy)]
struct Stop<'a> {
    deadline: Instant,
    cancel: Option<&'a AtomicBool>,
}

impl Stop<'_> {
    fn check(&self) -> std::io::Result<()> {
        if self.cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
            return Err(std::io::Error::other("the pull was cancelled"));
        }
        if Instant::now() >= self.deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the pull ran past its deadline",
            ));
        }
        Ok(())
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// `inner`, checking `stop` before every read.
struct Stopping<'a, R> {
    inner: R,
    stop: Stop<'a>,
}

impl<R: Read> Read for Stopping<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stop.check()?;
        self.inner.read(buf)
    }
}

/// `inner`, refusing to yield more than `left` bytes.
struct Bounded<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // One byte past the pin is enough to tell the file is too large.
        let room = usize::try_from(self.left.saturating_add(1)).unwrap_or(usize::MAX);
        let cap = buf.len().min(room);
        let n = self.inner.read(&mut buf[..cap])?;
        if n as u64 > self.left {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the file is larger than its pinned size",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// Fetches, verifies and extracts every file into `partial`; returns the
/// installed files relative to `partial`.
fn stage(
    entry: &ModelEntry,
    partial: &Path,
    opts: &PullOptions,
    stop: Stop<'_>,
    progress: &mut dyn FnMut(u64, u64),
    execution: &ExecutionContext,
) -> Result<Vec<InstalledFile>> {
    let total = entry.total_bytes();
    let mut done = 0u64;
    progress(done, total);
    let mut partial_entries = ModelPartialEntries {
        id: entry.id.clone(),
        files: entry
            .files
            .iter()
            .map(|f| f.file_name().to_owned())
            .collect(),
        dirs: Vec::new(),
    };
    partial_entries.save(partial)?;
    let mut installed = Vec::new();
    for f in &entry.files {
        let name = f.file_name();
        let context = || format!("fetching {name} for model {}", entry.id);
        stop.check().map_err(|e| Error::io(context(), e))?;
        let dest = partial.join(name);
        let mut on_bytes = |n: u64| {
            done += n;
            progress(done, total);
        };
        let (bytes, sha) = match opts.mirror.as_deref().map(|m| m.join(name)) {
            Some(src) if src.is_file() => copy_file(&src, &dest, f.bytes, stop, &mut on_bytes)?,
            _ => download(
                execution.http_agent(&f.url)?,
                &f.url,
                &dest,
                f.bytes,
                stop,
                &mut on_bytes,
            )?,
        };
        if !sha.eq_ignore_ascii_case(&f.sha256) {
            let _ = std::fs::remove_file(&dest);
            return Err(Error::io(
                format!("verifying {name} for model {}", entry.id),
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("SHA-256 mismatch: expected {}, got {sha}", f.sha256),
                ),
            ));
        }
        if bytes != f.bytes {
            let _ = std::fs::remove_file(&dest);
            return Err(Error::io(
                format!("verifying {name} for model {}", entry.id),
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("size mismatch: {} bytes pinned, got {bytes}", f.bytes),
                ),
            ));
        }
        if f.archive {
            stop.check().map_err(|e| Error::io(context(), e))?;
            installed.extend(extract_tar_bz2(&dest, partial, stop, &mut partial_entries)?);
            std::fs::remove_file(&dest).map_err(|e| Error::io_at(&dest, e))?;
            if let Some(root) = f.path_in_archive_root.as_deref()
                && !partial.join(root).is_dir()
            {
                return Err(Error::Io {
                    context: format!("extracting {name}"),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("archive has no {root}/ folder"),
                    ),
                });
            }
        } else {
            installed.push(InstalledFile {
                path: name.to_owned(),
                bytes,
                sha256: sha,
            });
        }
    }
    Ok(installed)
}

/// Replaces the model folder's contents with the staged files, writing the
/// marker last so an interrupted install reads as not installed.
fn install(
    store: &ModelStore,
    entry: &ModelEntry,
    dir: &Path,
    partial: &Path,
    files: Vec<InstalledFile>,
) -> Result<()> {
    let marker_path = dir.join(VERIFIED_MARKER);
    match std::fs::remove_file(&marker_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io_at(&marker_path, e)),
    }
    for item in read_dir(dir)? {
        if item.file_name() == PARTIAL_DIR {
            continue;
        }
        let path = item.path();
        let is_dir = item
            .file_type()
            .map_err(|e| Error::io_at(&path, e))?
            .is_dir();
        if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        }
        .map_err(|e| Error::io_at(&path, e))?;
    }
    for item in read_dir(partial)? {
        if item.file_name() == MODEL_PARTIAL_RECORD {
            continue;
        }
        let from = item.path();
        let to = dir.join(item.file_name());
        std::fs::rename(&from, &to).map_err(|e| Error::io_at(&from, e))?;
    }
    store.mark_verified(&VerifiedMarker {
        id: entry.id.clone(),
        sources: entry
            .files
            .iter()
            .map(|f| SourceHash {
                file: f.file_name().to_owned(),
                sha256: f.sha256.to_ascii_lowercase(),
            })
            .collect(),
        files,
    })?;
    std::fs::remove_dir_all(partial).map_err(|e| Error::io_at(partial, e))
}

fn read_dir(dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    std::fs::read_dir(dir)
        .and_then(|it| it.collect::<std::io::Result<Vec<_>>>())
        .map_err(|e| Error::io_at(dir, e))
}

fn remove_dir_if_exists(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io_at(dir, e)),
    }
}

/// A download's file name must be a plain name that cannot collide with the
/// store's own entries.
fn check_file_name(entry: &ModelEntry, f: &ModelFile) -> Result<()> {
    let name = f.file_name();
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "model {}: unusable file name in {}",
            entry.id, f.url
        )))
    }
}

fn create(path: &Path) -> Result<BufWriter<File>> {
    File::create(path)
        .map(BufWriter::new)
        .map_err(|e| Error::io_at(path, e))
}

fn finish(w: BufWriter<File>, path: &Path) -> Result<()> {
    w.into_inner()
        .map_err(|e| e.into_error())
        .and_then(|f| f.sync_all())
        .map_err(|e| Error::io_at(path, e))
}

fn copy_file(
    src: &Path,
    dest: &Path,
    pinned: u64,
    stop: Stop<'_>,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<(u64, String)> {
    let reader = Bounded {
        inner: Stopping {
            inner: File::open(src).map_err(|e| Error::io_at(src, e))?,
            stop,
        },
        left: pinned,
    };
    let mut w = create(dest)?;
    let out = copy_hashing(reader, &mut w, on_bytes).map_err(|e| Error::io_at(src, e))?;
    finish(w, dest)?;
    Ok(out)
}

/// Connect and per-read limits of a download (the pull's deadline bounds the
/// whole request).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// One download through the context's shared agent (connections reused
/// across the pull's files); an HTTP error status fails it.
fn download(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    pinned: u64,
    stop: Stop<'_>,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<(u64, String)> {
    let context = || format!("downloading {url}");
    stop.check().map_err(|e| Error::io(context(), e))?;
    // The whole request, body included, ends by the pull's deadline.
    let request = agent
        .get(url)
        .config()
        .http_status_as_error(true)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_per_read(Some(READ_TIMEOUT))
        .timeout_global(Some(stop.remaining()))
        .build();
    let response = request
        .call()
        .map_err(|e| Error::io(context(), e.into_io()))?;
    if let Some(announced) = response.body().content_length()
        && announced > pinned
    {
        return Err(Error::io(
            context(),
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the server announces {announced} bytes; {pinned} are pinned"),
            ),
        ));
    }
    let reader = Bounded {
        inner: Stopping {
            inner: response.into_body().into_reader(),
            stop,
        },
        left: pinned,
    };
    let mut w = create(dest)?;
    let out = copy_hashing(reader, &mut w, on_bytes).map_err(|e| Error::io(context(), e))?;
    finish(w, dest)?;
    Ok(out)
}

/// Extracts regular files and folders of a `.tar.bz2` into `dest`, hashing
/// each file and checking `stop` before every entry and read; other entry
/// types are skipped and paths that could leave `dest` are refused.
fn extract_tar_bz2(
    archive: &Path,
    dest: &Path,
    stop: Stop<'_>,
    entries: &mut ModelPartialEntries,
) -> Result<Vec<InstalledFile>> {
    let file = Stopping {
        inner: File::open(archive).map_err(|e| Error::io_at(archive, e))?,
        stop,
    };
    let decoder = bzip2::read::MultiBzDecoder::new(BufReader::new(file));
    let mut tar = tar::Archive::new(decoder);
    let context = || format!("extracting {}", archive.display());
    let bad = |msg: String| Error::Io {
        context: context(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, msg),
    };
    let mut out = Vec::new();
    for item in tar.entries().map_err(|e| Error::io(context(), e))? {
        stop.check().map_err(|e| Error::io(context(), e))?;
        let mut item = item.map_err(|e| Error::io(context(), e))?;
        let kind = item.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            continue;
        }
        let rel = item
            .path()
            .map_err(|e| Error::io(context(), e))?
            .into_owned();
        let parts: Vec<&str> = rel
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect::<Option<_>>()
            .ok_or_else(|| bad(format!("unsafe path {}", rel.display())))?;
        if parts.is_empty() {
            continue;
        }
        if parts[0].eq_ignore_ascii_case(MODEL_PARTIAL_RECORD) {
            return Err(bad(format!(
                "reserved model download path {}",
                rel.display()
            )));
        }
        let name = parts.join("/");
        let paths = if kind.is_dir() {
            &mut entries.dirs
        } else {
            &mut entries.files
        };
        if !paths.contains(&name) {
            paths.push(name.clone());
            entries.save(dest)?;
        }
        let target = parts.iter().fold(dest.to_path_buf(), |p, s| p.join(s));
        if kind.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| Error::io_at(&target, e))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io_at(parent, e))?;
        }
        let mut w = create(&target)?;
        let reader = Stopping {
            inner: &mut item,
            stop,
        };
        let (bytes, sha256) =
            copy_hashing(reader, &mut w, |_| {}).map_err(|e| Error::io(context(), e))?;
        finish(w, &target)?;
        out.push(InstalledFile {
            path: name,
            bytes,
            sha256,
        });
        #[cfg(test)]
        tests::entry_extracted();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Registry, Role};
    use std::cell::RefCell;
    use std::sync::mpsc;

    thread_local! {
        /// Runs after each archive file is extracted on this thread: the
        /// barrier of the unpacking cancellation test.
        static ON_ENTRY: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    }

    pub(super) fn entry_extracted() {
        ON_ENTRY.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook();
            }
        });
    }

    fn entry(id: &str, files: Vec<ModelFile>) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            display_name: None,
            roles: vec![Role::Vad],
            runtime: "onnx".into(),
            files,
            platforms: Vec::new(),
            languages: vec!["*".into()],
            license: "MIT".into(),
            license_verified: true,
            accepts_hotwords: false,
            params: toml::Table::new(),
            live_model: None,
        }
    }

    fn plain(name: &str, bytes: &[u8]) -> ModelFile {
        ModelFile {
            url: format!("https://models.invalid/files/{name}"),
            sha256: crate::store::hex(&<sha2::Sha256 as sha2::Digest>::digest(bytes)),
            bytes: bytes.len() as u64,
            archive: false,
            path_in_archive_root: None,
        }
    }

    fn tar_bz2(root: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        let mut b = tar::Builder::new(enc);
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, format!("{root}/{name}"), *data)
                .unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    fn opts(mirror: &Path) -> PullOptions {
        PullOptions {
            mirror: Some(mirror.to_path_buf()),
            ..PullOptions::default()
        }
    }

    /// No OS roots: only mirror and loopback `http://` files can install.
    fn x() -> ExecutionContext {
        crate::execution::test_context().1
    }

    #[test]
    fn interrupted_model_pull_cleanup_uses_the_written_file_list() {
        let mirror = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let archive = tar_bz2(
            "synthetic",
            &[("nested/tiny.onnx", b"synthetic"), ("later.onnx", b"later")],
        );
        let mut file = plain("synthetic.tar.bz2", &archive);
        file.archive = true;
        std::fs::write(mirror.path().join(file.file_name()), &archive).unwrap();
        let model = entry("tiny-1.0", vec![file]);
        ON_ENTRY.replace(Some(Box::new(|| {
            panic!("simulated interruption during extraction")
        })));
        let crash = std::panic::catch_unwind(|| {
            pull(
                &model,
                data.path(),
                &opts(mirror.path()),
                &mut |_, _| {},
                &x(),
            )
        });
        ON_ENTRY.replace(None);
        assert!(crash.is_err());
        let partial = ModelStore::new(data.path())
            .model_dir(&model.id)
            .unwrap()
            .join(PARTIAL_DIR);
        assert!(partial.join("synthetic/nested/tiny.onnx").is_file());
        assert!(partial.join(MODEL_PARTIAL_RECORD).is_file());
        let kept = partial.join("synthetic/nested/personal.txt");
        std::fs::write(&kept, "synthetic user file").unwrap();
        let cleanup = broscribe_platform::profile_cleanup::clean(
            data.path(),
            "data",
            true,
            &[],
            cfg!(windows),
        )
        .unwrap();
        assert_eq!(cleanup.kept, vec![kept.clone()]);
        assert!(!partial.join("synthetic/nested/tiny.onnx").exists());
        assert!(!partial.join("synthetic.tar.bz2").exists());
        assert_eq!(
            std::fs::read_to_string(kept).unwrap(),
            "synthetic user file"
        );
    }

    #[test]
    fn pull_never_reads_past_the_pinned_size() {
        let mirror = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut grown = b"tiny model".to_vec();
        grown.extend(std::iter::repeat_n(b'!', 1 << 20));
        std::fs::write(mirror.path().join("tiny.onnx"), &grown).unwrap();
        let e = entry("tiny", vec![plain("tiny.onnx", b"tiny model")]);
        let mut most = 0;
        let err = pull(
            &e,
            data.path(),
            &opts(mirror.path()),
            &mut |done, _| most = most.max(done),
            &x(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("pinned"), "{err}");
        assert!(most <= e.total_bytes(), "read {most} bytes past the pin");
        let store = ModelStore::new(data.path());
        assert_eq!(store.doctor(&e, false).unwrap(), Health::Missing);
        assert!(!store.model_dir("tiny").unwrap().join(PARTIAL_DIR).exists());
    }

    #[test]
    fn pull_stops_when_cancelled_or_past_its_deadline() {
        let mirror = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(mirror.path().join("tiny.onnx"), b"tiny model").unwrap();
        let e = entry("tiny", vec![plain("tiny.onnx", b"tiny model")]);
        let store = ModelStore::new(data.path());

        let cancel = Arc::new(AtomicBool::new(true));
        let cancelled = PullOptions {
            cancel: Some(Arc::clone(&cancel)),
            ..opts(mirror.path())
        };
        let err = pull(&e, data.path(), &cancelled, &mut |_, _| {}, &x()).unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
        assert_eq!(store.doctor(&e, false).unwrap(), Health::Missing);

        let late = PullOptions {
            deadline: Duration::ZERO,
            ..opts(mirror.path())
        };
        let err = pull(&e, data.path(), &late, &mut |_, _| {}, &x()).unwrap_err();
        assert!(err.to_string().contains("deadline"), "{err}");
        assert_eq!(store.doctor(&e, false).unwrap(), Health::Missing);
        assert!(!store.model_dir("tiny").unwrap().join(PARTIAL_DIR).exists());

        cancel.store(false, Ordering::Release);
        assert_eq!(
            pull(&e, data.path(), &cancelled, &mut |_, _| {}, &x()).unwrap(),
            PullOutcome::Installed
        );
    }

    /// A loopback server answering one request with `head` then `body`,
    /// then holding the connection open for `hold`.
    fn serve(head: String, body: Vec<u8>, hold: Duration) -> String {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                line.clear();
            }
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
            std::thread::sleep(hold);
        });
        format!("http://127.0.0.1:{port}/files/tiny.onnx")
    }

    #[test]
    fn download_refuses_an_announced_size_above_the_pin() {
        let data = tempfile::tempdir().unwrap();
        let mut f = plain("tiny.onnx", b"tiny model");
        f.url = serve(
            "HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n".into(),
            vec![0; 1 << 20],
            Duration::ZERO,
        );
        let e = entry("tiny", vec![f]);
        let mut most = 0;
        let err = pull(
            &e,
            data.path(),
            &PullOptions::default(),
            &mut |done, _| most = most.max(done),
            &x(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("pinned"), "{err}");
        assert_eq!(most, 0, "nothing of an oversized reply is read");
    }

    #[test]
    fn a_stalled_download_ends_at_the_total_deadline() {
        let data = tempfile::tempdir().unwrap();
        let mut f = plain("tiny.onnx", b"tiny model");
        // Headers and part of the body, then silence well inside the
        // per-read timeout.
        f.url = serve(
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n".into(),
            b"tiny".to_vec(),
            Duration::from_secs(20),
        );
        let e = entry("tiny", vec![f]);
        let started = Instant::now();
        let opts = PullOptions {
            deadline: Duration::from_millis(500),
            ..PullOptions::default()
        };
        let err = pull(&e, data.path(), &opts, &mut |_, _| {}, &x()).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10), "{err}");
        let store = ModelStore::new(data.path());
        assert_eq!(store.doctor(&e, false).unwrap(), Health::Missing);
        assert!(!store.model_dir("tiny").unwrap().join(PARTIAL_DIR).exists());
    }

    #[test]
    fn pull_from_mirror_verifies_and_is_idempotent() {
        let mirror = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(mirror.path().join("tiny.onnx"), b"tiny model").unwrap();
        let archive = tar_bz2("tiny-root", &[("a.txt", b"alpha"), ("sub/b.bin", b"beta")]);
        std::fs::write(mirror.path().join("tiny-extra.tar.bz2"), &archive).unwrap();
        let mut arch = plain("tiny-extra.tar.bz2", &archive);
        arch.archive = true;
        arch.path_in_archive_root = Some("tiny-root".into());
        let e = entry("tiny", vec![plain("tiny.onnx", b"tiny model"), arch]);

        let mut seen = Vec::new();
        let outcome = pull(
            &e,
            data.path(),
            &opts(mirror.path()),
            &mut |d, t| seen.push((d, t)),
            &x(),
        )
        .unwrap();
        assert_eq!(outcome, PullOutcome::Installed);
        assert_eq!(seen.last(), Some(&(e.total_bytes(), e.total_bytes())));

        let store = ModelStore::new(data.path());
        let dir = store.model_dir("tiny").unwrap();
        assert_eq!(std::fs::read(dir.join("tiny.onnx")).unwrap(), b"tiny model");
        assert_eq!(
            std::fs::read(dir.join("tiny-root/sub/b.bin")).unwrap(),
            b"beta"
        );
        assert!(!dir.join("tiny-extra.tar.bz2").exists());
        assert!(!dir.join(PARTIAL_DIR).exists());
        assert_eq!(store.content_dir(&e).unwrap(), dir.join("tiny-root"));
        let marker = store.read_marker("tiny").unwrap().unwrap();
        assert!(marker.matches(&e));
        assert_eq!(marker.files.len(), 3);
        assert_eq!(store.doctor(&e, true).unwrap(), Health::Installed);

        assert_eq!(
            pull(&e, data.path(), &opts(mirror.path()), &mut |_, _| {}, &x()).unwrap(),
            PullOutcome::AlreadyInstalled
        );

        // Same size, different bytes: only a re-hash notices.
        std::fs::write(dir.join("tiny-root/a.txt"), b"ALPHA").unwrap();
        assert_eq!(store.doctor(&e, false).unwrap(), Health::Installed);
        assert!(matches!(
            store.doctor(&e, true).unwrap(),
            Health::Corrupt { .. }
        ));
        std::fs::remove_file(dir.join("tiny.onnx")).unwrap();
        assert!(matches!(
            store.doctor(&e, false).unwrap(),
            Health::Corrupt { .. }
        ));

        // A corrupt install is repaired by pulling again.
        assert_eq!(
            pull(&e, data.path(), &opts(mirror.path()), &mut |_, _| {}, &x()).unwrap(),
            PullOutcome::Installed
        );
        assert_eq!(store.doctor(&e, false).unwrap(), Health::Installed);
        assert_eq!(
            std::fs::read(dir.join("tiny-root/a.txt")).unwrap(),
            b"alpha"
        );
    }

    #[test]
    fn pull_rejects_a_bad_hash() {
        let mirror = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(mirror.path().join("tiny.onnx"), b"tampered").unwrap();
        let e = entry("tiny", vec![plain("tiny.onnx", b"tiny model")]);
        let err = pull(&e, data.path(), &opts(mirror.path()), &mut |_, _| {}, &x()).unwrap_err();
        assert!(err.to_string().contains("SHA-256 mismatch"), "{err}");
        let store = ModelStore::new(data.path());
        assert!(!store.is_installed("tiny"));
        assert!(!store.model_dir("tiny").unwrap().join(PARTIAL_DIR).exists());
        assert_eq!(store.doctor(&e, false).unwrap(), Health::Missing);
    }

    #[test]
    fn pinned_change_marks_install_corrupt() {
        let mirror = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(mirror.path().join("tiny.onnx"), b"v1").unwrap();
        let v1 = entry("tiny", vec![plain("tiny.onnx", b"v1")]);
        pull(&v1, data.path(), &opts(mirror.path()), &mut |_, _| {}, &x()).unwrap();
        let v2 = entry("tiny", vec![plain("tiny.onnx", b"v2")]);
        let store = ModelStore::new(data.path());
        assert!(matches!(
            store.doctor(&v2, false).unwrap(),
            Health::Corrupt { .. }
        ));
    }

    #[test]
    fn unverified_licence_is_refused_without_the_flag() {
        let mirror = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(mirror.path().join("tiny.onnx"), b"tiny model").unwrap();
        let mut e = entry("tiny", vec![plain("tiny.onnx", b"tiny model")]);
        e.license_verified = false;
        let mut o = opts(mirror.path());
        assert_eq!(
            pull(&e, data.path(), &o, &mut |_, _| {}, &x())
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        assert!(
            !ModelStore::new(data.path())
                .model_dir("tiny")
                .unwrap()
                .exists()
        );
        o.allow_unverified_license = true;
        assert_eq!(
            pull(&e, data.path(), &o, &mut |_, _| {}, &x()).unwrap(),
            PullOutcome::Installed
        );
    }

    #[test]
    fn unsafe_file_names_are_refused() {
        let data = tempfile::tempdir().unwrap();
        let mut f = plain("x.onnx", b"x");
        f.url = "https://models.invalid/.verified".into();
        let err = pull(
            &entry("tiny", vec![f]),
            data.path(),
            &PullOptions::default(),
            &mut |_, _| {},
            &x(),
        )
        .unwrap_err();
        assert_eq!(err.code(), "invalid_input");
    }

    #[test]
    fn https_downloads_need_the_os_trust_store_and_mirrors_do_not() {
        let data = tempfile::tempdir().unwrap();
        let e = entry("tiny", vec![plain("tiny.onnx", b"tiny model")]);
        let err = pull(
            &e,
            data.path(),
            &PullOptions::default(),
            &mut |_, _| {},
            &x(),
        )
        .unwrap_err();
        assert_eq!(err.code(), crate::TRUST_STORE_UNAVAILABLE, "{err}");
        let store = ModelStore::new(data.path());
        assert_eq!(store.doctor(&e, false).unwrap(), Health::Missing);
        assert!(!store.model_dir("tiny").unwrap().join(PARTIAL_DIR).exists());

        let mirror = tempfile::tempdir().unwrap();
        std::fs::write(mirror.path().join("tiny.onnx"), b"tiny model").unwrap();
        assert_eq!(
            pull(&e, data.path(), &opts(mirror.path()), &mut |_, _| {}, &x()).unwrap(),
            PullOutcome::Installed,
            "an offline mirror needs no trust store"
        );
    }

    #[test]
    fn builtin_file_names_are_usable() {
        for m in Registry::builtin().unwrap().models {
            for f in &m.files {
                check_file_name(&m, f).unwrap();
            }
        }
    }

    #[test]
    fn ids_that_could_leave_the_store_are_refused() {
        let mirror = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(mirror.path().join("tiny.onnx"), b"tiny model").unwrap();
        let abs = data.path().join("victim").display().to_string();
        for id in [abs.as_str(), "", "..", "a/b"] {
            let e = entry(id, vec![plain("tiny.onnx", b"tiny model")]);
            let err =
                pull(&e, data.path(), &opts(mirror.path()), &mut |_, _| {}, &x()).unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{id:?}");
        }
        assert!(std::fs::read_dir(data.path()).unwrap().next().is_none());
    }

    #[test]
    fn concurrent_pulls_of_one_model_are_serialized() {
        let mirror = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let payload = vec![7u8; 4 << 20];
        std::fs::write(mirror.path().join("big.onnx"), &payload).unwrap();
        let archive = tar_bz2("big-root", &[("a.txt", b"alpha"), ("b.bin", &payload)]);
        std::fs::write(mirror.path().join("big.tar.bz2"), &archive).unwrap();
        let mut arch = plain("big.tar.bz2", &archive);
        arch.archive = true;
        arch.path_in_archive_root = Some("big-root".into());
        let e = entry("big", vec![plain("big.onnx", &payload), arch]);

        let execution = x();
        let outcomes: Vec<PullOutcome> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    s.spawn(|| {
                        pull(
                            &e,
                            data.path(),
                            &opts(mirror.path()),
                            &mut |_, _| {},
                            &execution,
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap().unwrap())
                .collect()
        });
        let mut sorted = outcomes.clone();
        sorted.sort_by_key(|o| *o == PullOutcome::AlreadyInstalled);
        assert_eq!(
            sorted,
            [PullOutcome::Installed, PullOutcome::AlreadyInstalled],
            "{outcomes:?}"
        );
        let store = ModelStore::new(data.path());
        assert_eq!(store.doctor(&e, true).unwrap(), Health::Installed);
        assert!(!store.model_dir("big").unwrap().join(PARTIAL_DIR).exists());
    }

    #[test]
    fn production_pulls_have_a_finite_bounded_deadline() {
        for o in [PullOptions::default(), PullOptions::from_env(false)] {
            assert!(
                o.deadline > Duration::ZERO && o.deadline <= MAX_PULL_DEADLINE,
                "{o:?}"
            );
        }
        let data = tempfile::tempdir().unwrap();
        let unbounded = PullOptions {
            deadline: Duration::MAX,
            ..PullOptions::default()
        };
        let err = pull(
            &entry("tiny", vec![plain("tiny.onnx", b"tiny model")]),
            data.path(),
            &unbounded,
            &mut |_, _| {},
            &x(),
        )
        .unwrap_err();
        assert_eq!(err.code(), "invalid_input", "{err}");
        assert!(std::fs::read_dir(data.path()).unwrap().next().is_none());
    }

    /// While pull A holds the model's lock (parked in its first progress
    /// report), pulls waiting for that lock end when cancelled or at their
    /// deadline, before A is released.
    #[test]
    fn a_pull_waiting_for_the_model_lock_stops_when_cancelled_or_late() {
        let mirror = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(mirror.path().join("tiny.onnx"), b"tiny model").unwrap();
        let model = entry("tiny", vec![plain("tiny.onnx", b"tiny model")]);
        let execution = x();
        let (model, data_dir, mirror_dir, execution) =
            (&model, data.path(), mirror.path(), &execution);
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::scope(|s| {
            let a = s.spawn(move || {
                let mut held = Some(held_tx);
                let mut park = |_: u64, _: u64| {
                    if let Some(tx) = held.take() {
                        tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    }
                };
                pull(model, data_dir, &opts(mirror_dir), &mut park, execution)
            });
            held_rx.recv().unwrap();

            let cancel = Arc::new(AtomicBool::new(false));
            let waiting = [
                PullOptions {
                    cancel: Some(Arc::clone(&cancel)),
                    ..opts(mirror_dir)
                },
                PullOptions {
                    deadline: Duration::from_millis(100),
                    ..opts(mirror_dir)
                },
            ];
            for o in waiting {
                let done_tx = done_tx.clone();
                s.spawn(move || {
                    let _ = done_tx.send(pull(model, data_dir, &o, &mut |_, _| {}, execution));
                });
            }
            cancel.store(true, Ordering::Release);
            // A failure bound, not an ordering claim: a waiter that cannot
            // stop would only return after A is released.
            let ended: Vec<_> = (0..2)
                .map(|_| done_rx.recv_timeout(Duration::from_secs(60)))
                .collect();
            release_tx.send(()).unwrap();
            assert_eq!(a.join().unwrap().unwrap(), PullOutcome::Installed);

            let mut errors: Vec<String> = ended
                .into_iter()
                .map(|r| {
                    r.expect("a waiting pull outlived the lock holder")
                        .unwrap_err()
                        .to_string()
                })
                .collect();
            errors.sort_by_key(|e| e.contains("deadline"));
            assert!(errors[0].contains("cancelled"), "{errors:?}");
            assert!(errors[1].contains("deadline"), "{errors:?}");
        });
        let store = ModelStore::new(data_dir);
        assert_eq!(store.doctor(model, true).unwrap(), Health::Installed);
        assert!(!store.model_dir("tiny").unwrap().join(PARTIAL_DIR).exists());
    }

    /// Cancelling after the first unpacked entry, or after the last one
    /// (right before the install), keeps the earlier install and its marker
    /// and leaves no staged files.
    #[test]
    fn a_pull_cancelled_while_unpacking_keeps_the_earlier_install() {
        let mirror = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let archived = |name: &str, archive: &[u8]| {
            std::fs::write(mirror.path().join(name), archive).unwrap();
            let mut f = plain(name, archive);
            f.archive = true;
            f.path_in_archive_root = Some("tiny-root".into());
            f
        };
        let old = entry(
            "tiny",
            vec![archived(
                "tiny-v1.tar.bz2",
                &tar_bz2("tiny-root", &[("a.txt", b"old")]),
            )],
        );
        let new = entry(
            "tiny",
            vec![archived(
                "tiny-v2.tar.bz2",
                &tar_bz2("tiny-root", &[("a.txt", b"alpha"), ("b.txt", b"beta")]),
            )],
        );
        assert_eq!(
            pull(
                &old,
                data.path(),
                &opts(mirror.path()),
                &mut |_, _| {},
                &x()
            )
            .unwrap(),
            PullOutcome::Installed
        );
        let store = ModelStore::new(data.path());
        let dir = store.model_dir("tiny").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancellable = PullOptions {
            cancel: Some(Arc::clone(&cancel)),
            ..opts(mirror.path())
        };

        for after in [1, 2] {
            cancel.store(false, Ordering::Release);
            let (flag, mut seen) = (Arc::clone(&cancel), 0);
            ON_ENTRY.set(Some(Box::new(move || {
                seen += 1;
                if seen == after {
                    flag.store(true, Ordering::Release);
                }
            })));
            let result = pull(&new, data.path(), &cancellable, &mut |_, _| {}, &x());
            ON_ENTRY.set(None);
            let err = result.unwrap_err();
            assert!(err.to_string().contains("cancelled"), "{after}: {err}");
            assert!(
                store.read_marker("tiny").unwrap().unwrap().matches(&old),
                "{after}"
            );
            assert_eq!(store.doctor(&old, true).unwrap(), Health::Installed);
            assert_eq!(std::fs::read(dir.join("tiny-root/a.txt")).unwrap(), b"old");
            assert!(!dir.join("tiny-root/b.txt").exists(), "{after}");
            assert!(!dir.join(PARTIAL_DIR).exists(), "{after}");
        }

        cancel.store(false, Ordering::Release);
        assert_eq!(
            pull(&new, data.path(), &cancellable, &mut |_, _| {}, &x()).unwrap(),
            PullOutcome::Installed
        );
        assert_eq!(
            std::fs::read(dir.join("tiny-root/a.txt")).unwrap(),
            b"alpha"
        );
    }
}
