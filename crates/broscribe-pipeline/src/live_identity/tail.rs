//! Bounded reader of a live session's closed journal chunks. Each chunk is
//! resampled whole and placed exactly as the live ASR feed sends it
//! (`broscribe_capture::anchors`), so speaker spans and ASR words share one
//! clock and one journal anchor rule. The resampler (kernel table and
//! buffers) and its output buffer are reused across chunks of one rate.

use std::path::Path;

use broscribe_capture::anchors::{Placement, journal_point};
use broscribe_capture::journal::{ChunkEntry, Track as CaptureTrack, read_chunk};
use broscribe_capture::resample::Resampler;
use broscribe_core::live_identity::JournalPoint;
use broscribe_core::{Error, Result};

use super::RATE;

/// What one [`TrackTail::poll`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Polled {
    /// Live samples added.
    pub appended: u64,
    /// Closed chunks whose file could not be found.
    pub missing: usize,
}

/// The last `cap` live samples of one track's closed chunks.
///
/// The retained samples are `buf[head..]`: trimming only moves `head`, and
/// the dropped prefix is compacted away once it reaches the cap, so each
/// sample is moved at most once per cap samples read (no front drain per
/// chunk) and slices stay contiguous. Live sample `start` is `buf[head]`.
pub struct TrackTail {
    track: CaptureTrack,
    cap: usize,
    /// Last chunk consumed (read or skipped).
    consumed: Option<u64>,
    /// Live sample of `buf[head]`.
    start: u64,
    buf: Vec<f32>,
    /// Index in `buf` of the first retained sample.
    head: usize,
    /// Placements of the chunks overlapping the retained samples.
    placed: Vec<Placement>,
    /// The resampler of the last read chunk's device rate, rebuilt only
    /// when the rate changes; [`Resampler::process`] gives each chunk
    /// exactly what a fresh whole-buffer resample would, so lengths and
    /// anchors are unchanged by the reuse.
    resampler: Option<(u32, Resampler)>,
    /// The last chunk's 16 kHz audio (capacity reused).
    resampled: Vec<f32>,
}

