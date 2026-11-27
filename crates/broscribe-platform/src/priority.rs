//! Scheduling priority (plan M4 §13.4): capture and journal threads high,
//! the live-preview process and `broscribe-worker` at utility/background.

use std::sync::Mutex;

use crate::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PriorityClass {
    /// Capture and journal threads: must keep up with the audio clock.
    Capture,
    /// Work the user is waiting for right now.
    Interactive,
    /// Long-running work with visible progress (live preview, final jobs).
    Utility,
    /// Work nobody is waiting for (batch imports, re-embedding).
    Background,
}

pub trait ProcessPriority: Send + Sync {
    /// Sets the calling thread's class.
    fn set_current_thread(&self, class: PriorityClass) -> Result<()>;
    /// Sets the whole process's class (call early, before starting threads,
    /// so new threads inherit it).
    fn set_current_process(&self, class: PriorityClass) -> Result<()>;
}

/// Who asked for which class, in call order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriorityCall {
    Thread(PriorityClass),
    Process(PriorityClass),
}

/// Records calls; changes nothing.
#[derive(Default)]
pub struct FakeProcessPriority {
    pub calls: Mutex<Vec<PriorityCall>>,
}

impl FakeProcessPriority {
    pub fn calls(&self) -> Vec<PriorityCall> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn record(&self, call: PriorityCall) {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(call);
    }
}

impl ProcessPriority for FakeProcessPriority {
    fn set_current_thread(&self, class: PriorityClass) -> Result<()> {
        self.record(PriorityCall::Thread(class));
        Ok(())
    }

    fn set_current_process(&self, class: PriorityClass) -> Result<()> {
        self.record(PriorityCall::Process(class));
        Ok(())
    }
}
