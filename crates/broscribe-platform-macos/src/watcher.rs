//! Library watcher on FSEvents (file-level events on a private serial
//! dispatch queue), filtered and debounced by
//! [`broscribe_platform::watch::ChangeDebouncer`].

use std::ffi::{CStr, OsStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::mpsc::Sender;

use broscribe_platform::watch::{ChangeDebouncer, LibraryChangeFilter, RawChange};
use broscribe_platform::{ChangeCallback, Error, LibraryWatcher, Result, WatchHandle};
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2_core_foundation::{CFArray, CFString};
use objc2_core_services::{
    ConstFSEventStreamRef, FSEventStreamContext, FSEventStreamCreate, FSEventStreamEventFlags,
    FSEventStreamEventId, FSEventStreamInvalidate, FSEventStreamRef, FSEventStreamRelease,
    FSEventStreamSetDispatchQueue, FSEventStreamStart, FSEventStreamStop,
    kFSEventStreamCreateFlagFileEvents, kFSEventStreamCreateFlagNoDefer,
    kFSEventStreamEventFlagEventIdsWrapped, kFSEventStreamEventFlagKernelDropped,
    kFSEventStreamEventFlagMustScanSubDirs, kFSEventStreamEventFlagRootChanged,
    kFSEventStreamEventFlagUserDropped, kFSEventStreamEventIdSinceNow,
};

/// FSEvents' own coalescing; the debouncer adds the 500 ms quiet period.
const LATENCY_S: f64 = 0.1;

pub struct MacLibraryWatcher;

impl LibraryWatcher for MacLibraryWatcher {
    fn watch(&self, root: &Path, on_change: ChangeCallback) -> Result<Box<dyn WatchHandle>> {
        if !root.is_dir() {
            return Err(Error::NotFound(format!(
                "library folder {}",
                root.display()
            )));
        }
        let filter = LibraryChangeFilter::new(root);
        let watched = filter
            .canonical_root()
            .to_str()
            .ok_or_else(|| Error::InvalidInput(format!("non-UTF-8 path {}", root.display())))?
            .to_owned();
        let debouncer = ChangeDebouncer::spawn(filter, on_change)?;

        // Owned by the handle; freed after the stream is gone.
        let sender: *mut Sender<RawChange> = Box::into_raw(Box::new(debouncer.sender()));
        let mut context = FSEventStreamContext {
            version: 0,
            info: sender.cast::<c_void>(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        let paths = CFArray::from_retained_objects(&[CFString::from_str(&watched)]);
        // SAFETY: the callback matches FSEventStreamCallback, `context` is
        // copied by FSEvents, and `info` stays valid until the stream is
        // invalidated and drained in `MacWatchHandle::finish`.
        let stream = unsafe {
            FSEventStreamCreate(
                None,
                Some(on_events),
                &mut context,
                AsRef::<CFArray>::as_ref(&*paths),
                kFSEventStreamEventIdSinceNow,
                LATENCY_S,
                kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagNoDefer,
            )
        };
        if stream.is_null() {
            // SAFETY: never handed to FSEvents.
            drop(unsafe { Box::from_raw(sender) });
            return Err(Error::Internal(format!(
                "FSEventStreamCreate failed for {watched}"
            )));
        }
        let queue = DispatchQueue::new("org.broscribe.library-watch", DispatchQueueAttr::SERIAL);
        let mut handle = MacWatchHandle {
            stream,
            queue,
            sender,
            debouncer: Some(debouncer),
            started: false,
        };
        // SAFETY: `stream` is valid; the queue outlives the stream (both
        // owned by the handle, the stream released first).
        unsafe { FSEventStreamSetDispatchQueue(stream, Some(&handle.queue)) };
        // SAFETY: as above. On failure `handle` drops and releases everything.
        if !unsafe { FSEventStreamStart(stream) } {
            return Err(Error::Internal(format!(
                "FSEventStreamStart failed for {watched}"
            )));
        }
        handle.started = true;
        Ok(Box::new(handle))
    }
}

/// FSEvents callback, on the watch's dispatch queue.
unsafe extern "C-unwind" fn on_events(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: NonNull<c_void>,
    flags: NonNull<FSEventStreamEventFlags>,
    _ids: NonNull<FSEventStreamEventId>,
) {
    // SAFETY: `info` is the `Sender` installed in `watch`, alive until the
    // stream has been invalidated and its queue drained.
    let sender = unsafe { &*info.cast::<Sender<RawChange>>() };
    // SAFETY: without kFSEventStreamCreateFlagUseCFTypes, `paths` is an array
    // of `count` C strings and `flags` an array of `count` flags.
    let (paths, flags) = unsafe {
        (
            std::slice::from_raw_parts(paths.as_ptr().cast::<*const c_char>(), count),
            std::slice::from_raw_parts(flags.as_ptr(), count),
        )
    };
    let rescan = kFSEventStreamEventFlagMustScanSubDirs
        | kFSEventStreamEventFlagUserDropped
        | kFSEventStreamEventFlagKernelDropped
        | kFSEventStreamEventFlagEventIdsWrapped
        | kFSEventStreamEventFlagRootChanged;
    for (&path, &flag) in paths.iter().zip(flags) {
        let change = if flag & rescan != 0 || path.is_null() {
            RawChange::Rescan
        } else {
            // SAFETY: a NUL-terminated path owned by FSEvents for this call.
            let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
            RawChange::Path(PathBuf::from(OsStr::from_bytes(bytes)))
        };
        // Fails only while the watch is shutting down.
        let _ = sender.send(change);
    }
}

struct MacWatchHandle {
    stream: FSEventStreamRef,
    queue: DispatchRetained<DispatchQueue>,
    sender: *mut Sender<RawChange>,
    debouncer: Option<ChangeDebouncer>,
    started: bool,
}

// SAFETY: the raw stream and sender pointers are only touched by `finish`,
// which serialises with the callback through the stream's serial queue.
unsafe impl Send for MacWatchHandle {}

impl MacWatchHandle {
    fn finish(&mut self) {
        if self.stream.is_null() {
            return;
        }
        // SAFETY: `stream` is valid until released here; Stop/Invalidate
        // prevent new callbacks, and the empty sync block drains any callback
        // already queued before the sender is freed.
        unsafe {
            if self.started {
                FSEventStreamStop(self.stream);
            }
            FSEventStreamInvalidate(self.stream);
            self.queue.exec_sync(|| {});
            FSEventStreamRelease(self.stream);
            drop(Box::from_raw(self.sender));
        }
        self.stream = std::ptr::null_mut();
        // Every sender is gone now, so the debouncer thread ends (without a
        // final callback).
        if let Some(debouncer) = self.debouncer.take() {
            debouncer.shutdown();
        }
    }
}

impl WatchHandle for MacWatchHandle {
    fn stop(mut self: Box<Self>) {
        self.finish();
    }
}

impl Drop for MacWatchHandle {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    fn collect_for(rx: &mpsc::Receiver<Vec<PathBuf>>, wait: Duration) -> Vec<PathBuf> {
        let deadline = Instant::now() + wait;
        let mut all = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(batch) => all.extend(batch),
                Err(_) => break,
            }
        }
        all
    }

    #[test]
    fn reports_meeting_writes_and_queue_jobs_but_ignores_lock_and_heartbeats() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (tx, rx) = mpsc::channel();
        let handle = MacLibraryWatcher
            .watch(root, Box::new(move |paths| tx.send(paths).unwrap()))
            .unwrap();

        std::fs::write(root.join(".lock"), b"1").unwrap();
        std::fs::create_dir_all(root.join(".queue/j-1")).unwrap();
        std::fs::write(root.join(".queue/j-1/job.json"), b"{}").unwrap();
        // A new job directory wakes idle workers; the lock and job.json heartbeats don't.
        let noise = collect_for(&rx, Duration::from_millis(1500));
        let queue = root.join(".queue");
        assert!(
            noise.iter().all(|p| p == &queue),
            "unexpected changes: {noise:?}"
        );
        std::fs::write(root.join(".queue/j-1/job.json"), b"{\"a\":1}").unwrap();
        std::fs::write(root.join(".lock"), b"2").unwrap();
        let heartbeat = collect_for(&rx, Duration::from_millis(1500));
        assert!(heartbeat.is_empty(), "unexpected changes: {heartbeat:?}");

        let meeting = root.join("2026/11/2026-11-14-1000-sync-abc123");
        std::fs::create_dir_all(&meeting).unwrap();
        let started = Instant::now();
        std::fs::write(meeting.join("meeting.json"), b"{}").unwrap();
        let batch = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("no change within 2 s");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(batch.contains(&meeting), "{batch:?}");
        assert!(batch.iter().all(|p| p == &meeting), "{batch:?}");

        handle.stop();
        std::fs::write(meeting.join("meeting.json"), b"{\"x\":1}").unwrap();
        assert!(collect_for(&rx, Duration::from_millis(800)).is_empty());
    }

    #[test]
    fn missing_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            MacLibraryWatcher
                .watch(&dir.path().join("missing"), Box::new(|_| {}))
                .is_err()
        );
    }
}
