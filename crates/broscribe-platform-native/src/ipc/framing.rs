//! Newline-delimited lines with a length limit ([`broscribe_ipc::MAX_LINE_BYTES`])
//! for the async server and the deadline-bounded blocking client. A line
//! longer than the limit is `InvalidData` and the connection should close.

use std::io::{self, ErrorKind};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

use super::CancelToken;

/// Reads one line without its newline; `None` at end of stream. A last
/// line without a newline is returned as is.
pub async fn read_line<R>(r: &mut R, max: usize) -> io::Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    let mut limited = AsyncReadExt::take(&mut *r, max as u64);
    let n = AsyncBufReadExt::read_until(&mut limited, b'\n', &mut buf).await?;
    finish_line(buf, n, max)
}

pub(crate) fn finish_line(mut buf: Vec<u8>, n: usize, max: usize) -> io::Result<Option<String>> {
    if n == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() >= max {
        return Err(too_long(max));
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))
}

fn too_long(max: usize) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        format!("line longer than {max} bytes"),
    )
}

/// A blocking byte stream whose reads and writes honour a deadline.
pub(crate) trait Wire {
    /// Reads at least one byte (0 at end of stream) before `deadline`
    /// (`TimedOut` after it); `None` waits as long as needed.
    fn read_some(&mut self, buf: &mut [u8], deadline: Option<Instant>) -> io::Result<usize>;
    /// Writes at least one byte of the nonempty `bytes` before `deadline`
    /// and returns how many; `TimedOut` only when none was written.
    fn write_some_by(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize>;
}

/// Longest uninterrupted wait (connecting, writing, a response, a reconcile
/// pause) of a cancellable call, so cancellation is noticed.
pub(crate) const CANCEL_SLICE: Duration = Duration::from_millis(250);

/// The end of the next wait toward `deadline`: with `cancel`, at most
/// [`CANCEL_SLICE`] away; `None` once cancelled.
pub(crate) fn next_slice(deadline: Instant, cancel: Option<&CancelToken>) -> Option<Instant> {
    match cancel {
        Some(cancel) if cancel.is_cancelled() => None,
        Some(_) => Some(deadline.min(Instant::now() + CANCEL_SLICE)),
        None => Some(deadline),
    }
}

/// A write that stopped before all its bytes were written.
#[derive(Debug)]
pub(crate) struct ShortWrite {
    /// Bytes the peer may have received (always a prefix, never repeated).
    pub(crate) written: usize,
    pub(crate) error: io::Error,
}

/// Writes all of `bytes` before `deadline`, resuming exactly where each
/// partial write stopped. With `cancel`, waits in slices and stops at
/// cancellation (`Interrupted`).
pub(crate) fn write_all<W: Wire>(
    wire: &mut W,
    bytes: &[u8],
    deadline: Instant,
    cancel: Option<&CancelToken>,
) -> Result<(), ShortWrite> {
    let mut written = 0;
    while written < bytes.len() {
        let Some(slice) = next_slice(deadline, cancel) else {
            return Err(ShortWrite {
                written,
                error: io::Error::new(ErrorKind::Interrupted, "cancelled while sending"),
            });
        };
        match wire.write_some_by(&bytes[written..], slice) {
            Ok(0) => {
                return Err(ShortWrite {
                    written,
                    error: ErrorKind::WriteZero.into(),
                });
            }
            Ok(n) => written += n,
            Err(e) if e.kind() == ErrorKind::TimedOut && slice < deadline => {}
            Err(error) => return Err(ShortWrite { written, error }),
        }
    }
    Ok(())
}

/// Buffered line reader over a [`Wire`]. A read that times out keeps the
/// partial line, so the caller may wait again without losing data.
pub(crate) struct Lines<W> {
    wire: W,
    buf: Vec<u8>,
    /// Start of unread data in `buf`.
    start: usize,
    /// Bytes from `start` already searched for a newline.
    scanned: usize,
}

const CHUNK: usize = 8 * 1024;

impl<W: Wire> Lines<W> {
    pub(crate) fn new(wire: W) -> Self {
        Self {
            wire,
            buf: Vec::new(),
            start: 0,
            scanned: 0,
        }
    }

