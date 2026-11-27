//! Audio playback for the app (meeting view, voice samples, review queue).
//!
//! Starting playback has two halves so that file checks and track probing
//! never run on the UI loop:
//!
//! 1. [`AudioPlayer::prepare`] (UI loop, no file I/O) issues a `Send`
//!    [`PlaybackPreparation`] for one file;
//! 2. [`PlaybackPreparation::run`] (a background thread) checks and probes
//!    the file within [`PREPARE_TIMEOUT`], cancellable through its
//!    [`PlaybackCancel`];
//! 3. [`AudioPlayer::play_prepared`] (UI loop) starts the result, refusing a
//!    preparation another player issued or a newer request superseded.
//!
//! [`AudioPlayer::play`] runs the three steps on the caller's thread, for
//! callers off the UI loop (audio checks).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// What the player is doing, as seen by [`AudioPlayer::poll`].
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerState {
    /// Nothing loaded, or stopped.
    Stopped,
    /// Playback was started but no audio plays yet (the backend is still
    /// opening the file). Backends bound this wait and report `Failed`
    /// when it lasts too long.
    Loading {
        position_s: f64,
    },
    Playing {
        position_s: f64,
    },
    Paused {
        position_s: f64,
    },
    /// Reached `end_s` or the end of the file. Reported by the poll that
    /// noticed it; the player is `Stopped` afterwards.
    Finished {
        position_s: f64,
    },
    /// The file could not be played, or stopping it failed (reported once;
    /// `Stopped` afterwards).
    Failed {
        message: String,
    },
}

/// Longest time [`PlaybackPreparation::run`] may take.
pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(10);

/// [`Error::code`] of a preparation that was cancelled: through its
/// [`PlaybackCancel`], or by its player (a newer request, `stop`, `play` or
/// the player being dropped).
pub const PLAYBACK_CANCELLED: &str = "playback_cancelled";
/// [`Error::code`] of a preparation [`AudioPlayer::play_prepared`] refused
/// because a newer request, `stop` or `play` superseded it.
pub const PLAYBACK_SUPERSEDED: &str = "playback_superseded";
/// [`Error::code`] of a file this backend can open but not play whole (e.g.
/// AVFoundation would shorten a longer track of a multi-track recording):
/// play the playable cache the CLI makes instead, as for a file
/// [`AudioPlayer::can_play_natively`] refuses.
pub const PLAYBACK_NEEDS_CACHE: &str = "playback_needs_cache";

/// Longest pause between cancellation and deadline checks in `run`.
const RUN_POLL: Duration = Duration::from_millis(20);

/// Plays one file at a time. Lives on the main thread (`!Send` allowed).
///
/// Positions are in seconds from the start of the file (recording-relative).
/// Nothing runs a timer: the app calls [`poll`](AudioPlayer::poll) (about
/// 4 Hz) only while something plays, and `poll` enforces `end_s`. No method
/// blocks on file I/O or child processes except [`play`](AudioPlayer::play).
pub trait AudioPlayer {
    /// Whether `path` can be played directly; other formats need a playable
    /// cache made by the CLI. Looks at the name only (no file I/O).
    fn can_play_natively(&self, path: &Path) -> bool;
    /// Starts a request to play `path` without file I/O: returns the
    /// preparation to [`run`](PlaybackPreparation::run) off the UI loop.
    /// Cancels and supersedes the previous pending request; whatever plays
    /// keeps playing. Errors when the backend can never play `path` (no
    /// player installed, unsupported format).
    fn prepare(&mut self, path: &Path) -> Result<PlaybackPreparation>;
    /// Plays a preparation this player issued for its current request, from
    /// `start_s`, stopping at `end_s` when given. Stops whatever was playing
    /// before. Refuses another player's preparation (`invalid_input`) and a
    /// superseded one ([`PLAYBACK_SUPERSEDED`]); nothing changes then.
    fn play_prepared(
        &mut self,
        prepared: PreparedPlayback,
        start_s: f64,
        end_s: Option<f64>,
    ) -> Result<()>;
    /// [`prepare`](Self::prepare), [`run`](PlaybackPreparation::run) and
    /// [`play_prepared`](Self::play_prepared) on the caller's thread, which
    /// it blocks for up to [`PREPARE_TIMEOUT`]: never call it on the UI loop.
    fn play(&mut self, path: &Path, start_s: f64, end_s: Option<f64>) -> Result<()> {
        check_range(start_s, end_s)?;
        let prepared = self.prepare(path)?.run()?;
        self.play_prepared(prepared, start_s, end_s)
    }
    /// Sets the session's rate for full playback. Bounded sample clips
    /// always play at normal speed.
    fn set_rate(&mut self, rate: f32) -> Result<()>;
    fn pause(&mut self);
    fn resume(&mut self) -> Result<()>;
    /// Stops playback and cancels a pending preparation without waiting for
    /// either to end.
    fn stop(&mut self);
    /// Current position, or `None` when nothing is loaded.
    fn position_s(&self) -> Option<f64>;
    /// Whether playback was started and not paused (also while `Loading`).
    fn is_playing(&self) -> bool;
    /// Updates and returns the state; stops at `end_s`.
    fn poll(&mut self) -> PlayerState;
}

