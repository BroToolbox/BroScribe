//! Library change notifications, so views refresh after CLI or worker writes.
//!
//! Implementations feed raw file-system paths to a [`ChangeDebouncer`], which
//! keeps only the paths views care about (see [`LibraryChangeFilter`]) and
//! reports them in batches after [`DEBOUNCE`] of quiet.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use broscribe_core::paths::{INDEX_DB, INDEX_DIR, PEOPLE_DIR};

use crate::{Error, Result};

/// Quiet period before a batch of changes is reported.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// Longest a batch is held while changes keep arriving.
pub const MAX_BATCH_DELAY: Duration = Duration::from_secs(2);

/// Called on a watcher thread with the changed items, deduplicated and sorted:
/// meeting folders (`<root>/YYYY/MM/<name>`), `<root>/people`,
/// `<root>/.index/broscribe.sqlite`, or `<root>` itself when everything should
/// be reloaded (events were dropped).
pub type ChangeCallback = Box<dyn Fn(Vec<PathBuf>) + Send>;

pub trait LibraryWatcher: Send + Sync {
    /// Watches the library at `root` until the handle is stopped or dropped.
    fn watch(&self, root: &Path, on_change: ChangeCallback) -> Result<Box<dyn WatchHandle>>;
}

/// A running watch. Dropping it also stops watching.
pub trait WatchHandle: Send {
    /// Stops watching; no callback runs after this returns.
    fn stop(self: Box<Self>);
}

/// Maps raw changed paths to the library items views refresh.
///
/// Ignored: the library lock, queue file writes (including lease heartbeats),
/// other index files, and paths outside meeting folders and people. Queue job
/// directory creation/removal is reported as `.queue` to wake idle workers.
#[derive(Clone, Debug)]
pub struct LibraryChangeFilter {
    root: PathBuf,
    /// The root as the OS reports it (symlinks resolved, e.g. `/private/var`).
    canonical_root: PathBuf,
}

impl LibraryChangeFilter {
    pub fn new(root: &Path) -> Self {
        let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        Self {
            root: root.to_path_buf(),
            canonical_root,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    /// The item to report for a change at `changed`, if any.
    pub fn classify(&self, changed: &Path) -> Option<PathBuf> {
        let rel = changed
            .strip_prefix(&self.canonical_root)
            .or_else(|_| changed.strip_prefix(&self.root))
            .ok()?;
        let parts: Vec<&str> = rel
            .components()
            .map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect::<Option<_>>()?;
        match parts.as_slice() {
            [first, ..] if *first == PEOPLE_DIR => Some(self.root.join(PEOPLE_DIR)),
            [".queue", job] if !job.starts_with('.') => Some(self.root.join(".queue")),
            [dir, file] if *dir == INDEX_DIR => {
                let db = Path::new(INDEX_DB).file_name()?.to_str()?;
                let base = file
                    .strip_suffix("-wal")
                    .or_else(|| file.strip_suffix("-journal"))
                    .unwrap_or(*file);
                (base == db).then(|| self.root.join(INDEX_DB))
            }
            [year, month, name, ..]
                if is_digits(year, 4) && is_digits(month, 2) && !name.starts_with('.') =>
            {
                Some(self.root.join(year).join(month).join(name))
            }
            _ => None,
        }
    }
}

fn is_digits(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit())
}

/// A raw change fed to a [`ChangeDebouncer`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RawChange {
    Path(PathBuf),
    /// Events were lost; everything must be reloaded.
    Rescan,
}

/// Collects raw changes on its own thread and reports filtered batches after
/// [`DEBOUNCE`] of quiet (or [`MAX_BATCH_DELAY`] under constant change).
pub struct ChangeDebouncer {
    tx: Option<Sender<RawChange>>,
    thread: Option<JoinHandle<()>>,
}

impl ChangeDebouncer {
    pub fn spawn(filter: LibraryChangeFilter, on_change: ChangeCallback) -> Result<Self> {
        Self::spawn_with(filter, DEBOUNCE, MAX_BATCH_DELAY, on_change)
    }

