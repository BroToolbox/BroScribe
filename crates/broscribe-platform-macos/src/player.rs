//! Playback with AVFoundation: `AVAudioPlayer` for single-track files,
//! `AVPlayer` (which mixes every enabled audio track) for multi-track ones.
//!
//! File checks and track probing run in [`PlaybackPreparation::run`], off
//! the UI loop, through the same AVFoundation reading as
//! [`MacMediaDecoder`]: a file AVFoundation cannot read, or a multi-track
//! file whose per-track timing it cannot report truthfully (a single iTunes
//! gapless length applied to tracks of different lengths, which would cut
//! the longer one short), is refused with
//! [`PLAYBACK_NEEDS_CACHE`](broscribe_platform::player::PLAYBACK_NEEDS_CACHE)
//! so the caller plays the CLI's playable cache instead of a clipped mix.
//! `AVPlayer` stays on the main thread and loads its item asynchronously;
//! `poll` reports that wait as `Loading`, bounded by [`LOAD_TIMEOUT`]. No
//! timers or observers: the app polls while something plays, and
//! [`AudioPlayer::poll`] enforces `end_s`.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use broscribe_platform::player::{
    PREPARE_TIMEOUT, PlaybackPreparation, PlaybackRequests, PreparedPlayback, ProbedPlayback,
    check_audio_file, check_range, needs_playable_cache,
};
use broscribe_platform::{AudioPlayer, Error, MediaDecoder, PlayerState, Result};
use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker};
use objc2_av_foundation::{AVPlayer, AVPlayerStatus, AVPlayerTimeControlStatus};
use objc2_avf_audio::AVAudioPlayer;
use objc2_core_media::{CMTime, kCMTimeZero};
use objc2_foundation::{NSError, NSURL};

use crate::decoder::MacMediaDecoder;

/// Extensions AVFoundation plays directly. Other formats (ogg, opus, webm,
/// video containers) need the playable cache made by the CLI.
pub const NATIVE_EXTENSIONS: [&str; 8] = ["m4a", "mp3", "wav", "aac", "caf", "aif", "aiff", "flac"];

/// Longest time `AVPlayer` may wait before audio starts (or resumes after a
/// stall) before `poll` reports a failure.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Time scale for seeks (sample-accurate at 48 kHz).
const SEEK_TIMESCALE: i32 = 48_000;

fn ns_error(context: &str, e: &NSError) -> Error {
    Error::Decode(format!("{context}: {}", e.localizedDescription()))
}

enum Engine {
    Single(Retained<AVAudioPlayer>),
    Mixed(Retained<AVPlayer>),
}

impl Engine {
    fn position_s(&self) -> f64 {
        // SAFETY: plain property reads on a live player.
        let s = unsafe {
            match self {
                Engine::Single(p) => p.currentTime(),
                Engine::Mixed(p) => p.currentTime().seconds(),
            }
        };
        if s.is_finite() { s.max(0.0) } else { 0.0 }
    }

    /// `AVPlayer` was asked to play but is still loading (or stalled).
    fn is_loading(&self) -> bool {
        match self {
            Engine::Single(_) => false,
            // SAFETY: plain property read on a live player.
            Engine::Mixed(p) => unsafe {
                p.timeControlStatus() == AVPlayerTimeControlStatus::WaitingToPlayAtSpecifiedRate
            },
        }
    }

    fn is_playing(&self) -> bool {
        // SAFETY: plain property reads on a live player.
        unsafe {
            match self {
                Engine::Single(p) => p.isPlaying(),
                // `play` sets the rate at once; AVPlayer sets it back to 0
                // when the item ends (actionAtItemEnd = pause).
                Engine::Mixed(p) => p.rate() != 0.0,
            }
        }
    }

    fn pause(&self) {
        // SAFETY: valid on a live player in any state.
        unsafe {
            match self {
                Engine::Single(p) => p.pause(),
                Engine::Mixed(p) => p.pause(),
            }
        }
    }