pub fn check_rate(rate: f32) -> Result<()> {
    if (0.75..=2.0).contains(&rate) {
        Ok(())
    } else {
        Err(Error::InvalidInput(
            "playback rate must be between 0.75 and 2.0".into(),
        ))
    }
}

/// Refuses a range no backend can play: `start_s` must be finite and not
/// negative, `end_s` finite and after it.
pub fn check_range(start_s: f64, end_s: Option<f64>) -> Result<()> {
    if !start_s.is_finite()
        || start_s < 0.0
        || end_s.is_some_and(|end| !end.is_finite() || end <= start_s)
    {
        return Err(Error::InvalidInput("invalid playback time range".into()));
    }
    Ok(())
}

/// Probe-side check that `path` names an existing regular file (blocking
/// file I/O: call it from a [`PlaybackProbe`]).
pub fn check_audio_file(path: &Path) -> Result<()> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::NotFound(format!("audio file {}", path.display())));
        }
        Err(e) => return Err(Error::io_at(path, e)),
    };
    if !metadata.is_file() {
        return Err(Error::InvalidInput(format!(
            "{} is not an audio file",
            path.display()
        )));
    }
    Ok(())
}

/// Stops one preparation early; clones share the flag.
#[derive(Clone, Debug, Default)]
pub struct PlaybackCancel(Arc<AtomicBool>);

impl PlaybackCancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What a backend's probe learned about a file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbedPlayback {
    /// Audio streams to mix (never empty), in the backend's own numbering
    /// (ffplay: absolute stream indices).
    pub tracks: Vec<usize>,
    /// Length of the audio, when the file records it.
    pub duration_s: Option<f64>,
}

/// A backend's probe: runs on a background thread, should return early once
/// the flag is set.
pub type PlaybackProbe = Box<dyn FnOnce(&Path, &AtomicBool) -> Result<ProbedPlayback> + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ticket {
    issuer: u64,
    request: u64,
}

/// One request to play a file, issued by [`AudioPlayer::prepare`]. `Send`:
/// [`run`](Self::run) belongs on a background thread.
pub struct PlaybackPreparation {
    ticket: Ticket,
    path: PathBuf,
    cancel: PlaybackCancel,
    timeout: Duration,
    probe: PlaybackProbe,
}

