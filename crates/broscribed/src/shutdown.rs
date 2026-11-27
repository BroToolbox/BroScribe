//! A one-way stop flag tasks and threads can wait on.

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

#[derive(Default)]
pub struct Shutdown {
    flag: AtomicBool,
    notify: Notify,
}

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_triggered(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Resolves once [`Self::trigger`] was called (at once if it already was).
    pub async fn wait(&self) {
        loop {
            // Created before the check: `notify_waiters` reaches it even
            // before it is first polled.
            let notified = self.notify.notified();
            if self.is_triggered() {
                return;
            }
            notified.await;
        }
    }
}