    fn set_rate(&self, rate: f32, playing: bool) {
        // SAFETY: rate is validated and both players are live. AVPlayer
        // must remain paused when only its future rate changes.
        unsafe {
            match self {
                Engine::Single(p) => {
                    p.setEnableRate(true);
                    p.setRate(rate);
                }
                Engine::Mixed(p) if playing => p.setRate(rate),
                Engine::Mixed(_) => {}
            }
        }
    }

    fn play(&self, rate: f32) -> Result<()> {
        self.set_rate(rate, false);
        // SAFETY: valid on a live player in any state.
        let started = unsafe {
            match self {
                Engine::Single(p) => p.play(),
                Engine::Mixed(p) => {
                    if rate == 1.0 {
                        p.play();
                    } else {
                        p.setRate(rate);
                    }
                    true
                }
            }
        };
        if started {
            Ok(())
        } else {
            Err(Error::Decode("AVAudioPlayer refused to play".into()))
        }
    }

    fn stop(&self) {
        // SAFETY: valid on a live player in any state.
        unsafe {
            match self {
                Engine::Single(p) => p.stop(),
                Engine::Mixed(p) => p.pause(),
            }
        }
    }

    /// The failure AVPlayer found while loading, if any.
    fn failure(&self) -> Option<String> {
        let Engine::Mixed(p) = self else {
            return None;
        };
        // SAFETY: plain property reads on a live player.
        if unsafe { p.status() } != AVPlayerStatus::Failed {
            return None;
        }
        // SAFETY: as above.
        let error = unsafe { p.error() };
        Some(error.map_or_else(
            || "playback failed".into(),
            |e| e.localizedDescription().to_string(),
        ))
    }

    /// Where playback ended on its own.
    fn end_position_s(&self) -> f64 {
        match self {
            // AVAudioPlayer rewinds when it reaches the end.
            // SAFETY: plain property read on a live player.
            Engine::Single(p) => unsafe { p.duration() },
            Engine::Mixed(_) => self.position_s(),
        }
    }
}

struct Current {
    engine: Engine,
    end_s: Option<f64>,
    paused: bool,
    /// Where playback (re)started: reported while `AVPlayer` still loads.
    anchor_s: f64,
    /// When `poll` first saw the current loading wait.
    loading_since: Option<Instant>,
}

impl Current {
    fn position_s(&self) -> f64 {
        let pos = self.engine.position_s();
        let pos = if self.engine.is_loading() {
            pos.max(self.anchor_s)
        } else {
            pos
        };
        self.end_s.map_or(pos, |end| pos.min(end))
    }
}

/// Main-thread player (`!Send`). Multi-track files need the main thread
/// (AVPlayer is main-thread only); single-track playback works anywhere.
pub struct MacAudioPlayer {
    current: Option<Current>,
    requests: PlaybackRequests,
    rate: f32,
}

impl Default for MacAudioPlayer {
    fn default() -> Self {
        Self {
            current: None,
            requests: PlaybackRequests::new(),
            rate: 1.0,
        }
    }
}

impl MacAudioPlayer {
    pub fn new() -> Self {
        Self::default()
    }

    fn stop_current(&mut self) {
        if let Some(cur) = self.current.take() {
            cur.engine.stop();
        }
    }

    /// Opens the file on the caller's thread (`AVAudioPlayer` reads its
    /// header; the file was checked and probed off the UI loop).
    fn open_single(url: &NSURL, start_s: f64) -> Result<Engine> {
        // SAFETY: `url` is a valid file URL; the error out-parameter is
        // handled by the binding.
        let player =
            unsafe { AVAudioPlayer::initWithContentsOfURL_error(AVAudioPlayer::alloc(), url) }
                .map_err(|e| ns_error("opening audio", &e))?;
        // SAFETY: property writes / preparation on a freshly created player.
        unsafe {
            player.setCurrentTime(start_s);
            player.prepareToPlay();
        }
        Ok(Engine::Single(player))
    }

