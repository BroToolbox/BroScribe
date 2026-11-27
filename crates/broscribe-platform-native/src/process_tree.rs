//! Unix process-tree containment through process groups (Windows uses the
//! platform crate's Job Object control directly).

use std::process::{Child, Command};

use broscribe_platform::{Error, ProcessTreeControl, ProcessTreeHandle, Result};
use broscribe_platform_unix::process_group;

pub(crate) struct GroupTreeControl;

impl ProcessTreeControl for GroupTreeControl {
    fn prepare(&self, command: &mut Command) -> Result<()> {
        process_group::prepare_group(command);
        Ok(())
    }

    fn attach(&self, child: &Child) -> Result<Box<dyn ProcessTreeHandle>> {
        Ok(Box::new(GroupTree { leader: child.id() }))
    }
}

struct GroupTree {
    leader: u32,
}

impl ProcessTreeHandle for GroupTree {
    fn leader_exited(&mut self, _child: &mut Child) -> Result<bool> {
        process_group::leader_exited(self.leader)
            .map_err(|e| Error::io("checking a contained child process", e))
    }

    fn kill_tree(&mut self, _child: &mut Child) -> Result<()> {
        process_group::kill_group(self.leader)
            .map_err(|e| Error::io("stopping a contained process group", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::{TreeExit, wait_tree};
    use std::time::{Duration, Instant};

    #[test]
    fn descendants_end_with_the_tree_on_exit_timeout_and_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        for (case, leader_waits, cancel) in [
            ("exit", false, false),
            ("timeout", true, false),
            ("cancel", true, true),
        ] {
            let marker = tmp.path().join(case);
            // The grandchild writes its marker after ~2 s unless killed.
            let script = format!(
                "(sleep 2; echo late > '{}') & {}",
                marker.display(),
                if leader_waits { "sleep 30" } else { "exit 0" }
            );
            let mut command = Command::new("/bin/sh");
            command.args(["-c", &script]);
            let control = GroupTreeControl;
            control.prepare(&mut command).unwrap();
            let mut child = command.spawn().unwrap();
            let mut tree = control.attach(&child).unwrap();
            let flag = std::sync::atomic::AtomicBool::new(cancel);
            let exit = wait_tree(
                tree.as_mut(),
                &mut child,
                Some(Instant::now() + Duration::from_millis(500)),
                Some(&flag),
            )
            .unwrap();
            match case {
                "exit" => assert!(
                    matches!(exit, TreeExit::Exited(s) if s.success()),
                    "{exit:?}"
                ),
                "timeout" => assert_eq!(exit, TreeExit::TimedOut),
                _ => assert_eq!(exit, TreeExit::Cancelled),
            }
            std::thread::sleep(Duration::from_secs(3));
            assert!(!marker.exists(), "{case}: a descendant outlived its tree");
        }
    }
}
