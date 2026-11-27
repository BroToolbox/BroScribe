//! Process-tree containment for helper children (agent CLIs, media tools):
//! the child and every descendant it starts end together on normal exit,
//! timeout and cancellation. Unix uses a process group; Windows a Job Object.
//!
//! Protocol: [`ProcessTreeControl::prepare`] the command, spawn it, then
//! [`ProcessTreeControl::attach`] the child at once. If `attach` fails the
//! caller kills and reaps the child itself. Drive the wait with [`wait_tree`],
//! which always contains descendants before reaping the leader.

use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// Creates contained process trees (native factory per OS, fake in tests).
pub trait ProcessTreeControl: Send + Sync {
    /// Configures `command` so its child starts as the root of a new tree.
    fn prepare(&self, command: &mut Command) -> Result<()>;
    /// Binds the tree of a child spawned from a prepared command. Call right
    /// after spawning; on Windows this also lets the suspended child run.
    fn attach(&self, child: &Child) -> Result<Box<dyn ProcessTreeHandle>>;
}

/// One contained tree.
pub trait ProcessTreeHandle: Send {
    /// Whether the leader has exited, without reaping it: while the leader
    /// is unreaped its ID (and the Unix group ID) cannot be reused.
    fn leader_exited(&mut self, child: &mut Child) -> Result<bool>;
    /// Terminates every process still in the tree, leader included.
    /// Idempotent; never reaps the leader.
    fn kill_tree(&mut self, child: &mut Child) -> Result<()>;
}

/// How [`wait_tree`] ended. Descendants are gone and the leader reaped in every case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeExit {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
}

/// Longest pause between exit checks.
const MAX_POLL: Duration = Duration::from_millis(50);

/// Waits for `child` until it exits, `deadline` passes or `cancel` is set;
/// then kills what remains of its tree (descendants of a leader that exited
/// normally included) and reaps the leader.
pub fn wait_tree(
    handle: &mut dyn ProcessTreeHandle,
    child: &mut Child,
    deadline: Option<Instant>,
    cancel: Option<&AtomicBool>,
) -> Result<TreeExit> {
    let mut pause = Duration::from_millis(1);
    let outcome = loop {
        match handle.leader_exited(child) {
            Ok(true) => break None,
            Ok(false) => {}
            Err(error) => {
                contain(handle, child)?;
                return Err(error);
            }
        }
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            break Some(TreeExit::Cancelled);
        }
        let now = Instant::now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            break Some(TreeExit::TimedOut);
        }
        let sleep = deadline.map_or(pause, |deadline| pause.min(deadline - now));
        std::thread::sleep(sleep);
        pause = (pause * 2).min(MAX_POLL);
    };
    let status = contain(handle, child)?;
    Ok(outcome.unwrap_or(TreeExit::Exited(status)))
}

/// Kills the remaining tree, then reaps the leader.
fn contain(handle: &mut dyn ProcessTreeHandle, child: &mut Child) -> Result<ExitStatus> {
    let killed = handle.kill_tree(child);
    let status = child
        .wait()
        .map_err(|error| Error::io("reaping a contained child process", error))?;
    killed.map(|()| status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::FakeProcessTreeControl;

    fn quick_child() -> (Command, Command) {
        // Portable synthetic children: the current test binary listing its tests.
        let exe = std::env::current_exe().unwrap();
        let mut quick = Command::new(&exe);
        quick.args(["--list", "--exact", "no-such-test"]);
        let mut slow = Command::new(exe);
        slow.args([
            "--exact",
            "process_tree::tests::sleeper_helper",
            "--ignored",
            "--nocapture",
        ]);
        (quick, slow)
    }

    #[test]
    #[ignore = "helper process for wait_tree tests"]
    fn sleeper_helper() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn normal_exit_still_contains_descendants_before_reaping() {
        let control = FakeProcessTreeControl::default();
        let (mut quick, _) = quick_child();
        control.prepare(&mut quick).unwrap();
        let mut child = quick.stdout(std::process::Stdio::null()).spawn().unwrap();
        let mut handle = control.attach(&child).unwrap();
        let exit = wait_tree(handle.as_mut(), &mut child, None, None).unwrap();
        assert!(matches!(exit, TreeExit::Exited(status) if status.success()));
        assert_eq!(control.kills(), 1, "descendants were not contained");
    }

    #[test]
    fn timeout_and_cancel_kill_the_tree_and_reap() {
        for cancelled in [false, true] {
            let control = FakeProcessTreeControl::default();
            let (_, mut slow) = quick_child();
            control.prepare(&mut slow).unwrap();
            let mut child = slow.stdout(std::process::Stdio::null()).spawn().unwrap();
            let mut handle = control.attach(&child).unwrap();
            let flag = AtomicBool::new(cancelled);
            let deadline = Instant::now() + Duration::from_millis(150);
            let started = Instant::now();
            let exit = wait_tree(handle.as_mut(), &mut child, Some(deadline), Some(&flag)).unwrap();
            assert_eq!(
                exit,
                if cancelled {
                    TreeExit::Cancelled
                } else {
                    TreeExit::TimedOut
                }
            );
            assert!(started.elapsed() < Duration::from_secs(10));
            assert_eq!(control.kills(), 1);
            // Reaped: nothing is left to wait for.
            assert!(child.try_wait().unwrap().is_some());
        }
    }
}