impl std::fmt::Debug for PlaybackPreparation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackPreparation")
            .field("ticket", &self.ticket)
            .field("path", &self.path)
            .field("cancelled", &self.cancel.is_cancelled())
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl PlaybackPreparation {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Handle that cancels this preparation from any thread (e.g. when the
    /// requesting view is released).
    pub fn cancel_handle(&self) -> PlaybackCancel {
        self.cancel.clone()
    }

    /// Checks and probes the file. Blocks the calling thread until the probe
    /// answers, the preparation is cancelled ([`PLAYBACK_CANCELLED`]) or its
    /// timeout passes (a `TimedOut` I/O error, cancelling the probe). The
    /// probe runs on its own thread, which a probe stuck in file I/O may
    /// outlive; it never touches the player.
    pub fn run(self) -> Result<PreparedPlayback> {
        let Self {
            ticket,
            path,
            cancel,
            timeout,
            probe,
        } = self;
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let deadline = Instant::now() + timeout;
        let (tx, rx) = mpsc::sync_channel(1);
        let flag = cancel.0.clone();
        let probe_path = path.clone();
        std::thread::Builder::new()
            .name("playback-prepare".into())
            .spawn(move || {
                let _ = tx.send(probe(&probe_path, &flag));
            })
            .map_err(|e| Error::io("starting playback preparation", e))?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left.min(RUN_POLL)) {
                Ok(_) if cancel.is_cancelled() => return Err(cancelled()),
                Ok(result) => {
                    let probed = result?;
                    if probed.tracks.is_empty() {
                        return Err(Error::Decode("no audio tracks to play".into()));
                    }
                    return Ok(PreparedPlayback {
                        ticket,
                        path,
                        tracks: probed.tracks,
                        // Unusable metadata is no length at all.
                        duration_s: probed.duration_s.filter(|d| d.is_finite() && *d > 0.0),
                    });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.is_cancelled() {
                        return Err(cancelled());
                    }
                    if Instant::now() >= deadline {
                        cancel.cancel();
                        return Err(Error::io(
                            format!("preparing {} for playback", path.display()),
                            std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                format!("no answer within {} ms", timeout.as_millis()),
                            ),
                        ));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::Internal(
                        "playback preparation ended without an answer".into(),
                    ));
                }
            }
        }
    }
}

/// The [`PLAYBACK_NEEDS_CACHE`] error, for probes.
pub fn needs_playable_cache(reason: impl Into<String>) -> Error {
    Error::Service {
        code: PLAYBACK_NEEDS_CACHE,
        message: reason.into(),
    }
}

fn cancelled() -> Error {
    Error::Service {
        code: PLAYBACK_CANCELLED,
        message: "playback preparation was cancelled".into(),
    }
}

/// A checked and probed file, ready for [`AudioPlayer::play_prepared`] of
/// the player that issued it. Single use.
#[derive(Debug)]
pub struct PreparedPlayback {
    ticket: Ticket,
    path: PathBuf,
    tracks: Vec<usize>,
    duration_s: Option<f64>,
}

impl PreparedPlayback {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Probed audio streams (never empty).
    pub fn tracks(&self) -> &[usize] {
        &self.tracks
    }

    /// Length of the audio (finite, positive), when the file records it.
    pub fn duration_s(&self) -> Option<f64> {
        self.duration_s
    }
}

static NEXT_ISSUER: AtomicU64 = AtomicU64::new(1);

/// Request bookkeeping every backend embeds: issues preparations, accepts
/// only its own current one, and cancels the pending one when superseded or
/// dropped (with its player).
#[derive(Debug)]
pub struct PlaybackRequests {
    issuer: u64,
    current: u64,
    pending: Option<PlaybackCancel>,
}

impl Default for PlaybackRequests {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackRequests {
    pub fn new() -> Self {
        Self {
            issuer: NEXT_ISSUER.fetch_add(1, Ordering::Relaxed),
            current: 0,
            pending: None,
        }
    }

    /// Starts a request for `path` probed by `probe` within `timeout`,
    /// superseding the previous one.
    pub fn begin(
        &mut self,
        path: &Path,
        timeout: Duration,
        probe: PlaybackProbe,
    ) -> PlaybackPreparation {
        self.supersede();
        self.current = self.current.wrapping_add(1);
        let cancel = PlaybackCancel::default();
        self.pending = Some(cancel.clone());
        PlaybackPreparation {
            ticket: Ticket {
                issuer: self.issuer,
                request: self.current,
            },
            path: path.to_path_buf(),
            cancel,
            timeout,
            probe,
        }
    }

