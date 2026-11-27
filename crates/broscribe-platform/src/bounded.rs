//! Short helper-tool runs (probes) with a total deadline, an output cap and
//! cancellation; the child is always killed and reaped before returning.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// Standard output of a finished bounded run.
#[derive(Debug)]
pub struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

/// Longest pause between exit checks.
const MAX_POLL: Duration = Duration::from_millis(25);

/// Bound each login-manager invocation, including when the caller holds the
/// account-wide registration lock.
pub const LOGIN_MANAGER_TIMEOUT: Duration = Duration::from_secs(10);

/// Captures both streams separately, up to 1 MiB each, within the deadline.
pub fn login_manager_output(command: &mut Command, timeout: Duration) -> Result<Output> {
    capture(command, timeout, 1 << 20, None, true)
}

/// Runs `command` (stdin and stderr discarded) until it exits, `timeout`
/// passes or `cancel` is set. Output past `max_stdout` bytes is an error.
/// Timeouts are `TimedOut` and cancellation `Interrupted` I/O errors.
pub fn output_within(
    command: &mut Command,
    timeout: Duration,
    max_stdout: usize,
    cancel: Option<&AtomicBool>,
) -> Result<BoundedOutput> {
    let Output { status, stdout, .. } = capture(command, timeout, max_stdout, cancel, false)?;
    Ok(BoundedOutput { status, stdout })
}

fn capture(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
    cancel: Option<&AtomicBool>,
    capture_stderr: bool,
) -> Result<Output> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if capture_stderr {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .spawn()
        .map_err(|e| Error::io("starting a helper tool", e))?;
    let stdout = read_pipe(child.stdout.take().expect("piped stdout"), limit);
    let stderr = child.stderr.take().map(|pipe| read_pipe(pipe, limit));
    let mut pause = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                end(&mut child);
                return Err(Error::io("waiting for a helper tool", e));
            }
        }
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            end(&mut child);
            return Err(Error::io(
                "helper tool",
                std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"),
            ));
        }
        let now = Instant::now();
        if now >= deadline {
            end(&mut child);
            return Err(Error::io(
                "helper tool",
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("no answer within {} ms", timeout.as_millis()),
                ),
            ));
        }
        std::thread::sleep(pause.min(deadline - now));
        pause = (pause * 2).min(MAX_POLL);
    };
    // A descendant holding the pipe must not hold the caller past its deadline.
    let output_deadline = if capture_stderr {
        deadline
    } else {
        deadline.max(Instant::now() + Duration::from_millis(100))
    };
    Ok(Output {
        status,
        stdout: collect_pipe(stdout, output_deadline, limit)?,
        stderr: stderr
            .map(|pipe| collect_pipe(pipe, output_deadline, limit))
            .transpose()?
            .unwrap_or_default(),
    })
}

fn read_pipe(
    pipe: impl Read + Send + 'static,
    limit: usize,
) -> mpsc::Receiver<std::io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let result = pipe
            .take((limit as u64).saturating_add(1))
            .read_to_end(&mut buffer)
            .map(|_| buffer);
        let _ = tx.send(result);
    });
    rx
}

fn collect_pipe(
    rx: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    deadline: Instant,
    limit: usize,
) -> Result<Vec<u8>> {
    let bytes = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => return Err(Error::io("reading a helper tool's output", e)),
        Err(_) => {
            return Err(Error::io(
                "helper tool",
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "output stayed open after exit",
                ),
            ));
        }
    };
    if bytes.len() > limit {
        return Err(Error::Decode(format!(
            "helper tool wrote more than {limit} bytes"
        )));
    }
    Ok(bytes)
}

/// Kills and reaps `child`, ignoring a child that already exited.
fn end(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Stops `child` without blocking the caller: killed now, reaped here when
/// it is already gone, otherwise by a short-lived background thread.
pub fn kill_and_reap_in_background(mut child: Child) {
    let _ = child.kill();
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("child-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_binary() -> Command {
        Command::new(std::env::current_exe().unwrap())
    }

    #[test]
    #[ignore = "helper process for bounded-run tests"]
    fn bounded_sleeper_helper() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn quick_tools_return_their_output() {
        let mut command = test_binary();
        command.args([
            "--list",
            "--exact",
            "bounded::tests::quick_tools_return_their_output",
        ]);
        let output = output_within(&mut command, Duration::from_secs(30), 1 << 20, None).unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("quick_tools_return_their_output")
        );
    }

    #[test]
    fn oversized_output_is_refused() {
        let mut command = test_binary();
        command.arg("--list");
        let error = output_within(&mut command, Duration::from_secs(30), 4, None).unwrap_err();
        assert_eq!(error.code(), "decode");
    }

    #[test]
    fn hung_tools_time_out_or_cancel_and_are_reaped() {
        for cancelled in [false, true] {
            let mut command = test_binary();
            command.args([
                "--exact",
                "bounded::tests::bounded_sleeper_helper",
                "--ignored",
            ]);
            let flag = AtomicBool::new(cancelled);
            let started = Instant::now();
            let error = output_within(
                &mut command,
                Duration::from_millis(200),
                1 << 20,
                Some(&flag),
            )
            .unwrap_err();
            assert_eq!(error.code(), "io");
            let text = error.to_string();
            assert!(
                text.contains(if cancelled { "cancelled" } else { "no answer" }),
                "{text}"
            );
            assert!(started.elapsed() < Duration::from_secs(10));
        }
    }
}