    fn open_mixed(url: &NSURL, start_s: f64) -> Result<Engine> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            Error::Unsupported("multi-track playback must run on the main thread".into())
        })?;
        // SAFETY: `url` is a valid file URL; seeking with zero tolerance is
        // valid before the item has loaded (it is applied once it has).
        unsafe {
            let player = AVPlayer::playerWithURL(url, mtm);
            if start_s > 0.0 {
                player.seekToTime_toleranceBefore_toleranceAfter(
                    CMTime::with_seconds(start_s, SEEK_TIMESCALE),
                    kCMTimeZero,
                    kCMTimeZero,
                );
            }
            Ok(Engine::Mixed(player))
        }
    }
}

impl AudioPlayer for MacAudioPlayer {
    fn can_play_natively(&self, path: &Path) -> bool {
        has_extension(path, &NATIVE_EXTENSIONS)
    }

    fn prepare(&mut self, path: &Path) -> Result<PlaybackPreparation> {
        if !self.can_play_natively(path) {
            return Err(Error::Unsupported(format!(
                "native playback of {}",
                path.display()
            )));
        }
        Ok(self.requests.begin(
            path,
            PREPARE_TIMEOUT,
            Box::new(|path: &Path, _cancel: &AtomicBool| probe_native(path)),
        ))
    }

    /// One track plays through `AVAudioPlayer`; several through `AVPlayer`,
    /// which needs the main thread (`unsupported` elsewhere).
    fn play_prepared(
        &mut self,
        prepared: PreparedPlayback,
        start_s: f64,
        end_s: Option<f64>,
    ) -> Result<()> {
        check_range(start_s, end_s)?;
        self.requests.accept(&prepared)?;
        let path = prepared.path();
        let url = NSURL::from_file_path(path)
            .ok_or_else(|| Error::InvalidInput(format!("file URL for {}", path.display())))?;
        let engine = match prepared.tracks() {
            [_] => Self::open_single(&url, start_s)?,
            _ => Self::open_mixed(&url, start_s)?,
        };
        self.stop_current();
        engine.play(if end_s.is_some() { 1.0 } else { self.rate })?;
        self.current = Some(Current {
            engine,
            end_s,
            paused: false,
            anchor_s: start_s,
            loading_since: None,
        });
        Ok(())
    }

    fn set_rate(&mut self, rate: f32) -> Result<()> {
        broscribe_platform::player::check_rate(rate)?;
        self.rate = rate;
        if let Some(cur) = &self.current
            && cur.end_s.is_none()
        {
            cur.engine.set_rate(rate, !cur.paused);
        }
        Ok(())
    }

    fn pause(&mut self) {
        if let Some(cur) = &mut self.current {
            cur.anchor_s = cur.position_s();
            cur.engine.pause();
            cur.paused = true;
            cur.loading_since = None;
        }
    }

    fn resume(&mut self) -> Result<()> {
        let cur = self
            .current
            .as_mut()
            .ok_or_else(|| Error::InvalidInput("nothing to resume".into()))?;
        cur.engine
            .play(if cur.end_s.is_some() { 1.0 } else { self.rate })?;
        cur.paused = false;
        Ok(())
    }

    /// Never waits: pausing an `AVPlayer` or stopping an `AVAudioPlayer`
    /// returns at once.
    fn stop(&mut self) {
        self.requests.supersede();
        self.stop_current();
    }

    fn position_s(&self) -> Option<f64> {
        self.current.as_ref().map(Current::position_s)
    }