    /// Cancels the pending request; its preparation can no longer play.
    pub fn supersede(&mut self) {
        if let Some(cancel) = self.pending.take() {
            cancel.cancel();
        }
    }

    /// Takes `prepared` as the pending request's result: refuses another
    /// player's preparation and a superseded one.
    pub fn accept(&mut self, prepared: &PreparedPlayback) -> Result<()> {
        if prepared.ticket.issuer != self.issuer {
            return Err(Error::InvalidInput(
                "the playback was prepared by another player".into(),
            ));
        }
        if prepared.ticket.request != self.current || self.pending.is_none() {
            return Err(Error::Service {
                code: PLAYBACK_SUPERSEDED,
                message: "a newer playback request replaced this one".into(),
            });
        }
        self.pending = None;
        Ok(())
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

impl Drop for PlaybackRequests {
    fn drop(&mut self) {
        self.supersede();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_after(delay: Duration, probed: ProbedPlayback) -> PlaybackProbe {
        Box::new(move |_: &Path, cancel: &AtomicBool| {
            let until = Instant::now() + delay;
            while Instant::now() < until {
                if cancel.load(Ordering::SeqCst) {
                    return Err(Error::Internal("probe saw the cancellation".into()));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(probed)
        })
    }

    fn one_track() -> ProbedPlayback {
        ProbedPlayback {
            tracks: vec![0],
            duration_s: Some(3.0),
        }
    }

    #[test]
    fn delayed_probe_is_accepted_once_for_the_current_request() {
        let mut requests = PlaybackRequests::new();
        let prep = requests.begin(
            Path::new("/a.wav"),
            PREPARE_TIMEOUT,
            probe_after(Duration::from_millis(30), one_track()),
        );
        assert!(requests.is_pending());
        let prepared = std::thread::spawn(move || prep.run())
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(prepared.path(), Path::new("/a.wav"));
        assert_eq!(prepared.tracks(), [0]);
        assert_eq!(prepared.duration_s(), Some(3.0));
        requests.accept(&prepared).unwrap();
        assert!(!requests.is_pending());
        // Single use: the request is consumed.
        assert_eq!(
            requests.accept(&prepared).unwrap_err().code(),
            PLAYBACK_SUPERSEDED
        );
    }

    #[test]
    fn newer_request_cancels_and_supersedes_the_pending_one() {
        let mut requests = PlaybackRequests::new();
        let old = requests.begin(
            Path::new("/old.wav"),
            PREPARE_TIMEOUT,
            probe_after(Duration::from_millis(30), one_track()),
        );
        let old_cancel = old.cancel_handle();
        let new = requests.begin(
            Path::new("/new.wav"),
            PREPARE_TIMEOUT,
            probe_after(Duration::ZERO, one_track()),
        );
        assert!(old_cancel.is_cancelled());
        assert_eq!(old.run().unwrap_err().code(), PLAYBACK_CANCELLED);
        // A stale result that finished anyway is refused too.
        let stale = PreparedPlayback {
            ticket: Ticket {
                issuer: requests.issuer,
                request: requests.current - 1,
            },
            path: "/old.wav".into(),
            tracks: vec![0],
            duration_s: None,
        };
        assert_eq!(
            requests.accept(&stale).unwrap_err().code(),
            PLAYBACK_SUPERSEDED
        );
        let fresh = new.run().unwrap();
        requests.accept(&fresh).unwrap();
    }

    #[test]
    fn cancel_handle_stops_a_slow_probe_promptly() {
        let mut requests = PlaybackRequests::new();
        let prep = requests.begin(
            Path::new("/slow.wav"),
            PREPARE_TIMEOUT,
            probe_after(Duration::from_secs(30), one_track()),
        );
        let cancel = prep.cancel_handle();
        let started = Instant::now();
        let worker = std::thread::spawn(move || prep.run());
        std::thread::sleep(Duration::from_millis(20));
        cancel.cancel();
        assert_eq!(
            worker.join().unwrap().unwrap_err().code(),
            PLAYBACK_CANCELLED
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stop_and_owner_drop_cancel_the_pending_preparation() {
        let mut requests = PlaybackRequests::new();
        let prep = requests.begin(
            Path::new("/a.wav"),
            PREPARE_TIMEOUT,
            probe_after(Duration::from_secs(30), one_track()),
        );
        requests.supersede();
        assert_eq!(prep.run().unwrap_err().code(), PLAYBACK_CANCELLED);

        let prep = requests.begin(
            Path::new("/a.wav"),
            PREPARE_TIMEOUT,
            probe_after(Duration::from_secs(30), one_track()),
        );
        let cancel = prep.cancel_handle();
        drop(requests);
        assert!(cancel.is_cancelled());
        assert_eq!(prep.run().unwrap_err().code(), PLAYBACK_CANCELLED);
    }

    #[test]
    fn another_players_preparation_is_refused() {
        let mut ours = PlaybackRequests::new();
        let mut theirs = PlaybackRequests::new();
        ours.begin(
            Path::new("/a.wav"),
            PREPARE_TIMEOUT,
            probe_after(Duration::ZERO, one_track()),
        );
        let foreign = theirs
            .begin(
                Path::new("/a.wav"),
                PREPARE_TIMEOUT,
                probe_after(Duration::ZERO, one_track()),
            )
            .run()
            .unwrap();
        assert_eq!(ours.accept(&foreign).unwrap_err().code(), "invalid_input");
        assert!(ours.is_pending());
    }

    #[test]
    fn slow_probe_times_out_and_is_cancelled() {
        let mut requests = PlaybackRequests::new();
        let prep = requests.begin(
            Path::new("/slow.wav"),
            Duration::from_millis(40),
            probe_after(Duration::from_secs(30), one_track()),
        );
        let cancel = prep.cancel_handle();
        let error = prep.run().unwrap_err();
        assert_eq!(error.code(), "io");
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn probe_results_are_checked() {
        let mut requests = PlaybackRequests::new();
        let empty = ProbedPlayback {
            tracks: Vec::new(),
            duration_s: None,
        };
        let error = requests
            .begin(
                Path::new("/a.m4a"),
                PREPARE_TIMEOUT,
                probe_after(Duration::ZERO, empty),
            )
            .run()
            .unwrap_err();
        assert_eq!(error.code(), "decode");
        let no_length = ProbedPlayback {
            tracks: vec![1, 2],
            duration_s: Some(f64::NAN),
        };
        let prepared = requests
            .begin(
                Path::new("/a.m4a"),
                PREPARE_TIMEOUT,
                probe_after(Duration::ZERO, no_length),
            )
            .run()
            .unwrap();
        assert_eq!(prepared.tracks(), [1, 2]);
        assert_eq!(prepared.duration_s(), None);
        let failing: PlaybackProbe =
            Box::new(|_: &Path, _: &AtomicBool| Err(Error::NotFound("audio file".into())));
        let error = requests
            .begin(Path::new("/gone.wav"), PREPARE_TIMEOUT, failing)
            .run()
            .unwrap_err();
        assert_eq!(error.code(), "not_found");
        let clipped: PlaybackProbe =
            Box::new(|_: &Path, _: &AtomicBool| Err(needs_playable_cache("would shorten a track")));
        let error = requests
            .begin(Path::new("/two.m4a"), PREPARE_TIMEOUT, clipped)
            .run()
            .unwrap_err();
        assert_eq!(error.code(), PLAYBACK_NEEDS_CACHE);
    }

    #[test]
    fn ranges_are_checked() {
        assert!(check_range(0.0, None).is_ok());
        assert!(check_range(1.0, Some(1.5)).is_ok());
        for (start, end) in [
            (f64::NAN, None),
            (-0.1, None),
            (2.0, Some(1.0)),
            (1.0, Some(1.0)),
            (0.0, Some(f64::INFINITY)),
        ] {
            assert_eq!(check_range(start, end).unwrap_err().code(), "invalid_input");
        }
    }
}
