//! Fallback [`AudioPlayer`] through an `ffplay` subprocess, for platforms
//! without a native player in use (Windows, Linux).
//!
//! `ffplay -nodisp -autoexit` plays from a start offset for a duration.
//! Multi-track recordings (one `mic` and one `system` track) are mixed with a
//! lavfi `amovie … amix` graph, since ffplay plays one audio stream otherwise.
//! The position is the wall clock since the process started; pausing ends
//! the process and resuming starts a new one at the paused position (Windows
//! has no portable way to suspend a child), so a resume can repeat a few
//! milliseconds.
//!
//! Track probing (`ffprobe`) runs in [`PlaybackPreparation::run`], off the
//! UI loop. Stopped `ffplay` children are killed at once and reaped by one
//! background thread that waits at most [`REAP_TIMEOUT`] for each.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{LazyLock, mpsc};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::bounded::output_within;
use crate::ffmpeg_decoder::{file_url, find_binary};
use crate::player::{
    PREPARE_TIMEOUT, PlaybackPreparation, PlaybackRequests, PreparedPlayback, ProbedPlayback,
    check_audio_file, check_range,
};
use crate::{AudioPlayer, Error, PlayerState, Result};

#[derive(Debug)]
pub struct FfplayPlayer {
    ffplay: Option<PathBuf>,
    ffprobe: Option<PathBuf>,
    current: Option<Current>,
    requests: PlaybackRequests,
    rate: f32,
    /// Why a stopped or paused `ffplay` may still be playing: reported by
    /// the next [`AudioPlayer::poll`].
    stop_failure: Option<String>,
}

#[derive(Debug)]
struct Current {
    path: PathBuf,
    tracks: Vec<usize>,
    end_s: Option<f64>,
    duration_s: Option<f64>,
    rate: f32,
    state: Playback,
}

#[derive(Debug)]
enum Playback {
    Running {
        child: Child,
        from_s: f64,
        started: Instant,
    },
    Paused {
        at_s: f64,
    },
}

impl FfplayPlayer {
    /// Finds `ffplay` and `ffprobe` in `PATH`, then in the usual extra folders.
    pub fn locate() -> Self {
        Self::with_binaries(find_binary("ffplay"), find_binary("ffprobe"))
    }

    pub fn with_binaries(ffplay: Option<PathBuf>, ffprobe: Option<PathBuf>) -> Self {
        Self {
            ffplay,
            ffprobe,
            current: None,
            requests: PlaybackRequests::new(),
            rate: 1.0,
            stop_failure: None,
        }
    }

    pub fn is_available(&self) -> bool {
        self.ffplay.is_some() && self.ffprobe.is_some()
    }

    fn spawn(
        &self,
        path: &Path,
        tracks: &[usize],
        from_s: f64,
        end_s: Option<f64>,
        rate: f32,
    ) -> Result<Child> {
        let ffplay = self.ffplay.as_deref().ok_or_else(|| {
            Error::Unsupported("ffplay is not installed; cannot play audio".into())
        })?;
        Command::new(ffplay)
            .args(ffplay_args(path, tracks, from_s, end_s, rate))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::io(format!("running {}", ffplay.display()), e))
    }

    /// Ends the running child, if any, without waiting for it.
    fn end_current(&mut self) {
        if let Some(current) = self.current.take()
            && let Playback::Running { child, .. } = current.state
        {
            self.end_child(child);
        }
    }

    fn end_child(&mut self, child: Child) {
        if let Some(failure) = kill_and_reap(child) {
            self.stop_failure = Some(failure);
        }
    }
}

/// Track lists are a few short lines; anything larger is not ffprobe's answer.
const PROBE_MAX_OUTPUT: usize = 64 * 1024;

