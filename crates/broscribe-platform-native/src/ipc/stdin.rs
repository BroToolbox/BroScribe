//! A child's stdin written without ever blocking the writer: a stalled
//! reader yields `WouldBlock`, and the writer waits a bounded time for room.

use std::io::{self, Write};
use std::process::ChildStdin;
use std::time::Duration;

use super::platform;

pub struct NonBlockingStdin(ChildStdin);

impl NonBlockingStdin {
    pub fn new(stdin: ChildStdin) -> io::Result<Self> {
        platform::set_stdin_nonblocking(&stdin)?;
        Ok(Self(stdin))
    }

    /// Waits at most `timeout` for room in the pipe. `true` means a write may
    /// now succeed (on Windows, after a short pause: retry the write).
    pub fn wait_writable(&self, timeout: Duration) -> io::Result<bool> {
        platform::wait_stdin_writable(&self.0, timeout)
    }

    pub fn into_inner(self) -> ChildStdin {
        self.0
    }
}

impl Write for NonBlockingStdin {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        platform::write_stdin(&mut self.0, bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    #[test]
    #[ignore = "helper process that never reads its stdin"]
    fn stdin_staller_helper() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn a_stalled_reader_never_blocks_the_writer() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ipc::stdin::tests::stdin_staller_helper",
                "--ignored",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = NonBlockingStdin::new(child.stdin.take().unwrap()).unwrap();
        let chunk = vec![b'x'; 64 * 1024];
        let started = Instant::now();
        let mut blocked = false;
        // Pipe buffers are at most a few MiB: the writer must hit WouldBlock.
        for _ in 0..1024 {
            match stdin.write(&chunk) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    blocked = true;
                    break;
                }
                Err(e) => panic!("{e}"),
            }
        }
        assert!(blocked, "the pipe never filled");
        let waited = Instant::now();
        let _ = stdin.wait_writable(Duration::from_millis(100)).unwrap();
        assert!(waited.elapsed() < Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(20));
        let _ = child.kill();
        let _ = child.wait();
    }
}
