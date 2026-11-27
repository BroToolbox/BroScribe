//! Process-group containment of helper children: the child leads a new
//! group, its descendants inherit it, and the whole group is killed before
//! the leader is reaped (an unreaped leader keeps the group ID from being
//! reused by an unrelated process).

use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Makes the command's child lead a new process group.
pub fn prepare_group(command: &mut Command) {
    command.process_group(0);
}

/// Whether the group leader `pid` (a child of this process) has exited,
/// without reaping it. A leader already reaped elsewhere counts as exited.
pub fn leader_exited(pid: u32) -> io::Result<bool> {
    let id = libc::id_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid child pid"))?;
    loop {
        // SAFETY: zeroed siginfo_t is a valid out-parameter for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: valid out-pointer; WNOWAIT leaves the child unreaped.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            // SAFETY: waitid filled (or left zeroed) the siginfo for a child event.
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => return Ok(true),
            _ => return Err(error),
        }
    }
}

/// Kills the processes in the group led by `pid`, without reaping the leader.
pub fn kill_group(pid: u32) -> io::Result<()> {
    let group = libc::pid_t::try_from(pid)
        .ok()
        .filter(|group| *group > 1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process group"))?;

    #[cfg(target_os = "macos")]
    return drain_macos_group(pid, group);
    #[cfg(not(target_os = "macos"))]
    send_group_kill(group).map(|_| ())
}

// True when a signal was delivered, false when the group is gone.
fn send_group_kill(group: libc::pid_t) -> io::Result<bool> {
    // SAFETY: killpg has no memory preconditions; the group is our child's.
    if unsafe { libc::killpg(group, libc::SIGKILL) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(error)
}

#[cfg(target_os = "macos")]
fn drain_macos_group(pid: u32, group: libc::pid_t) -> io::Result<()> {
    drain_macos_group_with(|| send_group_kill(group), || leader_exited(pid))
}

#[cfg(target_os = "macos")]
fn drain_macos_group_with(
    mut kill: impl FnMut() -> io::Result<bool>,
    mut exited: impl FnMut() -> io::Result<bool>,
) -> io::Result<()> {
    use std::time::{Duration, Instant};

    // Darwin snapshots the group before delivering signals. A fork between
    // the snapshot and delivery can leave a new member alive. Repeat while
    // there are targets, with the unreaped leader still reserving the group ID.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut delivered = false;
    let mut denied_since = None;
    loop {
        match kill() {
            Ok(false) => return Ok(()),
            Ok(true) => delivered = true,
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                // Darwin can stop targeting an exiting group before waitid
                // reports the zombie leader, even on the first kill attempt.
                if exited()? {
                    return Ok(());
                }
                // After an accepted kill, a large leader may take longer
                // to exit. Only pre-delivery denials use the shorter window.
                if !delivered
                    && denied_since.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_millis(75)
                {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "process group did not stop after SIGKILL",
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[cfg(target_os = "macos")]
    #[test]
    fn group_kill_retries_permission_denied_before_any_delivery() {
        for leader_exits in [false, true] {
            let mut attempts = 0;
            let mut probes = 0;
            drain_macos_group_with(
                || {
                    attempts += 1;
                    if attempts <= 2 {
                        Err(io::Error::from_raw_os_error(libc::EPERM))
                    } else {
                        assert!(!leader_exits);
                        Ok(false)
                    }
                },
                || {
                    probes += 1;
                    Ok(leader_exits && probes == 2)
                },
            )
            .unwrap();
            assert_eq!(attempts, if leader_exits { 2 } else { 3 });
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn group_kill_preserves_persistent_permission_denied() {
        let mut attempts = 0;
        let error = drain_macos_group_with(
            || {
                attempts += 1;
                Err(io::Error::from_raw_os_error(libc::EPERM))
            },
            || Ok(false),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        assert!(attempts > 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn group_kill_waits_past_the_pre_delivery_window_after_delivery() {
        let mut attempts = 0;
        let mut probes = 0;
        drain_macos_group_with(
            || {
                attempts += 1;
                if attempts == 1 {
                    Ok(true)
                } else {
                    Err(io::Error::from_raw_os_error(libc::EPERM))
                }
            },
            || {
                probes += 1;
                if probes == 2 {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(probes == 3)
            },
        )
        .unwrap();
        assert_eq!(attempts, 4);
    }

    #[test]
    fn descendants_die_with_the_group_after_the_leader_exits() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("grandchild.pid");
        // The leader starts a long-lived grandchild and exits at once.
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            &format!("sleep 30 & echo $! > '{}'", marker.display()),
        ]);
        prepare_group(&mut command);
        let mut child = command.spawn().unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while !leader_exited(child.id()).unwrap() {
            assert!(Instant::now() < until, "leader did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Still unreaped: the group ID is ours to kill.
        kill_group(child.id()).unwrap();
        // Repeated containment is harmless while the group ID is reserved.
        kill_group(child.id()).unwrap();
        assert!(child.wait().unwrap().success());
        let grandchild: libc::pid_t = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        // SAFETY: signal 0 only checks existence.
        while unsafe { libc::kill(grandchild, 0) } == 0 {
            assert!(Instant::now() < until, "grandchild survived its group");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn group_kill_drains_descendants_forking_during_signal_delivery() {
        use std::io::Read as _;
        use std::process::Stdio;

        let script = r#"
            for worker in 1 2 3 4; do
                (
                    for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16; do
                        sleep 2 &
                    done
                    wait
                ) &
            done
            printf R
            wait
        "#;

        for _ in 0..12 {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script]).stdout(Stdio::piped());
            prepare_group(&mut command);
            let mut child = command.spawn().unwrap();
            let mut ready = [0];
            child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
            let group = libc::pid_t::try_from(child.id()).unwrap();

            let stopped = kill_group(child.id());
            // SAFETY: signal 0 only probes the synthetic group. Its unreaped
            // leader still reserves the group ID.
            let live = unsafe { libc::killpg(group, 0) } == 0;
            if live {
                // Clean up even when testing a broken implementation.
                let _ = send_group_kill(group);
            }
            child.wait().unwrap();
            stopped.unwrap();
            assert_eq!(&ready, b"R");
            assert!(!live, "a live group member survived SIGKILL");
        }
    }

    #[test]
    fn leader_state_is_read_without_reaping() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 3"]);
        prepare_group(&mut command);
        let mut child = command.spawn().unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while !leader_exited(child.id()).unwrap() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        // Containing an already exited leader must preserve its real status,
        // including Darwin's EPERM for a zombie-only process group.
        kill_group(child.id()).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(3));
        assert!(kill_group(0).is_err());
        assert!(kill_group(1).is_err());
    }
}
