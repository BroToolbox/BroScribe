//! The common timeline (plan M4 §12.6): places every journaled chunk of both
//! tracks on recording time, zero-fills capture gaps, corrects each
//! segment's clock skew against the host clock and resamples every track
//! to one rate. The result is two streams of exactly equal length.
//!
//! Runs: consecutive chunks of one segment that continue each other. A new
//! run starts at a segment change and at a chunk flagged `after_gap`
//! (capture gap, dropped blocks, pause). Inside a run samples are never
//! inserted or removed; each run starts at its host time mapped to
//! recording time (pauses removed). A hole before a run is zero-filled; an
//! overlap (host-time jitter) skips the run's first samples.
//!
//! Skew: per segment, the chunk start times are regressed on the frame
//! count within each run (pooled over runs). The fitted rate replaces the
//! nominal one only when the fit spans long enough and the deviation is
//! significant and plausible; otherwise the nominal rate stands.

use std::path::{Path, PathBuf};

use broscribe_platform::PcmSource;

use crate::journal::{ChunkEntry, Cutoff, SessionManifest, TimeMap, Track, read_chunk};
use crate::resample::Resampler;
use crate::{Error, Result};

/// Largest skew accepted as a clock difference (ppm); beyond it the
/// estimate is treated as noise.
pub const MAX_SKEW_PPM: f64 = 1000.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimelineOptions {
    /// Output rate; default: the highest nominal rate of any segment, at
    /// most 48 kHz.
    pub sample_rate: Option<u32>,
    /// Read only chunks up to this cutoff (process now).
    pub cutoff: Option<Cutoff>,
    /// Shortest span of contiguous audio (seconds) a skew estimate needs.
    pub min_skew_span_s: f64,
}

impl Default for TimelineOptions {
    fn default() -> Self {
        Self {
            sample_rate: None,
            cutoff: None,
            min_skew_span_s: 300.0,
        }
    }
}