    pub(crate) fn wire_mut(&mut self) -> &mut W {
        &mut self.wire
    }

    /// The next line (without newline) before `deadline`; `None` at end of
    /// stream. Lines of `max` bytes or more are `InvalidData`.
    pub(crate) fn read_line(
        &mut self,
        max: usize,
        deadline: Option<Instant>,
    ) -> io::Result<Option<String>> {
        loop {
            let pending = &self.buf[self.start..];
            if let Some(offset) = pending[self.scanned..].iter().position(|b| *b == b'\n') {
                let end = self.start + self.scanned + offset;
                let line = self.buf[self.start..end].to_vec();
                self.start = end + 1;
                self.scanned = 0;
                // Like `read_line`: the line and its newline fit in `max` bytes.
                if line.len() >= max {
                    return Err(too_long(max));
                }
                return String::from_utf8(line)
                    .map(Some)
                    .map_err(|e| io::Error::new(ErrorKind::InvalidData, e));
            }
            self.scanned = pending.len();
            if pending.len() >= max {
                return Err(too_long(max));
            }
            if self.start > 0 && self.start >= self.buf.len() / 2 {
                self.buf.drain(..self.start);
                self.start = 0;
            }
            let filled = self.buf.len();
            self.buf.resize(filled + CHUNK, 0);
            let read = self.wire.read_some(&mut self.buf[filled..], deadline);
            let n = match read {
                Ok(n) => n,
                Err(e) => {
                    self.buf.truncate(filled);
                    return Err(e);
                }
            };
            self.buf.truncate(filled + n);
            if n == 0 {
                let rest = self.buf.split_off(self.start);
                self.buf.clear();
                self.start = 0;
                self.scanned = 0;
                let len = rest.len();
                return finish_line(rest, len, max);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;

    /// Serves scripted chunks; `Err(TimedOut)` entries model a slow peer.
    struct Script(VecDeque<io::Result<Vec<u8>>>);
    impl Wire for Script {
        fn read_some(&mut self, buf: &mut [u8], _: Option<Instant>) -> io::Result<usize> {
            match self.0.pop_front() {
                None => Ok(0),
                Some(Err(e)) => Err(e),
                Some(Ok(chunk)) => {
                    buf[..chunk.len()].copy_from_slice(&chunk);
                    Ok(chunk.len())
                }
            }
        }
        fn write_some_by(&mut self, bytes: &[u8], _: Instant) -> io::Result<usize> {
            Ok(bytes.len())
        }
    }

    /// Accepts scripted amounts per write; `None` (and the end of the
    /// script) is a full buffer (`TimedOut`). Sets `cancel_after` once its
    /// first bytes are accepted.
    struct Sink {
        steps: VecDeque<Option<usize>>,
        received: Vec<u8>,
        cancel_after: Option<CancelToken>,
    }

    impl Sink {
        fn new<const N: usize>(
            steps: [Option<usize>; N],
            cancel_after: Option<CancelToken>,
        ) -> Self {
            Self {
                steps: VecDeque::from(steps),
                received: Vec::new(),
                cancel_after,
            }
        }
    }

    impl Wire for Sink {
        fn read_some(&mut self, _: &mut [u8], _: Option<Instant>) -> io::Result<usize> {
            Ok(0)
        }
        fn write_some_by(&mut self, bytes: &[u8], _: Instant) -> io::Result<usize> {
            let Some(n) = self.steps.pop_front().flatten() else {
                return Err(ErrorKind::TimedOut.into());
            };
            let n = n.min(bytes.len());
            self.received.extend_from_slice(&bytes[..n]);
            if let Some(cancel) = self.cancel_after.take() {
                cancel.cancel();
            }
            Ok(n)
        }
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    #[test]
    fn writes_resume_at_the_exact_offset_after_stalls() {
        let bytes = b"{\"id\":\"r-1\",\"method\":\"status\"}\n";
        let mut sink = Sink::new([Some(3), None, Some(4), None, Some(usize::MAX)], None);
        write_all(&mut sink, bytes, far(), Some(&CancelToken::new())).unwrap();
        assert_eq!(sink.received, bytes);

        // Without cancellation a stall is the deadline: the offset survives.
        let mut sink = Sink::new([Some(2), None], None);
        let err = write_all(&mut sink, bytes, far(), None).unwrap_err();
        assert_eq!((err.written, err.error.kind()), (2, ErrorKind::TimedOut));
        assert_eq!(sink.received, bytes[..2]);
    }

    #[test]
    fn cancellation_stops_a_write_at_its_exact_offset() {
        let cancelled = CancelToken::new();
        cancelled.cancel();
        let mut sink = Sink::new([Some(usize::MAX)], None);
        let err = write_all(&mut sink, b"line\n", far(), Some(&cancelled)).unwrap_err();
        assert_eq!((err.written, err.error.kind()), (0, ErrorKind::Interrupted));
        assert!(sink.received.is_empty());

        // The peer takes five bytes, then stops reading; cancellation
        // arrives after that first partial write.
        let cancel = CancelToken::new();
        let mut sink = Sink::new([Some(5)], Some(cancel.clone()));
        let err = write_all(&mut sink, b"0123456789\n", far(), Some(&cancel)).unwrap_err();
        assert_eq!((err.written, err.error.kind()), (5, ErrorKind::Interrupted));
        assert_eq!(sink.received, b"01234");
    }

    #[test]
    fn lines_split_across_reads_survive_timeouts() {
        let mut lines = Lines::new(Script(VecDeque::from([
            Ok(b"one\ntw".to_vec()),
            Err(io::Error::from(ErrorKind::TimedOut)),
            Ok(b"o\nlast".to_vec()),
        ])));
        let deadline = Some(Instant::now() + Duration::from_secs(1));
        assert_eq!(
            lines.read_line(64, deadline).unwrap().as_deref(),
            Some("one")
        );
        assert_eq!(
            lines.read_line(64, deadline).unwrap_err().kind(),
            ErrorKind::TimedOut
        );
        assert_eq!(
            lines.read_line(64, deadline).unwrap().as_deref(),
            Some("two")
        );
        assert_eq!(lines.read_line(64, None).unwrap().as_deref(), Some("last"));
        assert_eq!(lines.read_line(64, None).unwrap(), None);
    }

    #[test]
    fn overlong_lines_are_refused() {
        let mut lines = Lines::new(Script(VecDeque::from([Ok(b"0123456789abcdef\n".to_vec())])));
        assert_eq!(
            lines.read_line(8, None).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        let mut exact = Lines::new(Script(VecDeque::from([Ok(b"1234567\n".to_vec())])));
        assert_eq!(
            exact.read_line(8, None).unwrap().as_deref(),
            Some("1234567")
        );
    }

    #[tokio::test]
    async fn async_lines_and_limit() {
        let mut reader = tokio::io::BufReader::new(&b"{\"v\":1}\nsecond\nlast"[..]);
        assert_eq!(
            read_line(&mut reader, 64).await.unwrap().as_deref(),
            Some("{\"v\":1}")
        );
        assert_eq!(
            read_line(&mut reader, 64).await.unwrap().as_deref(),
            Some("second")
        );
        assert_eq!(
            read_line(&mut reader, 64).await.unwrap().as_deref(),
            Some("last")
        );
        assert_eq!(read_line(&mut reader, 64).await.unwrap(), None);
        let mut long = tokio::io::BufReader::new(&b"0123456789abcdef\n"[..]);
        assert_eq!(
            read_line(&mut long, 8).await.unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }
}
