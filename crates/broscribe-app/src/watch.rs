//! External library changes (plan §8 items 3, 13, 17): the platform
//! [`LibraryWatcher`] reports changed paths (debounced); they are classified
//! here and applied to [`AppState`], which refreshes the affected view models.
//! Window activation triggers a full refresh as well (see [`crate::ui`]).
//!
//! Batches are classified on the watcher's thread against the library root
//! resolved once ([`LibraryPaths`]), and merged into one pending [`Changes`]
//! until the main thread takes it: however many batches arrive while the UI
//! is busy, one merged set waits (at most [`MAX_TRACKED_MEETINGS`] meeting
//! folders, beyond which everything is reloaded) and nothing is lost.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use broscribe_core::Result;
use broscribe_core::paths::{INDEX_DIR, PEOPLE_DIR, QUEUE_DIR};
use broscribe_platform::{LibraryWatcher, WatchHandle};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{Context, Task};

use crate::state::AppState;

/// Meeting folders a pending change names one by one; more become a reload
/// of everything ([`Changes::all`]).
pub const MAX_TRACKED_MEETINGS: usize = 512;

/// What a batch of changed paths touches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Meeting folders, relative to the library (`YYYY/MM/<name>`).
    pub meetings: BTreeSet<PathBuf>,
    pub people: bool,
    pub index: bool,
    pub queue: bool,
    /// The library root itself: events were dropped, reload everything.
    pub all: bool,
}

impl Changes {
    fn add(&mut self, rel: &Path) {
        let parts: Vec<&str> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect();
        match parts.as_slice() {
            [] => self.reload_all(),
            [first, ..] if *first == PEOPLE_DIR => self.people = true,
            [first, file] if *first == INDEX_DIR && file.ends_with("-shm") => {
                // SQLite's shared-memory index of the WAL: readers write it
                // too, and it never holds data of its own. A commit always
                // changes the database or its `-wal` as well.
            }
            [first, ..] if *first == INDEX_DIR => self.index = true,
            [first, ..] if *first == QUEUE_DIR => self.queue = true,
            [year, month, name, ..] if (is_digits(year, 4) && is_digits(month, 2)) && !self.all => {
                self.meetings
                    .insert([*year, *month, *name].iter().collect());
                if self.meetings.len() > MAX_TRACKED_MEETINGS {
                    self.reload_all();
                }
            }
            _ => {}
        }
    }

    /// Everything is reloaded, which covers every single meeting.
    fn reload_all(&mut self) {
        self.all = true;
        self.meetings.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.meetings.is_empty() && !self.people && !self.index && !self.queue && !self.all
    }

    /// Whether meeting lists (library, search, review queue) may be stale.
    pub fn affects_meetings(&self) -> bool {
        !self.meetings.is_empty() || self.index || self.all
    }

    /// Whether people (names, voiceprints) may be stale.
    pub fn affects_people(&self) -> bool {
        self.people || self.all
    }
}

fn is_digits(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_digit())
}

/// The library root as given and as the OS reports it (symlinks resolved,
/// e.g. `/private/var`), resolved once per watch.
#[derive(Clone, Debug)]
pub struct LibraryPaths {
    root: PathBuf,
    canonical: Option<PathBuf>,
}

impl LibraryPaths {
    /// Resolves `root` (a file-system call: once per watch, not per batch).
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            canonical: root.canonicalize().ok(),
        }
    }

    /// Classifies absolute (or library-relative) paths. Paths outside the
    /// library and other files at its top level are ignored.
    pub fn classify(&self, paths: &[PathBuf]) -> Changes {
        let mut changes = Changes::default();
        self.add_to(&mut changes, paths);
        changes
    }

    /// Adds the changes `paths` name to `changes`.
    pub fn add_to(&self, changes: &mut Changes, paths: &[PathBuf]) {
        for path in paths {
            let rel = if path.is_relative() {
                Some(path.as_path())
            } else {
                path.strip_prefix(&self.root).ok().or_else(|| {
                    self.canonical
                        .as_deref()
                        .and_then(|c| path.strip_prefix(c).ok())
                })
            };
            if let Some(rel) = rel {
                changes.add(rel);
            }
        }
    }
}

/// A running watch: the platform handle and the task applying its batches.
/// Dropping it stops both.
pub struct LibraryWatch {
    _handle: Box<dyn WatchHandle>,
    _task: Task<()>,
}

