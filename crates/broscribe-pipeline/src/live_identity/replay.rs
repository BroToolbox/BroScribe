//! Isolated replay of the live-speakers worker over PCM (plan D
//! verification): no capture, no daemon. A synthetic journal is written
//! block by block exactly as the recorder would (closed, fsynced chunks;
//! optional device clock drift and a pause), the real [`Driver`] analyses
//! it as the worker does, and the run reports performance and an
//! end-to-end anchor check:
//!
//! - every span's journal anchor and a grid of synthetic "ASR turn" starts
//!   placed on the live ASR feed's clock (`broscribe_capture::anchors`) are
//!   mapped through the final 16 kHz timeline, as final publication maps a
//!   decision; `*_live_vs_final_max_ms` is how far live time and final time
//!   of the same journal frame differ (pauses, drift, skew correction);
//! - `anchor_roundtrip_max_samples`: live sample → journal frame → live
//!   sample, which must stay within rounding.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use broscribe_capture::anchors::{journal_point, placements};
use broscribe_capture::journal::{self, JournalWriter, SessionMeta};
use broscribe_capture::timeline::{Timeline, TimelineOptions};
use broscribe_core::{Error, Result};
use broscribe_ipc::Track;
use broscribe_ipc::live::{LiveIdentitySettings, SpeakersEvent, SpeakersStart};
use serde::Serialize;

use super::finalize::map_point;
use super::{Driver, EvidenceSource, SpeakerModels, capture_track, sample, seconds};

/// What to replay and how.
#[derive(Clone, Debug)]
pub struct ReplayOptions {
    /// A folder for the synthetic journal (created; the session must not
    /// exist yet).
    pub journal_root: PathBuf,
    pub session_id: String,
    pub track: Track,
    /// Rate of the PCM (the synthetic device rate).
    pub rate: u32,
    pub chunk_s: f64,
    /// Capture block length written at a time.
    pub block_s: f64,
    /// 0: as fast as possible; 1: real time; 2: twice real time.
    pub pace: f64,
    /// Device clock deviation applied to the synthetic host times (ppm).
    pub drift_ppm: f64,
    /// A pause after this many seconds of audio, lasting this long.
    pub pause: Option<(f64, f64)>,
    /// Spacing of the synthetic ASR turn starts checked (seconds).
    pub turn_s: f64,
    pub settings: LiveIdentitySettings,
}

impl ReplayOptions {
    pub fn new(journal_root: PathBuf, rate: u32) -> Self {
        Self {
            journal_root,
            session_id: "live-replay".into(),
            track: Track::System,
            rate,
            chunk_s: journal::CHUNK_SECONDS,
            block_s: 0.5,
            pace: 0.0,
            drift_ppm: 0.0,
            pause: None,
            turn_s: 1.0,
            settings: LiveIdentitySettings::default(),
        }
    }
}

/// Counts only: no audio, text or names.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ReplaySummary {
    pub audio_s: f64,
    pub wall_s: f64,
    pub windows: usize,
    pub behind_windows: usize,
    pub gaps: usize,
    pub warnings: usize,
    pub spans: usize,
    pub voices_max: usize,
    pub window_ms_max: u64,
    pub window_ms_mean: f64,
    /// Chunk closed → the window covering its end emitted.
    pub lag_ms_max: u64,
    pub lag_ms_mean: f64,
    pub retained_max_samples: usize,
    pub span_anchor_checks: usize,
    pub span_live_vs_final_max_ms: f64,
    pub turn_anchor_checks: usize,
    pub turns_unanchored: usize,
    pub turn_live_vs_final_max_ms: f64,
    pub anchor_roundtrip_max_samples: u64,
}