/// Checks `path` and lists its audio streams and length (background thread).
fn probe_tracks(ffprobe: &Path, path: &Path, cancel: &AtomicBool) -> Result<ProbedPlayback> {
    check_audio_file(path)?;
    let mut command = Command::new(ffprobe);
    command
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=index:format=duration",
            "-of",
            "json",
        ])
        .arg(file_url(path));
    let output = output_within(
        &mut command,
        PREPARE_TIMEOUT,
        PROBE_MAX_OUTPUT,
        Some(cancel),
    )?;
    if !output.status.success() {
        return Err(Error::Decode(format!(
            "ffprobe failed with {}",
            output.status
        )));
    }
    parse_probe(&output.stdout)
}

#[derive(Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    index: usize,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

/// Every audio stream is required (never silently omitted); the length is
/// optional metadata.
fn parse_probe(stdout: &[u8]) -> Result<ProbedPlayback> {
    let output: ProbeOutput = serde_json::from_slice(stdout)
        .map_err(|e| Error::Decode(format!("ffprobe returned an invalid answer: {e}")))?;
    if output.streams.is_empty() {
        return Err(Error::Decode("no audio tracks to play".into()));
    }
    Ok(ProbedPlayback {
        tracks: output.streams.iter().map(|s| s.index).collect(),
        duration_s: output
            .format
            .and_then(|f| f.duration)
            .and_then(|d| d.trim().parse::<f64>().ok()),
    })
}

/// Arguments for playing the absolute audio stream indices `tracks` of `path` from `from_s`,
/// stopping at `end_s`.
fn ffplay_args(
    path: &Path,
    tracks: &[usize],
    from_s: f64,
    end_s: Option<f64>,
    rate: f32,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> =
        ["-nodisp", "-autoexit", "-hide_banner", "-loglevel", "error"]
            .into_iter()
            .map(Into::into)
            .collect();
    let from_s = from_s.max(0.0);
    let duration = end_s.map(|end| (end - from_s).max(0.0));
    if tracks.len() <= 1 {
        args.push("-ss".into());
        args.push(format!("{from_s:.3}").into());
        if let Some(d) = duration {
            args.push("-t".into());
            args.push(format!("{d:.3}").into());
        }
        if rate != 1.0 {
            args.extend(["-af".into(), format!("atempo={rate}").into()]);
        }
        args.push(file_url(path));
    } else {
        let file = escape_filter_value(&path.to_string_lossy());
        let mut graph = String::new();
        for (i, stream_index) in tracks.iter().enumerate() {
            graph.push_str(&format!(
                "amovie={file}:si={stream_index}:sp={from_s:.3}[a{i}];"
            ));
        }
        for i in 0..tracks.len() {
            graph.push_str(&format!("[a{i}]"));
        }
        // `normalize=0`: each track keeps its own level, as a native mixer plays them.
        graph.push_str(&format!("amix=inputs={}:normalize=0", tracks.len()));
        if let Some(d) = duration {
            graph.push_str(&format!(",atrim=duration={d:.3}"));
        }
        if rate != 1.0 {
            graph.push_str(&format!(",atempo={rate}"));
        }
        graph.push_str("[out0]");
        args.extend(["-f".into(), "lavfi".into(), "-i".into(), graph.into()]);
    }
    args
}