    fn is_playing(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|c| !c.paused && c.engine.is_playing())
    }

    fn poll(&mut self) -> PlayerState {
        let Some(cur) = &mut self.current else {
            return PlayerState::Stopped;
        };
        if let Some(message) = cur.engine.failure() {
            self.stop_current();
            return PlayerState::Failed { message };
        }
        if !cur.paused && cur.engine.is_loading() {
            let since = *cur.loading_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= LOAD_TIMEOUT {
                self.stop_current();
                return PlayerState::Failed {
                    message: format!(
                        "the audio did not start within {} s",
                        LOAD_TIMEOUT.as_secs()
                    ),
                };
            }
            return PlayerState::Loading {
                position_s: cur.position_s(),
            };
        }
        cur.loading_since = None;
        let position_s = cur.engine.position_s();
        if let Some(end) = cur.end_s
            && position_s >= end
        {
            self.stop_current();
            return PlayerState::Finished { position_s: end };
        }
        if cur.paused {
            return PlayerState::Paused { position_s };
        }
        if !cur.engine.is_playing() {
            let position_s = cur.engine.end_position_s();
            self.stop_current();
            return PlayerState::Finished { position_s };
        }
        PlayerState::Playing { position_s }
    }
}

impl Drop for MacAudioPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Checks `path` and lists its tracks with AVFoundation (background thread).
fn probe_native(path: &Path) -> Result<ProbedPlayback> {
    check_audio_file(path)?;
    let info = MacMediaDecoder.probe(path).map_err(|e| match e {
        Error::Unsupported(reason) => needs_playable_cache(reason),
        other => other,
    })?;
    // `MacMediaDecoder` leaves a duration unknown exactly when it cannot
    // stand behind AVFoundation's timing; `AVPlayer` follows that timing.
    if info.audio_tracks > 1 && info.duration_s.is_none() {
        return Err(needs_playable_cache(format!(
            "{}: AVFoundation would play its {} audio tracks only as long as the \
             first one; play the playable cache",
            path.display(),
            info.audio_tracks
        )));
    }
    Ok(ProbedPlayback {
        tracks: (0..info.audio_tracks as usize).collect(),
        duration_s: info.duration_s,
    })
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| extensions.iter().any(|n| n.eq_ignore_ascii_case(e)))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use super::*;

    /// Writes a mono 16-bit PCM WAV of a 440 Hz tone.
    fn write_tone_wav(path: &Path, seconds: f32, rate: u32) {
        let n = (seconds * rate as f32) as u32;
        let mut f = std::fs::File::create(path).unwrap();
        let data_len = n * 2;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        f.write_all(&rate.to_le_bytes()).unwrap();
        f.write_all(&(rate * 2).to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_len.to_le_bytes()).unwrap();
        for i in 0..n {
            let t = i as f32 / rate as f32;
            let s = (0.2 * (std::f32::consts::TAU * 440.0 * t).sin() * f32::from(i16::MAX)) as i16;
            f.write_all(&s.to_le_bytes()).unwrap();
        }
    }

    fn tone(dir: &Path) -> PathBuf {
        let path = dir.join("tone.wav");
        write_tone_wav(&path, 1.0, 16_000);
        path
    }

    #[test]
    fn generated_wav_is_natively_playable() {
        let dir = tempfile::tempdir().unwrap();
        let path = tone(dir.path());
        let player = MacAudioPlayer::new();
        assert!(player.can_play_natively(&path));
        assert!(player.can_play_natively(Path::new("x.M4A")));
        assert!(!player.can_play_natively(Path::new("x.ogg")));
        assert!(!player.can_play_natively(Path::new("x.mp4")));
        assert!(!player.can_play_natively(Path::new("noext")));
    }

    #[test]
    fn missing_and_unsupported_files_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut player = MacAudioPlayer::new();
        let error = player
            .play(&dir.path().join("gone.wav"), 0.0, None)
            .unwrap_err();
        assert_eq!(error.code(), "not_found");
        let ogg = dir.path().join("a.ogg");
        std::fs::write(&ogg, b"not audio").unwrap();
        assert_eq!(player.prepare(&ogg).unwrap_err().code(), "unsupported");
        assert_eq!(player.poll(), PlayerState::Stopped);
        assert_eq!(player.position_s(), None);
    }

    #[test]
    fn preparation_probes_tracks_and_length_without_playing() {
        let dir = tempfile::tempdir().unwrap();
        let path = tone(dir.path());
        let mut player = MacAudioPlayer::new();
        let preparation = player.prepare(&path).unwrap();
        let prepared = std::thread::spawn(move || preparation.run())
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(prepared.tracks(), [0]);
        assert!((prepared.duration_s().unwrap() - 1.0).abs() < 1e-3);
        // A refused range changes nothing and keeps the request.
        assert_eq!(
            player
                .play_prepared(prepared, 0.5, Some(0.25))
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        assert!(!player.is_playing());
    }

    #[test]
    fn files_avfoundation_cannot_play_whole_need_the_cache() {
        use broscribe_platform::encoder::SliceSource;
        use broscribe_platform::player::PLAYBACK_NEEDS_CACHE;
        use broscribe_platform::{AudioEncoder, EncodeSettings, EncoderTrack};

        let dir = tempfile::tempdir().unwrap();
        let rate = 48_000;
        let encode = |path: &Path, lengths: &[usize]| {
            let mut sources: Vec<SliceSource> = lengths
                .iter()
                .map(|&n| SliceSource::new(vec![0.1; n]))
                .collect();
            let mut tracks: Vec<EncoderTrack<'_>> = ["mic", "system"]
                .into_iter()
                .zip(sources.iter_mut())
                .map(|(title, source)| EncoderTrack { title, source })
                .collect();
            crate::encoder::MacAudioEncoder
                .encode(path, &EncodeSettings::speech(rate), &mut tracks)
                .unwrap();
        };
        // A recording whose system track outlasts the microphone: its single
        // gapless length would stop the mix after the first track.
        let two = dir.path().join("two.m4a");
        encode(&two, &[48_000, 72_000]);
        assert_eq!(probe_native(&two).unwrap_err().code(), PLAYBACK_NEEDS_CACHE);
        let one = dir.path().join("one.m4a");
        encode(&one, &[48_000]);
        let probed = probe_native(&one).unwrap();
        assert_eq!(probed.tracks, [0]);
        assert!(probed.duration_s.is_some_and(|d| (d - 1.0).abs() < 0.05));

        let garbage = dir.path().join("a.caf");
        std::fs::write(&garbage, b"not audio").unwrap();
        assert_eq!(
            probe_native(&garbage).unwrap_err().code(),
            PLAYBACK_NEEDS_CACHE
        );
        assert_eq!(
            probe_native(&dir.path().join("gone.m4a"))
                .unwrap_err()
                .code(),
            "not_found"
        );
        assert_eq!(
            probe_native(dir.path()).unwrap_err().code(),
            "invalid_input"
        );
    }

    #[test]
    fn stop_and_drop_cancel_a_pending_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let path = tone(dir.path());
        let mut player = MacAudioPlayer::new();
        let stale = player.prepare(&path).unwrap().run().unwrap();
        player.stop();
        assert_eq!(
            player.play_prepared(stale, 0.0, None).unwrap_err().code(),
            broscribe_platform::player::PLAYBACK_SUPERSEDED
        );
        let pending = player.prepare(&path).unwrap();
        drop(player);
        assert_eq!(
            pending.run().unwrap_err().code(),
            broscribe_platform::player::PLAYBACK_CANCELLED
        );
    }

    #[test]
    #[ignore = "plays sound on the default output device"]
    fn plays_a_bounded_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = tone(dir.path());
        let mut player = MacAudioPlayer::new();
        player.play(&path, 0.2, Some(0.6)).unwrap();
        assert!(player.is_playing());
        let started = std::time::Instant::now();
        loop {
            match player.poll() {
                PlayerState::Playing { position_s } => assert!(position_s >= 0.19),
                PlayerState::Finished { position_s } => {
                    assert!((position_s - 0.6).abs() < 1e-9);
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
            assert!(
                started.elapsed().as_secs_f64() < 2.0,
                "did not stop at end_s"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!player.is_playing());
        assert_eq!(player.poll(), PlayerState::Stopped);
    }
}