/// Replays `pcm` (mono, `opts.rate`) through a synthetic journal and the
/// live-speakers driver; `emit` sees every event as the worker would write
/// it.
pub fn replay(
    pcm: &[f32],
    opts: &ReplayOptions,
    models: &SpeakerModels<'_>,
    evidence: &mut dyn EvidenceSource,
    emit: &mut dyn FnMut(&SpeakersEvent) -> Result<()>,
) -> Result<ReplaySummary> {
    let positive = |v: f64| v.is_finite() && v > 0.0;
    let non_negative = |v: f64| v.is_finite() && v >= 0.0;
    if opts.rate == 0
        || !positive(opts.block_s)
        || !positive(opts.turn_s)
        || !positive(opts.chunk_s)
    {
        return Err(Error::InvalidInput(
            "replay rate, chunk, block and turn must be positive".into(),
        ));
    }
    // Pace 0 replays as fast as possible; drift keeps the clock moving
    // forward; a pause is a finite, non-negative point and length.
    if !non_negative(opts.pace)
        || !positive(1.0 + opts.drift_ppm * 1e-6)
        || opts
            .pause
            .is_some_and(|(at_s, len_s)| !non_negative(at_s) || !non_negative(len_s))
    {
        return Err(Error::InvalidInput(
            "replay pace, drift and pause must be finite, the clock moving forward".into(),
        ));
    }
    let ct = capture_track(opts.track);
    let mut writer = JournalWriter::create(
        &opts.journal_root,
        &opts.session_id,
        SessionMeta::default(),
        opts.chunk_s,
        std::process::id(),
    )?;
    writer.begin_segment(ct, 0, None, opts.rate, 0)?;
    let dir = writer.dir().to_owned();
    let mut driver = Driver::new(&SpeakersStart {
        session_id: opts.session_id.clone(),
        journal_dir: dir.clone(),
        epoch: 1,
        settings: opts.settings.clone(),
    })?;

    let started = Instant::now();
    let scale = 1.0 + opts.drift_ppm * 1e-6;
    let host = |frames: u64, paused_ns: u64| -> u64 {
        (frames as f64 / f64::from(opts.rate) * 1e9 * scale) as u64 + paused_ns
    };
    let block = ((opts.block_s * f64::from(opts.rate)) as usize).max(1);
    let mut paused_ns = 0u64;
    let mut paused = false;
    let mut closed_at: BTreeMap<u64, Instant> = BTreeMap::new();
    let mut summary = ReplaySummary {
        audio_s: pcm.len() as f64 / f64::from(opts.rate),
        ..ReplaySummary::default()
    };
    let mut window_ms: Vec<u64> = Vec::new();
    let mut lags: Vec<u64> = Vec::new();
    let mut span_anchors = Vec::new();

    let mut pos = 0usize;
    while pos < pcm.len() {
        if let Some((at_s, len_s)) = opts.pause
            && !paused
            && pos as f64 / f64::from(opts.rate) >= at_s
        {
            let at = host(pos as u64, paused_ns);
            writer.pause(at)?;
            paused_ns += (len_s * 1e9) as u64;
            writer.resume(at + (len_s * 1e9) as u64)?;
            paused = true;
        }
        let end = (pos + block).min(pcm.len());
        writer.append(ct, host(pos as u64, paused_ns), &pcm[pos..end])?;
        pos = end;
        if opts.pace > 0.0 {
            let due = Duration::try_from_secs_f64(pos as f64 / f64::from(opts.rate) / opts.pace)
                .map_err(|_| Error::InvalidInput("replay pace is too slow".into()))?;
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        let now = Instant::now();
        for c in &writer.manifest().track(ct).chunks {
            if c.frames.is_some() {
                closed_at.entry(c.seq).or_insert(now);
            }
        }
        let placed = placements(writer.manifest(), ct);
        let mut events = Vec::new();
        driver.step(models, evidence, &|_| {}, &mut |e| {
            events.push((Instant::now(), e));
            Ok(())
        })?;
        summary.retained_max_samples = summary
            .retained_max_samples
            .max(driver.retained().into_iter().max().unwrap_or(0));
        for (at, event) in events {
            note(
                &event,
                at,
                &placed,
                &closed_at,
                &mut summary,
                &mut window_ms,
                &mut lags,
                &mut span_anchors,
            );
            emit(&event)?;
        }
    }

    // Stop: close the last chunk, then let the driver finish.
    let manifest = writer.finish(host(pcm.len() as u64, paused_ns), "user")?;
    let now = Instant::now();
    for c in &manifest.track(ct).chunks {
        closed_at.entry(c.seq).or_insert(now);
    }
    let placed = placements(&manifest, ct);
    for _ in 0..1_000 {
        let mut events = Vec::new();
        let step = driver.step(models, evidence, &|_| {}, &mut |e| {
            events.push((Instant::now(), e));
            Ok(())
        })?;
        for (at, event) in events {
            note(
                &event,
                at,
                &placed,
                &closed_at,
                &mut summary,
                &mut window_ms,
                &mut lags,
                &mut span_anchors,
            );
            emit(&event)?;
        }
        if step.finished {
            break;
        }
    }
    summary.wall_s = started.elapsed().as_secs_f64();
    summary.window_ms_max = window_ms.iter().copied().max().unwrap_or(0);
    summary.window_ms_mean = mean(&window_ms);
    summary.lag_ms_max = lags.iter().copied().max().unwrap_or(0);
    summary.lag_ms_mean = mean(&lags);

    // End to end: anchors through the final timeline.
    let timeline = Timeline::plan(
        &dir,
        &manifest,
        &TimelineOptions {
            sample_rate: Some(16_000),
            ..Default::default()
        },
    )?;
    for (live, point) in span_anchors {
        if let Some(final_s) = map_point(&timeline, ct, point) {
            summary.span_anchor_checks += 1;
            let error = (final_s - seconds(live)).abs() * 1000.0;
            summary.span_live_vs_final_max_ms = summary.span_live_vs_final_max_ms.max(error);
        }
    }
    let live_end = placed.iter().map(|p| p.end()).max().unwrap_or(0);
    let mut at = 0.0;
    while sample(at) < live_end {
        let s = sample(at);
        at += opts.turn_s;
        let Some(point) = journal_point(&placed, s) else {
            summary.turns_unanchored += 1;
            continue;
        };
        if let Some(p) = placed.iter().rev().find(|p| p.seq == point.seq) {
            let step = f64::from(p.rate) / 16_000.0;
            let back = p.start + (point.frame as f64 / step).round() as u64;
            summary.anchor_roundtrip_max_samples =
                summary.anchor_roundtrip_max_samples.max(back.abs_diff(s));
        }
        match map_point(&timeline, ct, point) {
            Some(final_s) => {
                summary.turn_anchor_checks += 1;
                let error = (final_s - seconds(s)).abs() * 1000.0;
                summary.turn_live_vs_final_max_ms = summary.turn_live_vs_final_max_ms.max(error);
            }
            None => summary.turns_unanchored += 1,
        }
    }
    Ok(summary)
}

fn mean(values: &[u64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<u64>() as f64 / values.len() as f64
    }
}

#[allow(clippy::too_many_arguments)]
fn note(
    event: &SpeakersEvent,
    at: Instant,
    placed: &[broscribe_capture::anchors::Placement],
    closed_at: &BTreeMap<u64, Instant>,
    summary: &mut ReplaySummary,
    window_ms: &mut Vec<u64>,
    lags: &mut Vec<u64>,
    span_anchors: &mut Vec<(u64, broscribe_core::live_identity::JournalPoint)>,
) {
    match event {
        SpeakersEvent::Window(w) => {
            summary.windows += 1;
            summary.behind_windows += usize::from(w.behind);
            summary.spans += w.spans.len();
            summary.voices_max = summary.voices_max.max(w.voices.len());
            window_ms.push(w.elapsed_ms);
            if let Some(closed) = placed
                .iter()
                .rev()
                .find(|p| p.start < w.to_sample && w.to_sample <= p.end())
                .and_then(|p| closed_at.get(&p.seq))
            {
                lags.push(at.saturating_duration_since(*closed).as_millis() as u64);
            }
            for s in &w.spans {
                if let Some(point) = s.from {
                    span_anchors.push((s.from_sample, point));
                }
            }
        }
        SpeakersEvent::Gap { .. } => summary.gaps += 1,
        SpeakersEvent::Warning { .. } => summary.warnings += 1,
        // The driver analyses only after its models were loaded: model
        // loading (`loading`, `ready`) and liveness (`heartbeat`) are the
        // worker process's own events, never analysis progress, and a
        // session-sample reply is no window. None counts here.
        SpeakersEvent::Loading { .. }
        | SpeakersEvent::Ready { .. }
        | SpeakersEvent::Heartbeat { .. }
        | SpeakersEvent::SessionSample { .. } => {}
    }
}