/// Escapes a filter option value for a filtergraph description: first the
/// option level (`\`, `'`, `:`), then the graph level (`\`, `'`, `[`, `]`,
/// `,`, `;`).
fn escape_filter_value(value: &str) -> String {
    let escape = |s: &str, special: &[char]| {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if special.contains(&c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    let option = escape(value, &['\\', '\'', ':']);
    escape(&option, &['\\', '\'', '[', ']', ',', ';'])
}

/// Longest wait for a killed `ffplay` to exit before the reaper gives up on it.
pub const REAP_TIMEOUT: Duration = Duration::from_secs(5);
const REAP_POLL: Duration = Duration::from_millis(50);

/// Kills `child` without waiting and hands it to the reaper. Returns why it
/// may still be running when the kill failed.
fn kill_and_reap(mut child: Child) -> Option<String> {
    let pid = child.id();
    let failure = match child.kill() {
        Ok(()) => None,
        Err(e) => match child.try_wait() {
            Ok(Some(_)) => return None,
            _ => Some(format!("could not stop ffplay (process {pid}): {e}")),
        },
    };
    if !matches!(child.try_wait(), Ok(Some(_))) {
        reap_in_background(child);
    }
    failure
}

/// One reaper thread for every stopped child: never blocks the caller, and
/// waits at most [`REAP_TIMEOUT`] per child.
fn reap_in_background(child: Child) {
    static REAPER: LazyLock<Option<mpsc::Sender<Child>>> = LazyLock::new(|| {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("ffplay-reaper".into())
            .spawn(move || reap_loop(rx))
            .ok()
            .map(|_| tx)
    });
    let unsent = match &*REAPER {
        Some(tx) => tx.send(child).err().map(|e| e.0),
        None => Some(child),
    };
    if let Some(child) = unsent {
        tracing::warn!(
            pid = child.id(),
            "no reaper thread for a stopped ffplay; it exits unreaped"
        );
    }
}

fn reap_loop(rx: mpsc::Receiver<Child>) {
    let mut children: Vec<(Child, Instant)> = Vec::new();
    loop {
        let received = if children.is_empty() {
            match rx.recv() {
                Ok(child) => Some(child),
                Err(_) => return,
            }
        } else {
            match rx.recv_timeout(REAP_POLL) {
                Ok(child) => Some(child),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(REAP_POLL);
                    None
                }
            }
        };
        let deadline = Instant::now() + REAP_TIMEOUT;
        children.extend(
            received
                .into_iter()
                .chain(rx.try_iter())
                .map(|child| (child, deadline)),
        );
        reap_ready(&mut children, Instant::now());
    }
}

/// Drops children that exited, and those past their deadline (killed once
/// more and logged).
fn reap_ready(children: &mut Vec<(Child, Instant)>, now: Instant) {
    children.retain_mut(|(child, deadline)| match child.try_wait() {
        Ok(Some(_)) => false,
        Ok(None) if now < *deadline => true,
        Ok(None) => {
            let _ = child.kill();
            tracing::warn!(
                pid = child.id(),
                "ffplay did not exit within {} s of being killed; no longer waiting",
                REAP_TIMEOUT.as_secs()
            );
            false
        }
        Err(e) => {
            tracing::warn!(pid = child.id(), "waiting for a stopped ffplay: {e}");
            false
        }
    });
}

impl Current {
    fn position_s(&self) -> f64 {
        let pos = match &self.state {
            Playback::Running {
                from_s, started, ..
            } => from_s + started.elapsed().as_secs_f64() * f64::from(self.rate),
            Playback::Paused { at_s } => *at_s,
        };
        // The wall clock runs on while ffplay exits at the end of the file.
        let limit = match (self.end_s, self.duration_s) {
            (Some(end), Some(length)) => Some(end.min(length)),
            (end, length) => end.or(length),
        };
        limit.map_or(pos, |limit| pos.min(limit))
    }
}

impl AudioPlayer for FfplayPlayer {
    /// ffplay decodes whatever ffmpeg does; the file is checked when prepared.
    fn can_play_natively(&self, _path: &Path) -> bool {
        self.is_available()
    }

    fn prepare(&mut self, path: &Path) -> Result<PlaybackPreparation> {
        if self.ffplay.is_none() {
            return Err(Error::Unsupported(
                "ffplay is not installed; cannot play audio".into(),
            ));
        }
        let ffprobe = self.ffprobe.clone().ok_or_else(|| {
            Error::Unsupported("ffprobe is not installed; cannot inspect audio tracks".into())
        })?;
        Ok(self.requests.begin(
            path,
            PREPARE_TIMEOUT,
            Box::new(move |path: &Path, cancel: &AtomicBool| probe_tracks(&ffprobe, path, cancel)),
        ))
    }

    /// No blocking work beyond spawning ffplay.
    fn play_prepared(
        &mut self,
        prepared: PreparedPlayback,
        start_s: f64,
        end_s: Option<f64>,
    ) -> Result<()> {
        check_range(start_s, end_s)?;
        self.requests.accept(&prepared)?;
        let tracks = prepared.tracks().to_vec();
        let rate = if end_s.is_some() { 1.0 } else { self.rate };
        let child = self.spawn(prepared.path(), &tracks, start_s, end_s, rate)?;
        self.end_current();
        self.current = Some(Current {
            path: prepared.path().to_path_buf(),
            tracks,
            end_s,
            duration_s: prepared.duration_s(),
            rate,
            state: Playback::Running {
                child,
                from_s: start_s,
                started: Instant::now(),
            },
        });
        Ok(())
    }

    fn set_rate(&mut self, rate: f32) -> Result<()> {
        crate::player::check_rate(rate)?;
        if let Some(current) = &self.current
            && current.end_s.is_none()
            && current.rate != rate
        {
            if matches!(current.state, Playback::Running { .. }) {
                let from_s = current.position_s();
                let child = self.spawn(&current.path, &current.tracks, from_s, None, rate)?;
                let current = self.current.as_mut().expect("current playback");
                let old = std::mem::replace(
                    &mut current.state,
                    Playback::Running {
                        child,
                        from_s,
                        started: Instant::now(),
                    },
                );
                current.rate = rate;
                if let Playback::Running { child, .. } = old {
                    self.end_child(child);
                }
            } else if let Some(current) = &mut self.current {
                current.rate = rate;
            }
        }
        self.rate = rate;
        Ok(())
    }

    fn pause(&mut self) {
        let Some(current) = &mut self.current else {
            return;
        };
        if !matches!(current.state, Playback::Running { .. }) {
            return;
        }
        let at_s = current.position_s();
        if let Playback::Running { child, .. } =
            std::mem::replace(&mut current.state, Playback::Paused { at_s })
        {
            self.end_child(child);
        }
    }

    fn resume(&mut self) -> Result<()> {
        let Some(current) = &self.current else {
            return Ok(());
        };
        let Playback::Paused { at_s } = current.state else {
            return Ok(());
        };
        let child = self.spawn(
            &current.path,
            &current.tracks,
            at_s,
            current.end_s,
            current.rate,
        )?;
        if let Some(current) = &mut self.current {
            current.state = Playback::Running {
                child,
                from_s: at_s,
                started: Instant::now(),
            };
        }
        Ok(())
    }

    /// Never waits: the ffplay child is killed now and reaped in the
    /// background.
    fn stop(&mut self) {
        self.requests.supersede();
        self.end_current();
    }

    fn position_s(&self) -> Option<f64> {
        self.current.as_ref().map(Current::position_s)
    }

    fn is_playing(&self) -> bool {
        matches!(
            self.current.as_ref().map(|c| &c.state),
            Some(Playback::Running { .. })
        )
    }

    fn poll(&mut self) -> PlayerState {
        if let Some(message) = self.stop_failure.take() {
            self.end_current();
            return PlayerState::Failed { message };
        }
        let Some(current) = &mut self.current else {
            return PlayerState::Stopped;
        };
        let position_s = current.position_s();
        let exited = match &mut current.state {
            Playback::Paused { .. } => return PlayerState::Paused { position_s },
            Playback::Running { child, .. } => child.try_wait(),
        };
        match exited {
            Ok(None) => {
                if current.end_s.is_some_and(|end| position_s >= end) {
                    self.end_current();
                    PlayerState::Finished { position_s }
                } else {
                    PlayerState::Playing { position_s }
                }
            }
            Ok(Some(status)) if status.success() => {
                self.current = None;
                PlayerState::Finished { position_s }
            }
            Ok(Some(status)) => {
                self.current = None;
                PlayerState::Failed {
                    message: format!("ffplay exited with {status}"),
                }
            }
            Err(e) => {
                self.end_current();
                PlayerState::Failed {
                    message: format!("waiting for ffplay: {e}"),
                }
            }
        }
    }
}

impl Drop for FfplayPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::PLAYBACK_SUPERSEDED;

    fn strings(args: Vec<std::ffi::OsString>) -> Vec<String> {
        args.into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// A child that runs until killed: this test binary waiting on a test
    /// that sleeps.
    fn sleeper() -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "ffmpeg_player::tests::ffplay_sleeper_helper",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    #[ignore = "helper process for reaping tests"]
    fn ffplay_sleeper_helper() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn single_track_seeks_and_limits() {
        let args = strings(ffplay_args(
            Path::new("/m/a.m4a"),
            &[0],
            2.5,
            Some(4.0),
            1.0,
        ));
        assert_eq!(
            args[5..],
            ["-ss", "2.500", "-t", "1.500", "file:/m/a.m4a"].map(String::from)
        );
    }

    #[test]
    fn multi_track_mixes_with_escaped_path() {
        let args = strings(ffplay_args(
            Path::new("C:\\x,y [1]\\a'b.m4a"),
            &[1, 2],
            1.0,
            None,
            1.0,
        ));
        let graph = args.last().unwrap();
        assert!(args.contains(&"lavfi".to_string()));
        assert!(graph.contains(":si=1:sp=1.000[a0];"));
        assert!(graph.contains(":si=2:sp=1.000[a1];"));
        assert!(graph.ends_with("[a0][a1]amix=inputs=2:normalize=0[out0]"));
        // `\` → `\\` (option) → `\\\\` (graph); `'` → `\'` → `\\\'`; `:` → `\:` → `\\:`.
        assert_eq!(
            escape_filter_value("C:\\x,[a]'"),
            "C\\\\:\\\\\\\\x\\,\\[a\\]\\\\\\'"
        );
    }

    #[test]
    fn missing_ffplay_is_unsupported_and_idle() {
        let mut player = FfplayPlayer::with_binaries(None, None);
        assert!(!player.can_play_natively(Path::new("/nonexistent.m4a")));
        let err = player.prepare(Path::new("/nonexistent.m4a")).unwrap_err();
        assert_eq!(err.code(), "unsupported");
        let err = player
            .play(Path::new("/nonexistent.m4a"), 0.0, None)
            .unwrap_err();
        assert_eq!(err.code(), "unsupported");
        assert_eq!(player.poll(), PlayerState::Stopped);
        assert_eq!(player.position_s(), None);
        assert!(player.play(Path::new("/unused"), f64::NAN, None).is_err());
        assert!(player.play(Path::new("/unused"), 2.0, Some(1.0)).is_err());
    }

    #[test]
    fn probe_output_is_parsed_strictly() {
        let probed =
            parse_probe(br#"{"streams":[{"index":1},{"index":2}],"format":{"duration":"12.5"}}"#)
                .unwrap();
        assert_eq!(probed.tracks, [1, 2]);
        assert_eq!(probed.duration_s, Some(12.5));
        let unknown_length =
            parse_probe(br#"{"streams":[{"index":0}],"format":{"duration":"N/A"}}"#).unwrap();
        assert_eq!(unknown_length.duration_s, None);
        assert_eq!(parse_probe(b"").unwrap_err().code(), "decode");
        assert_eq!(
            parse_probe(br#"{"streams":[],"format":{}}"#)
                .unwrap_err()
                .code(),
            "decode"
        );
        assert_eq!(
            parse_probe(br#"{"streams":[{"index":"x"}]}"#)
                .unwrap_err()
                .code(),
            "decode"
        );
    }

    #[test]
    fn missing_file_fails_preparation_off_the_ui_loop() {
        let mut player = FfplayPlayer::with_binaries(
            Some(PathBuf::from("/unused/ffplay")),
            Some(PathBuf::from("/unused/ffprobe")),
        );
        let dir = tempfile::tempdir().unwrap();
        // Preparing does no file I/O; running it reports the missing file.
        let preparation = player.prepare(&dir.path().join("gone.m4a")).unwrap();
        assert_eq!(preparation.run().unwrap_err().code(), "not_found");
        let preparation = player.prepare(dir.path()).unwrap();
        assert_eq!(preparation.run().unwrap_err().code(), "invalid_input");
        assert!(!player.is_playing());
    }

    #[test]
    fn stop_cancels_the_pending_preparation() {
        let mut player = FfplayPlayer::with_binaries(
            Some(PathBuf::from("/unused/ffplay")),
            Some(PathBuf::from("/unused/ffprobe")),
        );
        let preparation = player.prepare(Path::new("/a.m4a")).unwrap();
        let cancel = preparation.cancel_handle();
        player.stop();
        assert!(cancel.is_cancelled());
        let preparation = player.prepare(Path::new("/b.m4a")).unwrap();
        let cancel = preparation.cancel_handle();
        drop(player);
        assert!(cancel.is_cancelled());
        assert_eq!(
            preparation.run().unwrap_err().code(),
            crate::player::PLAYBACK_CANCELLED
        );
    }

    #[test]
    fn superseded_preparation_is_refused() {
        // Probes without ffprobe through the request bookkeeping, as
        // `prepare` would with one.
        let mut player = FfplayPlayer::with_binaries(None, None);
        let first = player.requests.begin(
            Path::new("/a.m4a"),
            PREPARE_TIMEOUT,
            Box::new(|_: &Path, _: &AtomicBool| {
                Ok(ProbedPlayback {
                    tracks: vec![0],
                    duration_s: Some(2.0),
                })
            }),
        );
        let prepared = first.run().unwrap();
        player.stop();
        assert_eq!(
            player
                .play_prepared(prepared, 0.0, None)
                .unwrap_err()
                .code(),
            PLAYBACK_SUPERSEDED
        );
        assert_eq!(player.poll(), PlayerState::Stopped);
    }

    #[test]
    fn prepared_playback_validates_range_before_anything_changes() {
        let mut player = FfplayPlayer::with_binaries(None, None);
        let prepared = player
            .requests
            .begin(
                Path::new("/a.m4a"),
                PREPARE_TIMEOUT,
                Box::new(|_: &Path, _: &AtomicBool| {
                    Ok(ProbedPlayback {
                        tracks: vec![0],
                        duration_s: None,
                    })
                }),
            )
            .run()
            .unwrap();
        assert_eq!(
            player
                .play_prepared(prepared, 1.0, Some(0.5))
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        // The refused range left the request pending.
        assert!(player.requests.is_pending());
    }

    #[test]
    fn eof_position_is_capped_at_the_known_length() {
        let current = Current {
            path: PathBuf::from("/a.m4a"),
            tracks: vec![0],
            end_s: None,
            duration_s: Some(2.0),
            rate: 1.0,
            state: Playback::Paused { at_s: 2.4 },
        };
        assert_eq!(current.position_s(), 2.0);
        let current = Current {
            end_s: Some(1.5),
            ..current
        };
        assert_eq!(current.position_s(), 1.5);
    }

    #[test]
    fn stopping_never_waits_and_the_reaper_collects_the_child() {
        let child = sleeper();
        let started = Instant::now();
        assert_eq!(kill_and_reap(child), None);
        assert!(started.elapsed() < Duration::from_secs(1));

        let mut children = vec![(sleeper(), Instant::now() + REAP_TIMEOUT)];
        children[0].0.kill().unwrap();
        let until = Instant::now() + REAP_TIMEOUT;
        while !children.is_empty() && Instant::now() < until {
            reap_ready(&mut children, Instant::now());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(children.is_empty());
    }

    #[test]
    fn reaping_gives_up_after_its_deadline() {
        let mut children = vec![(sleeper(), Instant::now())];
        let pid = children[0].0.id();
        reap_ready(&mut children, Instant::now() + Duration::from_millis(1));
        assert!(children.is_empty(), "pid {pid} still tracked");
    }

    #[test]
    fn failed_stop_is_reported_once() {
        let mut player = FfplayPlayer::with_binaries(None, None);
        player.stop_failure = Some("could not stop ffplay (process 1): denied".into());
        assert_eq!(
            player.poll(),
            PlayerState::Failed {
                message: "could not stop ffplay (process 1): denied".into()
            }
        );
        assert_eq!(player.poll(), PlayerState::Stopped);
    }
}