/// One segment's clock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SegmentClock {
    pub segment: u32,
    pub nominal_rate: u32,
    /// Rate against the host clock (Hz); equals the nominal rate when no
    /// skew was applied.
    pub effective_rate: f64,
    /// Applied skew in parts per million (0 when not applied).
    pub skew_ppm: f64,
    /// The estimate before the significance test, if one was possible.
    pub estimated_ppm: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub segment: u32,
    pub chunks: Vec<ChunkEntry>,
    /// Output frame where the run starts.
    pub start_frame: u64,
    /// Input frames skipped at the start (overlap with the previous run).
    pub skip_input: u64,
    pub input_frames: u64,
    pub output_frames: u64,
    pub effective_rate: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackPlan {
    pub track: Track,
    pub clocks: Vec<SegmentClock>,
    pub runs: Vec<Run>,
    /// Frames of zero-fill inserted for gaps (not counting trailing padding).
    pub gap_frames: u64,
    /// End of the last run.
    pub end_frame: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Timeline {
    pub dir: PathBuf,
    pub sample_rate: u32,
    /// Length of every track.
    pub frames: u64,
    pub time_map: TimeMap,
    /// Mic, then system.
    pub tracks: [TrackPlan; 2],
}

impl Timeline {
    /// Plans the timeline of the journal in `dir`.
    pub fn plan(dir: &Path, manifest: &SessionManifest, options: &TimelineOptions) -> Result<Self> {
        crate::journal::validate_sample_rates(manifest)?;
        let rate = match options.sample_rate {
            Some(r) if r > 0 => r,
            Some(_) => return Err(Error::InvalidInput("timeline sample rate is 0".into())),
            None => Track::ALL
                .iter()
                .flat_map(|t| manifest.track(*t).segments.iter().map(|s| s.sample_rate))
                .max()
                .unwrap_or(48_000)
                .min(48_000),
        };
        let time_map = manifest.time_map();
        let plan = |track| plan_track(manifest, track, rate, &time_map, options);
        let tracks = [plan(Track::Mic)?, plan(Track::System)?];
        let frames = tracks.iter().map(|t| t.end_frame).max().unwrap_or(0);
        Ok(Self {
            dir: dir.to_path_buf(),
            sample_rate: rate,
            frames,
            time_map,
            tracks,
        })
    }

    pub fn duration_s(&self) -> f64 {
        self.frames as f64 / f64::from(self.sample_rate)
    }

    pub fn track(&self, track: Track) -> &TrackPlan {
        &self.tracks[track.index()]
    }

    /// A stream of `track` on the timeline, `frames` long.
    pub fn stream(&self, track: Track) -> TrackStream {
        TrackStream {
            dir: self.dir.clone(),
            track,
            rate: self.sample_rate,
            runs: self.track(track).runs.clone(),
            total: self.frames,
            pos: 0,
            run: 0,
            active: None,
            spare: None,
        }
    }

    /// The whole track in memory (short sessions, tests).
    pub fn read_track(&self, track: Track) -> Result<Vec<f32>> {
        let mut stream = self.stream(track);
        let mut out = vec![0.0; self.frames as usize];
        let mut filled = 0;
        while filled < out.len() {
            let n = stream.read(&mut out[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        out.truncate(filled);
        Ok(out)
    }
}

fn plan_track(
    manifest: &SessionManifest,
    track: Track,
    rate: u32,
    time_map: &TimeMap,
    options: &TimelineOptions,
) -> Result<TrackPlan> {
    let journal = manifest.track(track);
    let limit = match options.cutoff {
        Some(cutoff) => match cutoff.get(track) {
            Some(seq) => Some(seq),
            None => {
                return Ok(TrackPlan {
                    track,
                    clocks: Vec::new(),
                    runs: Vec::new(),
                    gap_frames: 0,
                    end_frame: 0,
                });
            }
        },
        None => None,
    };
    let chunks = journal
        .chunks
        .iter()
        .filter(|c| c.frames.is_some_and(|f| f > 0))
        .filter(|c| limit.is_none_or(|l| c.seq <= l));

    // Group into runs.
    let mut groups: Vec<Vec<ChunkEntry>> = Vec::new();
    for chunk in chunks {
        let continues = groups
            .last()
            .and_then(|g| g.last())
            .is_some_and(|prev| prev.segment == chunk.segment && !chunk.after_gap);
        match groups.last_mut() {
            Some(group) if continues => group.push(chunk.clone()),
            _ => groups.push(vec![chunk.clone()]),
        }
    }

    let clocks: Vec<SegmentClock> = journal
        .segments
        .iter()
        .map(|s| {
            let runs: Vec<&Vec<ChunkEntry>> =
                groups.iter().filter(|g| g[0].segment == s.index).collect();
            segment_clock(s.index, s.sample_rate, &runs, options.min_skew_span_s)
        })
        .collect();

    let mut runs = Vec::with_capacity(groups.len());
    let mut pos = 0u64;
    let mut gap_frames = 0u64;
    for group in groups {
        let segment = group[0].segment;
        let clock = clocks
            .iter()
            .find(|c| c.segment == segment)
            .ok_or_else(|| {
                Error::Schema(format!(
                    "{} chunk {} refers to missing segment {segment}",
                    track.as_str(),
                    group[0].seq
                ))
            })?;
        let input: u64 = group.iter().filter_map(|c| c.frames).sum();
        let rec_ns = time_map.recording_ns(group[0].host_start_ns);
        let desired = (u128::from(rec_ns) * u128::from(rate) / 1_000_000_000) as u64;
        let (start_frame, skip_input) = if desired >= pos {
            gap_frames += desired - pos;
            (desired, 0)
        } else {
            let overlap = pos - desired;
            let skip = ((overlap as f64) * clock.effective_rate / f64::from(rate)).ceil() as u64;
            (pos, skip.min(input))
        };
        let output_frames =
            ((input - skip_input) as f64 * f64::from(rate) / clock.effective_rate).round() as u64;
        pos = start_frame + output_frames;
        runs.push(Run {
            segment,
            chunks: group,
            start_frame,
            skip_input,
            input_frames: input,
            output_frames,
            effective_rate: clock.effective_rate,
        });
    }
    Ok(TrackPlan {
        track,
        clocks,
        runs,
        gap_frames,
        end_frame: pos,
    })
}

/// Pooled least-squares fit of host time on frame position over the runs
/// of one segment.
fn segment_clock(
    segment: u32,
    nominal_rate: u32,
    runs: &[&Vec<ChunkEntry>],
    min_span_s: f64,
) -> SegmentClock {
    let nominal = SegmentClock {
        segment,
        nominal_rate,
        effective_rate: f64::from(nominal_rate),
        skew_ppm: 0.0,
        estimated_ppm: None,
    };
    let mut sxx = 0.0f64;
    let mut sxy = 0.0f64;
    let mut points = 0usize;
    let mut fitted_runs = 0usize;
    let mut span_frames = 0u64;
    let mut series: Vec<Vec<(f64, f64)>> = Vec::new();
    for run in runs {
        if run.len() < 2 {
            continue;
        }
        let mut x = 0u64;
        let base = run[0].host_start_ns;
        let pts: Vec<(f64, f64)> = run
            .iter()
            .map(|c| {
                let p = (x as f64, c.host_start_ns.saturating_sub(base) as f64);
                x += c.frames.unwrap_or(0);
                p
            })
            .collect();
        span_frames += pts.last().map_or(0, |p| p.0 as u64);
        series.push(pts);
    }
    for pts in &series {
        let n = pts.len() as f64;
        let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
        let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
        for (x, y) in pts {
            sxx += (x - mx) * (x - mx);
            sxy += (x - mx) * (y - my);
        }
        points += pts.len();
        fitted_runs += 1;
    }
    if sxx <= 0.0 || points < fitted_runs + 2 || nominal_rate == 0 {
        return nominal;
    }
    let slope = sxy / sxx; // ns per frame
    if slope <= 0.0 {
        return nominal;
    }
    let mut rss = 0.0;
    for pts in &series {
        let n = pts.len() as f64;
        let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
        let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
        for (x, y) in pts {
            let r = (y - my) - slope * (x - mx);
            rss += r * r;
        }
    }
    let dof = (points - fitted_runs - 1).max(1) as f64;
    let se_slope = (rss / dof / sxx).sqrt();
    let effective = 1e9 / slope;
    let ppm = (effective / f64::from(nominal_rate) - 1.0) * 1e6;
    let se_ppm = se_slope / slope * 1e6;
    let span_s = span_frames as f64 / f64::from(nominal_rate);
    let significant = ppm.abs() >= 2.0 * se_ppm;
    if span_s >= min_span_s && significant && ppm.abs() <= MAX_SKEW_PPM {
        SegmentClock {
            segment,
            nominal_rate,
            effective_rate: effective,
            skew_ppm: ppm,
            estimated_ppm: Some(ppm),
        }
    } else {
        SegmentClock {
            estimated_ppm: Some(ppm),
            ..nominal
        }
    }
}

struct ActiveRun {
    resampler: Resampler,
    next_chunk: usize,
    skip: u64,
    emitted: u64,
}

/// One track of a [`Timeline`] as a [`PcmSource`].
pub struct TrackStream {
    dir: PathBuf,
    track: Track,
    rate: u32,
    runs: Vec<Run>,
    total: u64,
    pos: u64,
    run: usize,
    active: Option<ActiveRun>,
    /// The last finished run's resampler and input rate, reset for the next
    /// run at that rate instead of rebuilding its kernel.
    spare: Option<(f64, Resampler)>,
}

impl TrackStream {
    pub fn track(&self) -> Track {
        self.track
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    pub fn total_frames(&self) -> u64 {
        self.total
    }

    fn zeros(&mut self, out: &mut [f32], until: u64) -> usize {
        let n = (until - self.pos).min(out.len() as u64) as usize;
        out[..n].fill(0.0);
        self.pos += n as u64;
        n
    }
}

impl PcmSource for TrackStream {
    fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        if out.is_empty() || self.pos >= self.total {
            return Ok(0);
        }
        // Runs that produce nothing (fully overlapped) are skipped, so a
        // read returns 0 only at the end of the stream.
        while self
            .runs
            .get(self.run)
            .is_some_and(|r| r.output_frames == 0)
        {
            self.run += 1;
        }
        let Some(run) = self.runs.get(self.run) else {
            let total = self.total;
            return Ok(self.zeros(out, total));
        };
        if self.pos < run.start_frame {
            let start = run.start_frame.min(self.total);
            return Ok(self.zeros(out, start));
        }
        let rate = self.rate;
        if self.active.is_none() {
            let resampler = match self.spare.take() {
                Some((input_rate, mut resampler)) if input_rate == run.effective_rate => {
                    resampler.reset();
                    resampler
                }
                _ => Resampler::new(run.effective_rate, f64::from(rate)),
            };
            self.active = Some(ActiveRun {
                resampler,
                next_chunk: 0,
                skip: run.skip_input,
                emitted: 0,
            });
        }
        let Some(active) = self.active.as_mut() else {
            return Err(Error::Internal("timeline run did not start".into()));
        };
        let want = ((run.output_frames - active.emitted) as usize)
            .min(out.len())
            .min((self.total - self.pos) as usize);
        let mut filled = 0;
        while filled < want {
            let n = active.resampler.pull(&mut out[filled..want]);
            filled += n;
            if filled == want {
                break;
            }
            if n == 0 {
                match run.chunks.get(active.next_chunk) {
                    Some(chunk) => {
                        let samples = read_chunk(&self.dir, self.track, chunk)?;
                        active.next_chunk += 1;
                        let skip = (active.skip as usize).min(samples.len());
                        active.skip -= skip as u64;
                        active.resampler.push(&samples[skip..]);
                    }
                    None => {
                        active.resampler.finish();
                        let more = active.resampler.pull(&mut out[filled..want]);
                        if more == 0 {
                            // Input exhausted (rounding): pad the run's end.
                            out[filled..want].fill(0.0);
                            filled = want;
                        } else {
                            filled += more;
                        }
                    }
                }
            }
        }
        active.emitted += filled as u64;
        self.pos += filled as u64;
        if active.emitted >= run.output_frames {
            if let Some(done) = self.active.take() {
                self.spare = Some((run.effective_rate, done.resampler));
            }
            self.run += 1;
        }
        Ok(filled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{JournalWriter, SessionMeta, load_manifest};

    /// A journal at `rate` Hz with 1 s chunks.
    fn journal(root: &Path) -> JournalWriter {
        JournalWriter::create(root, "t", SessionMeta::default(), 1.0, 1).unwrap()
    }

    fn tone(n: usize, value: f32) -> Vec<f32> {
        vec![value; n]
    }

    #[test]
    fn zero_rate_segment_is_rejected_on_load_and_when_planning() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = journal(root.path());
        writer
            .begin_segment(Track::Mic, 0, None, 16_000, 0)
            .unwrap();
        writer.append(Track::Mic, 0, &[0.5; 16]).unwrap();
        let dir = writer.dir().to_owned();
        let mut manifest = writer.finish(1_000_000, "user").unwrap();
        manifest.mic.segments[0].sample_rate = 0;
        crate::journal::write_manifest(&dir, &manifest).unwrap();

        assert!(matches!(load_manifest(&dir), Err(Error::Schema(_))));
        let options = TimelineOptions {
            sample_rate: Some(16_000),
            ..Default::default()
        };
        assert!(matches!(
            Timeline::plan(&dir, &manifest, &options),
            Err(Error::Schema(_))
        ));
    }

    #[test]
    fn gap_is_zero_filled_and_tracks_are_equal_length() {
        let root = tempfile::tempdir().unwrap();
        let mut w = journal(root.path());
        w.begin_segment(Track::Mic, 0, None, 1000, 0).unwrap();
        w.begin_segment(Track::System, 0, None, 1000, 0).unwrap();
        w.append(Track::Mic, 0, &tone(3000, 0.5)).unwrap();
        // System: 1 s of audio, a 1 s tap gap, 1.5 s more.
        w.append(Track::System, 0, &tone(1000, 0.25)).unwrap();
        w.append(Track::System, 2_000_000_000, &tone(1500, 0.75))
            .unwrap();
        let dir = w.dir().to_path_buf();
        let m = w.finish(3_500_000_000, "user").unwrap();

        let t = Timeline::plan(&dir, &m, &TimelineOptions::default()).unwrap();
        assert_eq!(t.sample_rate, 1000);
        assert_eq!(t.frames, 3500);
        let sys = t.read_track(Track::System).unwrap();
        let mic = t.read_track(Track::Mic).unwrap();
        assert_eq!(sys.len(), 3500);
        assert_eq!(mic.len(), 3500);
        assert!(sys[..1000].iter().all(|s| *s == 0.25));
        assert!(sys[1000..2000].iter().all(|s| *s == 0.0), "gap zero-filled");
        assert!(sys[2000..].iter().all(|s| *s == 0.75));
        assert!(mic[..3000].iter().all(|s| *s == 0.5));
        assert!(
            mic[3000..].iter().all(|s| *s == 0.0),
            "padded to the longest track"
        );
        assert_eq!(t.track(Track::System).gap_frames, 1000);
    }

    #[test]
    fn skew_is_estimated_and_corrected() {
        let root = tempfile::tempdir().unwrap();
        let mut w = journal(root.path());
        // The system device runs 500 ppm fast: 1000.5 frames per host second.
        let true_rate = 1000.5f64;
        w.begin_segment(Track::System, 0, None, 1000, 0).unwrap();
        w.begin_segment(Track::Mic, 0, None, 1000, 0).unwrap();
        let mut frames = 0u64;
        for _ in 0..120 {
            // 100 ms blocks stamped by the host clock.
            let host = (frames as f64 / true_rate * 1e9) as u64;
            w.append(Track::System, host, &tone(100, 0.5)).unwrap();
            frames += 100;
        }
        w.append(Track::Mic, 0, &tone(12_000, 0.5)).unwrap();
        let dir = w.dir().to_path_buf();
        let m = w.finish(12_000_000_000, "user").unwrap();

        let options = TimelineOptions {
            min_skew_span_s: 5.0,
            ..TimelineOptions::default()
        };
        let t = Timeline::plan(&dir, &m, &options).unwrap();
        let clock = t.track(Track::System).clocks[0];
        assert!((clock.skew_ppm - 500.0).abs() < 5.0, "{clock:?}");
        // 12 000 device frames last 11.994 host seconds.
        let out = t.track(Track::System).end_frame;
        assert!((out as i64 - 11_994).abs() <= 1, "{out}");
        assert_eq!(t.track(Track::Mic).clocks[0].skew_ppm, 0.0);

        // Too short a span: nominal rate kept, estimate still reported.
        let t = Timeline::plan(&dir, &m, &TimelineOptions::default()).unwrap();
        let clock = t.track(Track::System).clocks[0];
        assert_eq!(clock.skew_ppm, 0.0);
        assert!(clock.estimated_ppm.is_some());
        assert_eq!(t.track(Track::System).end_frame, 12_000);
    }

    #[test]
    fn pauses_are_removed_from_the_timeline() {
        let root = tempfile::tempdir().unwrap();
        let mut w = journal(root.path());
        w.begin_segment(Track::Mic, 0, None, 1000, 0).unwrap();
        w.append(Track::Mic, 0, &tone(1000, 0.1)).unwrap();
        w.pause(1_000_000_000).unwrap();
        // Captured while paused: never journaled.
        assert!(
            !w.append(Track::Mic, 1_000_000_000, &tone(500, 0.9))
                .unwrap()
        );
        w.resume(5_000_000_000).unwrap();
        w.append(Track::Mic, 5_000_000_000, &tone(1000, 0.2))
            .unwrap();
        w.bookmark(5_500_000_000, Some("decision".into())).unwrap();
        let dir = w.dir().to_path_buf();
        let m = w.finish(6_000_000_000, "user").unwrap();

        let t = Timeline::plan(&dir, &m, &TimelineOptions::default()).unwrap();
        assert_eq!(t.frames, 2000, "the 4 s pause takes no time");
        let mic = t.read_track(Track::Mic).unwrap();
        assert!(mic[..1000].iter().all(|s| *s == 0.1));
        assert!(mic[1000..].iter().all(|s| *s == 0.2));
        // Bookmarks map through the same time map.
        assert!((t.time_map.recording_s(m.bookmarks[0].at_ns) - 1.5).abs() < 1e-9);
    }

    #[test]
    fn cutoff_reads_a_prefix_and_the_rest_arrives_once() {
        let root = tempfile::tempdir().unwrap();
        let mut w = journal(root.path());
        w.begin_segment(Track::Mic, 0, None, 1000, 0).unwrap();
        w.begin_segment(Track::System, 0, None, 1000, 0).unwrap();
        let ramp = |from: usize, n: usize| (from..from + n).map(|i| i as f32).collect::<Vec<_>>();
        w.append(Track::Mic, 0, &ramp(0, 2500)).unwrap();
        w.append(Track::System, 0, &ramp(0, 2500)).unwrap();
        // Process now: snapshot while capture continues.
        let cutoff = w.cutoff();
        assert_eq!(
            cutoff,
            Cutoff {
                mic: Some(1),
                system: Some(1)
            }
        );
        let dir = w.dir().to_path_buf();
        let during = load_manifest(&dir).unwrap();
        let options = TimelineOptions {
            cutoff: Some(cutoff),
            ..TimelineOptions::default()
        };
        let partial = Timeline::plan(&dir, &during, &options).unwrap();
        assert_eq!(partial.frames, 2000);
        assert_eq!(partial.read_track(Track::Mic).unwrap(), ramp(0, 2000));

        w.append(Track::Mic, 2_500_000_000, &ramp(2500, 1500))
            .unwrap();
        w.append(Track::System, 2_500_000_000, &ramp(2500, 1500))
            .unwrap();
        let m = w.finish(4_000_000_000, "user").unwrap();
        let full = Timeline::plan(&dir, &m, &TimelineOptions::default()).unwrap();
        // Every sample exactly once, in order.
        assert_eq!(full.read_track(Track::Mic).unwrap(), ramp(0, 4000));
        assert_eq!(full.read_track(Track::System).unwrap(), ramp(0, 4000));
    }

    #[test]
    fn segments_resample_to_one_rate() {
        let root = tempfile::tempdir().unwrap();
        let mut w = journal(root.path());
        w.begin_segment(Track::Mic, 0, Some("A".into()), 2000, 0)
            .unwrap();
        w.append(Track::Mic, 0, &tone(2000, 0.5)).unwrap();
        // Device switch to a 1 kHz device.
        w.begin_segment(Track::Mic, 1, Some("B".into()), 1000, 1_000_000_000)
            .unwrap();
        w.append(Track::Mic, 1_000_000_000, &tone(1000, 0.5))
            .unwrap();
        let dir = w.dir().to_path_buf();
        let m = w.finish(2_000_000_000, "user").unwrap();
        let t = Timeline::plan(&dir, &m, &TimelineOptions::default()).unwrap();
        assert_eq!(t.sample_rate, 2000);
        assert_eq!(t.frames, 4000);
        assert_eq!(t.track(Track::Mic).runs.len(), 2);
        let mic = t.read_track(Track::Mic).unwrap();
        // DC passes through the resampler away from the edges.
        assert!(mic[2100..3900].iter().all(|s| (s - 0.5).abs() < 0.01));
    }

    /// Runs at one rate share a resampler, and each still starts afresh at
    /// its own anchor: exactly what resampling each run alone gives.
    #[test]
    fn same_rate_runs_resample_independently() {
        let root = tempfile::tempdir().unwrap();
        let mut w = journal(root.path());
        w.begin_segment(Track::Mic, 0, None, 2000, 0).unwrap();
        let wave = |n: usize, f: f32| (0..n).map(|i| (i as f32 * f).sin()).collect::<Vec<_>>();
        let first = wave(3000, 0.01);
        let second = wave(2000, 0.03);
        w.append(Track::Mic, 0, &first).unwrap();
        // 1.5 s later than the first run ends: a second run of the segment.
        w.append(Track::Mic, 3_000_000_000, &second).unwrap();
        let dir = w.dir().to_path_buf();
        let m = w.finish(4_000_000_000, "user").unwrap();
        let options = TimelineOptions {
            sample_rate: Some(1000),
            ..TimelineOptions::default()
        };
        let t = Timeline::plan(&dir, &m, &options).unwrap();
        assert_eq!(t.track(Track::Mic).runs.len(), 2);
        let mut want = crate::resample::resample(&first, 2000.0, 1000.0);
        want.resize(3000, 0.0);
        want.extend(crate::resample::resample(&second, 2000.0, 1000.0));
        assert_eq!(t.read_track(Track::Mic).unwrap(), want);
    }
}