    pub fn spawn_with(
        filter: LibraryChangeFilter,
        quiet: Duration,
        max_delay: Duration,
        on_change: ChangeCallback,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<RawChange>();
        let thread = std::thread::Builder::new()
            .name("broscribe-library-watch".into())
            .spawn(move || {
                let classify = |change: RawChange| match change {
                    RawChange::Path(p) => filter.classify(&p),
                    RawChange::Rescan => Some(filter.root().to_path_buf()),
                };
                loop {
                    let Ok(first) = rx.recv() else { return };
                    let Some(first) = classify(first) else {
                        continue;
                    };
                    let mut batch = BTreeSet::from([first]);
                    let started = Instant::now();
                    let mut disconnected = false;
                    loop {
                        let left = max_delay.saturating_sub(started.elapsed());
                        if left.is_zero() {
                            break;
                        }
                        match rx.recv_timeout(quiet.min(left)) {
                            Ok(change) => batch.extend(classify(change)),
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => {
                                disconnected = true;
                                break;
                            }
                        }
                    }
                    if disconnected {
                        // The watch is stopping: no callback after `stop`.
                        return;
                    }
                    on_change(batch.into_iter().collect());
                }
            })
            .map_err(|e| Error::Io {
                context: "starting the library watch thread".into(),
                source: e,
            })?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    /// A sender for raw changes; the debouncer ends once every sender
    /// (including its own) is dropped.
    pub fn sender(&self) -> Sender<RawChange> {
        self.tx.clone().expect("sender exists until drop")
    }

    /// Drops the debouncer's own sender and waits for its thread. Every other
    /// sender must already be gone, or this blocks until they are.
    pub fn shutdown(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ChangeDebouncer {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_library_paths() {
        let root = Path::new("/lib");
        let f = LibraryChangeFilter {
            root: root.into(),
            canonical_root: "/private/lib".into(),
        };
        let meeting = root.join("2026/11/2026-11-14-1000-sync-abc123");
        assert_eq!(
            f.classify(Path::new(
                "/private/lib/2026/11/2026-11-14-1000-sync-abc123/meeting.json"
            )),
            Some(meeting.clone())
        );
        assert_eq!(
            f.classify(&root.join("2026/11/2026-11-14-1000-sync-abc123")),
            Some(meeting)
        );
        assert_eq!(
            f.classify(&root.join("people/p-1/person.md")),
            Some(root.join("people"))
        );
        assert_eq!(f.classify(&root.join("people")), Some(root.join("people")));
        assert_eq!(
            f.classify(&root.join(".index/broscribe.sqlite-wal")),
            Some(root.join(".index/broscribe.sqlite"))
        );
        for ignored in [
            ".lock",
            ".queue/j-1/job.json",
            ".queue",
            ".index/other.tmp",
            "2026",
            "2026/11",
            "2026/11/.tmp-x",
            "notes/x.md",
        ] {
            assert_eq!(f.classify(&root.join(ignored)), None, "{ignored}");
        }
        assert_eq!(f.classify(Path::new("/elsewhere/2026/11/x")), None);
    }

    #[test]
    fn queue_directory_changes_wake_workers_without_heartbeat_noise() {
        let filter = LibraryChangeFilter::new(Path::new("/lib"));
        assert_eq!(
            filter.classify(Path::new("/lib/.queue/j-123")),
            Some("/lib/.queue".into())
        );
        assert_eq!(
            filter.classify(Path::new("/lib/.queue/j-123/job.json")),
            None
        );
        assert_eq!(filter.classify(Path::new("/lib/.queue/.lock")), None);
    }

    #[test]
    fn debouncer_batches_and_filters() {
        let (tx, rx) = mpsc::channel();
        let deb = ChangeDebouncer::spawn_with(
            LibraryChangeFilter::new(Path::new("/lib")),
            Duration::from_millis(50),
            Duration::from_secs(5),
            Box::new(move |paths| tx.send(paths).unwrap()),
        )
        .unwrap();
        let s = deb.sender();
        s.send(RawChange::Path("/lib/.lock".into())).unwrap();
        s.send(RawChange::Path("/lib/people/a/person.md".into()))
            .unwrap();
        s.send(RawChange::Path("/lib/people/b/person.md".into()))
            .unwrap();
        s.send(RawChange::Path("/lib/2026/01/m/meeting.json".into()))
            .unwrap();
        let batch = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            batch,
            vec![
                PathBuf::from("/lib/2026/01/m"),
                PathBuf::from("/lib/people")
            ]
        );
        s.send(RawChange::Path("/lib/.queue/j/job.json".into()))
            .unwrap();
        s.send(RawChange::Rescan).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            vec![PathBuf::from("/lib")]
        );
        drop(s);
        deb.shutdown();
        assert!(rx.try_recv().is_err());
    }
}
