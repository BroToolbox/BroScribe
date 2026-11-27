use broscribe_platform::watch::{ChangeDebouncer, LibraryChangeFilter, RawChange};
use broscribe_platform::{ChangeCallback, LibraryWatcher, Result, WatchHandle};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;

#[derive(Default)]
pub struct LinuxLibraryWatcher;
struct LinuxWatchHandle {
    watcher: Option<RecommendedWatcher>,
    debouncer: Option<ChangeDebouncer>,
}
impl LinuxWatchHandle {
    fn shutdown(&mut self) {
        self.watcher.take();
        if let Some(debouncer) = self.debouncer.take() {
            debouncer.shutdown();
        }
    }
}
impl Drop for LinuxWatchHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
impl WatchHandle for LinuxWatchHandle {
    fn stop(mut self: Box<Self>) {
        self.shutdown();
    }
}
impl LibraryWatcher for LinuxLibraryWatcher {
    fn watch(&self, root: &Path, on_change: ChangeCallback) -> Result<Box<dyn WatchHandle>> {
        let debouncer = ChangeDebouncer::spawn(LibraryChangeFilter::new(root), on_change)?;
        let sender = debouncer.sender();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) if !event.need_rescan() => {
                    for path in event.paths {
                        let _ = sender.send(RawChange::Path(path));
                    }
                }
                _ => {
                    let _ = sender.send(RawChange::Rescan);
                }
            })
            .map_err(|e| crate::platform_error("creating inotify library watch", e))?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| crate::platform_error("watching library", e))?;
        Ok(Box::new(LinuxWatchHandle {
            watcher: Some(watcher),
            debouncer: Some(debouncer),
        }))
    }
}
