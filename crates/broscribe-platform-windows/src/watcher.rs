use broscribe_platform::{
    ChangeCallback, LibraryWatcher, Result, WatchHandle,
    watch::{ChangeDebouncer, LibraryChangeFilter, RawChange},
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
pub struct WinLibraryWatcher;
struct Handle {
    watcher: Option<RecommendedWatcher>,
    debouncer: Option<ChangeDebouncer>,
}
impl Handle {
    fn shutdown(&mut self) {
        self.watcher.take();
        if let Some(debouncer) = self.debouncer.take() {
            debouncer.shutdown();
        }
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
impl WatchHandle for Handle {
    fn stop(mut self: Box<Self>) {
        self.shutdown();
    }
}
impl LibraryWatcher for WinLibraryWatcher {
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
            .map_err(|e| crate::error("creating ReadDirectoryChangesW watcher", e))?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| crate::error("watching library", e))?;
        Ok(Box::new(Handle {
            watcher: Some(watcher),
            debouncer: Some(debouncer),
        }))
    }
}