/// Changes classified on the watcher thread, waiting for the main thread.
type Pending = Arc<Mutex<Changes>>;

fn take(pending: &Pending) -> Changes {
    std::mem::take(&mut *pending.lock().unwrap_or_else(PoisonError::into_inner))
}

impl AppState {
    /// Watches the current library with `watcher`; replaces any previous
    /// watch. Batches reported while the main thread is busy are merged and
    /// applied in one [`AppState::library_changed`] call.
    pub fn attach_watcher(
        &mut self,
        watcher: &dyn LibraryWatcher,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let root = self.library().root().to_path_buf();
        let paths = LibraryPaths::new(&root);
        let pending: Pending = Arc::default();
        // At most one wake-up is outstanding: one is sent only when the
        // pending set goes from empty to non-empty, and the main thread
        // empties it after receiving that wake-up.
        let (wake, mut woken) = mpsc::unbounded::<()>();
        let sink = pending.clone();
        let handle = watcher.watch(
            &root,
            Box::new(move |batch| {
                let mut changes = sink.lock().unwrap_or_else(PoisonError::into_inner);
                let was_empty = changes.is_empty();
                paths.add_to(&mut changes, &batch);
                if was_empty && !changes.is_empty() {
                    // Fails only once the app has dropped the watch.
                    let _ = wake.unbounded_send(());
                }
            }),
        )?;
        let task = cx.spawn(async move |this, cx| {
            while woken.next().await.is_some() {
                let changes = take(&pending);
                if changes.is_empty() {
                    continue;
                }
                if this
                    .update(cx, |state, cx| state.library_changed(&changes, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.set_watch(LibraryWatch {
            _handle: handle,
            _task: task,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use broscribe_core::Library;
    use broscribe_core::index::{CursorRequest, Index, SearchQuery};
    use broscribe_core::schema::{Meeting, Source};
    use broscribe_platform::watch::LibraryChangeFilter;

    use super::*;

    #[gpui::test]
    fn batches_queued_while_busy_apply_once_and_merged(cx: &mut gpui::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        use broscribe_core::config::Config;
        use broscribe_platform::fakes::{FakeLibraryWatcher, FakePaths};
        use broscribe_platform::resolve_paths;
        use gpui::AppContext as _;

        use crate::state::AppEvent;

        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("lib")).unwrap();
        let root = library.root().to_path_buf();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Config::default(),
                    library,
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        let seen = Rc::new(RefCell::new(Vec::new()));
        let _events = cx.update(|cx| {
            let seen = seen.clone();
            cx.subscribe(&state, move |_, event: &AppEvent, _| {
                if let AppEvent::LibraryChanged(changes) = event {
                    seen.borrow_mut().push(changes.clone());
                }
            })
        });
        let watcher = FakeLibraryWatcher::default();
        state
            .update(cx, |s, cx| s.attach_watcher(&watcher, cx))
            .unwrap();

        // Three batches before the main thread runs: one merged change.
        let meeting = root.join("2026/11/m-0000a1/meeting.json");
        assert_eq!(watcher.trigger(&root, vec![meeting]), 1);
        watcher.trigger(&root, vec![root.join(".index/broscribe.sqlite-shm")]);
        watcher.trigger(&root, vec![root.join("people/p-000001/person.md")]);
        cx.run_until_parked();
        {
            let seen = seen.borrow();
            assert_eq!(seen.len(), 1, "{seen:?}");
            assert_eq!(
                seen[0].meetings.iter().collect::<Vec<_>>(),
                [&PathBuf::from("2026/11/m-0000a1")]
            );
            assert!(seen[0].people && !seen[0].index && !seen[0].all);
        }
        // Only reader noise: nothing to apply.
        watcher.trigger(&root, vec![root.join(".index/broscribe.sqlite-shm")]);
        cx.run_until_parked();
        assert_eq!(seen.borrow().len(), 1);
        // A later batch is a new change.
        watcher.trigger(&root, vec![root.join(".index/broscribe.sqlite-wal")]);
        cx.run_until_parked();
        let seen = seen.borrow();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].index && seen[1].meetings.is_empty());
    }

    #[test]
    fn classifies_library_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        let paths = LibraryPaths::new(&root);
        let changes = paths.classify(&[
            root.join("2026/11/2026-11-13-1405-untitled-7f3a2c/meeting.json"),
            root.join("2026/11/2026-11-13-1405-untitled-7f3a2c/meeting.md"),
            root.join("people/p-123456/person.md"),
            root.join(".index/broscribe.sqlite"),
            root.join("notes.txt"),
            dir.path().join("elsewhere/people/x"),
        ]);
        assert_eq!(
            changes.meetings.into_iter().collect::<Vec<_>>(),
            vec![PathBuf::from("2026/11/2026-11-13-1405-untitled-7f3a2c")]
        );
        assert!(changes.people && changes.index && !changes.queue);

        let changes = paths.classify(&[root.join(".queue/j-abcdef/job.json")]);
        assert!(changes.queue && !changes.affects_meetings());
        assert!(paths.classify(&[root.join(".lock")]).is_empty());
        let all = paths.classify(std::slice::from_ref(&root));
        assert!(all.all && all.affects_meetings() && all.affects_people());

        // The database and its WAL are index changes; the shared-memory
        // file readers write is not.
        for file in ["broscribe.sqlite-wal", "broscribe.sqlite-journal"] {
            let changes = paths.classify(&[root.join(".index").join(file)]);
            assert!(changes.index && changes.affects_meetings(), "{file}");
        }
        assert!(
            paths
                .classify(&[root.join(".index/broscribe.sqlite-shm")])
                .is_empty()
        );
    }

    #[test]
    fn merged_changes_lose_nothing_and_stay_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        let paths = LibraryPaths::new(&root);
        let mut pending = Changes::default();
        paths.add_to(&mut pending, &[root.join("2026/11/m-a/meeting.json")]);
        paths.add_to(&mut pending, &[root.join(".queue/j-1")]);
        paths.add_to(&mut pending, &[root.join("people/p-1/person.md")]);
        assert_eq!(pending.meetings.len(), 1);
        assert!(pending.queue && pending.people && !pending.all);

        // More meeting folders than tracked: everything reloads, which
        // includes each of them.
        let many: Vec<PathBuf> = (0..=MAX_TRACKED_MEETINGS)
            .map(|i| root.join(format!("2026/11/m-{i:06}/meeting.json")))
            .collect();
        paths.add_to(&mut pending, &many);
        assert!(pending.all && pending.meetings.is_empty());
        assert!(pending.queue && pending.people, "other flags are kept");
        paths.add_to(&mut pending, &[root.join("2026/11/m-b/meeting.json")]);
        assert!(pending.all && pending.meetings.is_empty());
    }

    /// Size and modification time of every file in `.index/`.
    fn index_files(root: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
        let mut files: Vec<_> = std::fs::read_dir(root.join(INDEX_DIR))
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                (entry.path(), meta.len(), meta.modified().unwrap())
            })
            .collect();
        files.sort();
        files
    }

    /// UI44: a library view's read must not look like a library change to
    /// the watch pipeline (platform filter, then [`LibraryPaths`]), or every
    /// read would trigger another one.
    #[test]
    fn reading_the_index_reports_no_change_to_the_watch() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let mut meeting = Meeting::new(
            broscribe_core::MeetingId::random(),
            Source::Import,
            broscribe_core::schema::now(),
        );
        meeting.title = Some("Synthetic".into());
        broscribe_core::ops::with_index(&lib, |tx, index| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)?;
            index.upsert_meeting(tx, &meeting)?;
            Ok(())
        })
        .unwrap();
        let read = || {
            let index = Index::open_readonly(lib.root()).unwrap();
            let page = index
                .search_meetings_page(&SearchQuery::default(), &CursorRequest::first(10))
                .unwrap();
            assert_eq!(page.items.len(), 1);
        };
        // The first reader after the writer closed may create the WAL files.
        read();
        let before = index_files(lib.root());
        // Coarse file-system clocks must not hide a write.
        std::thread::sleep(Duration::from_millis(20));
        read();
        read();
        let after = index_files(lib.root());
        let changed: Vec<PathBuf> = after
            .iter()
            .filter(|file| !before.contains(file))
            .map(|(path, ..)| path.clone())
            .collect();
        let filter = LibraryChangeFilter::new(lib.root());
        let reported: Vec<PathBuf> = changed
            .iter()
            .filter_map(|path| filter.classify(path))
            .collect();
        let changes = LibraryPaths::new(lib.root()).classify(&reported);
        assert!(
            changes.is_empty(),
            "reads changed {changed:?}, reported as {changes:?}"
        );
        // And neither stage would report them on its own.
        assert!(LibraryPaths::new(lib.root()).classify(&changed).is_empty());
    }
}