impl TrackTail {
    pub fn new(track: CaptureTrack, cap_samples: usize) -> Self {
        Self {
            track,
            cap: cap_samples.max(1),
            consumed: None,
            start: 0,
            buf: Vec::new(),
            head: 0,
            placed: Vec::new(),
            resampler: None,
            resampled: Vec::new(),
        }
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn end(&self) -> u64 {
        self.start + self.retained() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.retained() == 0
    }

    /// Samples retained now (never more than the cap).
    pub fn retained(&self) -> usize {
        self.buf.len() - self.head
    }

    /// Whether every chunk in `all` was consumed.
    pub fn caught_up(&self, all: &[Placement]) -> bool {
        all.last()
            .is_none_or(|p| self.consumed.is_some_and(|c| c >= p.seq))
    }

    /// `[from, to)` if fully retained.
    pub fn slice(&self, from: u64, to: u64) -> Option<&[f32]> {
        if from < self.start || to > self.end() || to <= from {
            return None;
        }
        let a = self.head + (from - self.start) as usize;
        let b = self.head + (to - self.start) as usize;
        Some(&self.buf[a..b])
    }

    /// The journal position of a retained live sample.
    pub fn point(&self, sample: u64) -> Option<JournalPoint> {
        journal_point(&self.placed, sample)
    }

    /// Reads the chunks of `all` not consumed yet, in order. Chunks ending
    /// at or before `skip_before` are skipped unread (the caller reports
    /// that backlog as a gap); a missing chunk file is counted and breaks
    /// continuity.
    pub fn poll(&mut self, dir: &Path, all: &[Placement], skip_before: u64) -> Result<Polled> {
        let mut polled = Polled::default();
        for p in all {
            if self.consumed.is_some_and(|consumed| p.seq <= consumed) {
                continue;
            }
            self.consumed = Some(p.seq);
            if p.len == 0 || p.end() <= skip_before {
                continue;
            }
            let entry = ChunkEntry {
                seq: p.seq,
                segment: p.segment,
                host_start_ns: 0,
                frames: Some(p.frames),
                after_gap: false,
                recovered: false,
            };
            let samples = match read_chunk(dir, self.track, &entry) {
                Ok(s) => s,
                Err(Error::Io { .. } | Error::NotFound(_)) => {
                    polled.missing += 1;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if self
                .resampler
                .as_ref()
                .is_some_and(|(rate, _)| *rate != p.rate)
            {
                self.resampler = None;
            }
            let (_, resampler) = self.resampler.get_or_insert_with(|| {
                (p.rate, Resampler::new(f64::from(p.rate), f64::from(RATE)))
            });
            let mut audio = std::mem::take(&mut self.resampled);
            resampler.process(&samples, &mut audio);
            polled.appended += self.place(p.start, &audio);
            self.resampled = audio;
            self.placed.push(*p);
            self.trim();
        }
        Ok(polled)
    }

    /// Writes `samples` at live sample `at`: a hole before them is
    /// zero-filled (a capture gap) unless longer than the cap, which starts
    /// the buffer anew; samples before the current end (host-time jitter)
    /// are dropped.
    fn place(&mut self, at: u64, samples: &[f32]) -> u64 {
        if self.is_empty() {
            self.reset(at);
        }
        let end = self.end();
        let skip = if at >= end {
            let hole = at - end;
            if hole > self.cap as u64 {
                self.reset(at);
            } else {
                self.buf.resize(self.buf.len() + hole as usize, 0.0);
            }
            0
        } else {
            (end - at) as usize
        };
        let fresh = samples.get(skip..).unwrap_or(&[]);
        self.buf.extend_from_slice(fresh);
        fresh.len() as u64
    }

    /// Empties the buffer, its next sample being live sample `at`.
    fn reset(&mut self, at: u64) {
        self.buf.clear();
        self.head = 0;
        self.placed.clear();
        self.start = at;
    }

    /// Keeps the last `cap` samples. The dropped prefix stays in `buf`
    /// until it reaches the cap, then goes in one move.
    fn trim(&mut self) {
        let retained = self.retained();
        if retained <= self.cap {
            return;
        }
        let drop = retained - self.cap;
        self.head += drop;
        self.start += drop as u64;
        let start = self.start;
        self.placed.retain(|p| p.end() > start);
        if self.head >= self.cap {
            self.buf.drain(..self.head);
            self.head = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live sample `i` holds the value `i`, so any slice shows exactly
    /// which live samples it covers.
    fn ramp(from: u64, len: usize) -> Vec<f32> {
        (0..len).map(|i| (from + i as u64) as f32).collect()
    }

    #[test]
    fn record_live_tail_ring_keeps_the_exact_sample_map_across_compactions() {
        let cap = 1_000;
        let mut tail = TrackTail::new(CaptureTrack::System, cap);
        let mut at = 0u64;
        // Uneven chunk sizes, many caps' worth: trims and compactions at
        // every phase of the buffer.
        for (i, len) in [137usize, 400, 999, 1, 1_000, 2_345, 64, 777]
            .iter()
            .cycle()
            .take(64)
            .enumerate()
        {
            // Some jitter: a chunk overlapping the end by a few samples.
            let start = if i % 5 == 4 { at.saturating_sub(3) } else { at };
            let audio = ramp(start, *len + (at - start) as usize);
            tail.place(start, &audio);
            tail.trim();
            at += *len as u64;
            assert_eq!(tail.end(), at);
            assert!(tail.retained() <= cap);
            assert_eq!(tail.retained(), cap.min(at as usize));
            assert!(tail.buf.len() <= 2 * cap + 2_345, "bounded storage");
            let from = tail.start();
            assert_eq!(
                tail.slice(from, at).unwrap(),
                &ramp(from, (at - from) as usize)[..]
            );
            assert!(tail.slice(from.saturating_sub(1), at).is_none() || from == 0);
        }
        // A hole within the cap is zero-filled at its exact place; one
        // beyond it starts the buffer anew.
        let gap_at = at + 10;
        tail.place(gap_at, &ramp(gap_at, 5));
        tail.trim();
        let s = tail.slice(at, gap_at + 5).unwrap();
        assert!(s[..10].iter().all(|x| *x == 0.0));
        assert_eq!(&s[10..], &ramp(gap_at, 5)[..]);
        let far = gap_at + 5 + cap as u64 + 1;
        tail.place(far, &ramp(far, 7));
        tail.trim();
        assert_eq!(tail.start(), far);
        assert_eq!(tail.slice(far, far + 7).unwrap(), &ramp(far, 7)[..]);
    }

    /// RT19: chunks of two device rates go through one reused resampler
    /// per rate, and every chunk lands exactly where, and as, a fresh
    /// whole-buffer resample of it would (same lengths, same anchors).
    #[test]
    fn record_live_tail_reused_resampler_matches_whole_chunk_resampling() {
        use broscribe_capture::anchors::placements;
        use broscribe_capture::journal::{JournalWriter, SessionMeta};
        use broscribe_capture::resample::resample;

        let tone = |rate: u32, freq: f64| -> Vec<f32> {
            (0..rate as usize)
                .map(|i| {
                    (0.3 * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate)).sin())
                        as f32
                })
                .collect()
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut journal = JournalWriter::create(
            tmp.path(),
            "tail-s1",
            SessionMeta::default(),
            1.0,
            std::process::id(),
        )
        .unwrap();
        let dir = journal.dir().to_owned();
        let track = CaptureTrack::System;
        journal.begin_segment(track, 0, None, 48_000, 0).unwrap();
        for s in 0..3u64 {
            journal
                .append(track, s * 1_000_000_000, &tone(48_000, 300.0))
                .unwrap();
        }
        // A device change to another rate.
        journal
            .begin_segment(track, 1, None, 44_100, 3_000_000_000)
            .unwrap();
        for s in 3..6u64 {
            journal
                .append(track, s * 1_000_000_000, &tone(44_100, 500.0))
                .unwrap();
        }
        let manifest = journal.finish(6_000_000_000, "user").unwrap();
        let all = placements(&manifest, track);
        assert!(all.iter().any(|p| p.rate == 48_000) && all.iter().any(|p| p.rate == 44_100));
        let mut tail = TrackTail::new(track, RATE as usize * 60);
        tail.poll(&dir, &all, 0).unwrap();
        assert_eq!(
            tail.resampler.as_ref().map(|(rate, _)| *rate),
            Some(44_100),
            "rebuilt only for the new rate"
        );
        let mut end = all[0].start;
        for p in &all {
            let entry = ChunkEntry {
                seq: p.seq,
                segment: p.segment,
                host_start_ns: 0,
                frames: Some(p.frames),
                after_gap: false,
                recovered: false,
            };
            let device = read_chunk(&dir, track, &entry).unwrap();
            let fresh = resample(&device, f64::from(p.rate), f64::from(RATE));
            assert_eq!(fresh.len() as u64, p.len, "anchored length");
            // Samples before the end so far were dropped as jitter.
            let from = p.start.max(end);
            if p.end() > from {
                let skip = (from - p.start) as usize;
                assert_eq!(tail.slice(from, p.end()).unwrap(), &fresh[skip..]);
            }
            end = end.max(p.end());
        }
        assert_eq!(tail.end(), end);
    }
}
