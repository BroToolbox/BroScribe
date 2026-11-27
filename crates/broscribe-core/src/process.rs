//! Process liveness, used to reclaim queue leases of processes that are gone.

/// Implemented by the platform crates; `broscribe-platform` re-exports it.
pub trait ProcessProbe: Send + Sync {
    /// Whether a process with this ID exists (including ones owned by other users).
    fn is_alive(&self, pid: u32) -> bool;
    fn current_pid(&self) -> u32;
}
