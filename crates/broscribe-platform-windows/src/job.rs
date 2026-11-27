//! Process-tree containment through Job Objects: the child starts suspended,
//! joins a kill-on-close job before running any code, and every process it
//! starts inherits the job.
use crate::native::OwnedHandle;
use broscribe_platform::{Error, ProcessTreeControl, ProcessTreeHandle, Result};
use std::{
    os::windows::{io::AsRawHandle, process::CommandExt},
    process::{Child, Command},
};
use windows::{
    Win32::{
        Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject,
            },
            Threading::{
                CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread,
                THREAD_SUSPEND_RESUME, WaitForSingleObject,
            },
        },
    },
    core::PCWSTR,
};

/// Exit code of processes ended by [`ProcessTreeHandle::kill_tree`].
const KILLED: u32 = 1;

pub struct WinProcessTreeControl;

impl ProcessTreeControl for WinProcessTreeControl {
    /// Sets the creation flags to `CREATE_SUSPENDED | CREATE_NO_WINDOW`;
    /// callers must not change creation flags afterwards.
    fn prepare(&self, command: &mut Command) -> Result<()> {
        command.creation_flags(CREATE_SUSPENDED.0 | CREATE_NO_WINDOW.0);
        Ok(())
    }

    fn attach(&self, child: &Child) -> Result<Box<dyn ProcessTreeHandle>> {
        let job = create_job()?;
        let process = HANDLE(child.as_raw_handle());
        unsafe { AssignProcessToJobObject(job.0, process) }
            .map_err(|e| crate::error("assigning a child to its job", e))?;
        if let Err(error) = resume(child.id()) {
            let _ = unsafe { TerminateJobObject(job.0, KILLED) };
            return Err(error);
        }
        Ok(Box::new(JobTree { job }))
    }
}

fn create_job() -> Result<OwnedHandle> {
    let job = OwnedHandle(
        unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|e| crate::error("creating a process job", e))?,
    );
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .map_err(|e| crate::error("configuring a process job", e))?;
    Ok(job)
}

/// Resumes the suspended primary thread of process `pid`.
fn resume(pid: u32) -> Result<()> {
    let snapshot = OwnedHandle(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }
            .map_err(|e| crate::error("listing child threads", e))?,
    );
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut resumed = 0;
    let mut next = unsafe { Thread32First(snapshot.0, &mut entry) };
    while next.is_ok() {
        if entry.th32OwnerProcessID == pid {
            let thread = OwnedHandle(
                unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) }
                    .map_err(|e| crate::error("opening a child thread", e))?,
            );
            if unsafe { ResumeThread(thread.0) } == u32::MAX {
                return Err(crate::error(
                    "resuming a child thread",
                    std::io::Error::last_os_error(),
                ));
            }
            resumed += 1;
        }
        next = unsafe { Thread32Next(snapshot.0, &mut entry) };
    }
    if resumed == 0 {
        return Err(Error::Internal(
            "the suspended child process has no thread to resume".into(),
        ));
    }
    Ok(())
}

/// Closing the job (drop) kills whatever is left of the tree.
struct JobTree {
    job: OwnedHandle,
}

impl ProcessTreeHandle for JobTree {
    fn leader_exited(&mut self, child: &mut Child) -> Result<bool> {
        match unsafe { WaitForSingleObject(HANDLE(child.as_raw_handle()), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(crate::error(
                "checking a child process",
                std::io::Error::last_os_error(),
            )),
        }
    }

    fn kill_tree(&mut self, _child: &mut Child) -> Result<()> {
        unsafe { TerminateJobObject(self.job.0, KILLED) }
            .map_err(|e| crate::error("terminating a process job", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::{TreeExit, wait_tree};
    use std::time::{Duration, Instant};

    #[test]
    fn the_job_kills_descendants_on_timeout_and_after_normal_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let system =
            std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
        let cmd = system.join("cmd.exe");
        let ping = system.join("ping.exe");
        for detached in [false, true] {
            let marker = tmp.path().join(format!("grandchild-{detached}.txt"));
            // The leader starts a grandchild that writes its marker after
            // ~5 s; on the first pass the leader then waits (timeout), on the
            // second it exits at once.
            // Keep shell syntax in batch files: Command::arg uses C-runtime
            // escaping, which is not cmd.exe /c's quoting convention.
            std::fs::write(
                tmp.path().join("grandchild.cmd"),
                format!(
                    "@echo off\r\n\"{}\" -n 6 127.0.0.1 >nul\r\necho late>grandchild-{detached}.txt\r\n",
                    ping.display(),
                ),
            )
            .unwrap();
            std::fs::write(
                tmp.path().join("leader.cmd"),
                format!(
                    "@echo off\r\nstart \"\" /b \"{}\" /d /c grandchild.cmd\r\n{}",
                    cmd.display(),
                    if detached {
                        "exit /b %errorlevel%\r\n".to_owned()
                    } else {
                        format!("\"{}\" -n 30 127.0.0.1 >nul\r\n", ping.display())
                    },
                ),
            )
            .unwrap();
            let mut command = Command::new(&cmd);
            command
                .current_dir(tmp.path())
                .args(["/d", "/c", "leader.cmd"]);
            let control = WinProcessTreeControl;
            control.prepare(&mut command).unwrap();
            let mut child = command.spawn().unwrap();
            let mut tree = control.attach(&child).unwrap();
            let exit = wait_tree(
                tree.as_mut(),
                &mut child,
                Some(Instant::now() + Duration::from_secs(1)),
                None,
            )
            .unwrap();
            if detached {
                assert!(
                    matches!(exit, TreeExit::Exited(status) if status.success()),
                    "{exit:?}"
                );
            } else {
                assert_eq!(exit, TreeExit::TimedOut);
            }
            drop(tree);
            std::thread::sleep(Duration::from_secs(7));
            assert!(!marker.exists(), "a descendant outlived its job");
        }
    }
}
