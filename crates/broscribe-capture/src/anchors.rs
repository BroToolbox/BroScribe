//! Live-clock placement of closed journal chunks (plan D): where each
//! durable chunk lies in recording time at the live preview's 16 kHz, and
//! the journal position (chunk, frame) of a live sample.
//!
//! The rule is the live ASR feed's own: the capture preview sends each
//! closed chunk as one frame starting at `round(recording_s(host start) ·
//! 16 kHz)` ([`live_start_sample`]) holding the chunk resampled whole
//! ([`live_len`] samples). The live-speakers worker reads and places chunks
//! the same way and the daemon anchors decisions with the same placements,
//! so an ASR word time, a diarized span and a decision's journal anchor all
//! name the same journal frame (up to one sample of rounding). The final
//! timeline then maps those journal frames itself (skew included).

use broscribe_core::live_identity::{JournalPoint, LIVE_SAMPLE_RATE};

use crate::journal::{SessionManifest, Track};

/// Where one closed chunk lies on the live clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub seq: u64,
    pub segment: u32,
    /// Live sample of the chunk's first frame.
    pub start: u64,
    /// Live samples it spans.
    pub len: u64,
    /// Frames in the chunk file (device rate).
    pub frames: u64,
    pub rate: u32,
}

impl Placement {
    pub fn end(&self) -> u64 {
        self.start + self.len
    }
}

/// The live sample a preview frame starting at recording time
/// `recording_s` starts at.
pub fn live_start_sample(recording_s: f64) -> u64 {
    (recording_s * f64::from(LIVE_SAMPLE_RATE)).round() as u64
}

/// Live samples `frames` at `rate` resample to (the whole-buffer
/// resampler's output length).
pub fn live_len(frames: u64, rate: u32) -> u64 {
    let step = f64::from(rate.max(1)) / f64::from(LIVE_SAMPLE_RATE);
    (frames as f64 / step).round() as u64
}

/// The closed chunks of `track`, in journal order: a chunk is durable only
/// once it has a frame count, and every later chunk of the track is still
/// open, so the list stops at the first open one.
pub fn placements(manifest: &SessionManifest, track: Track) -> Vec<Placement> {
    let journal = manifest.track(track);
    let time_map = manifest.time_map();
    let mut out: Vec<Placement> = Vec::new();
    for chunk in &journal.chunks {
        let Some(frames) = chunk.frames else {
            break;
        };
        let Some(rate) = journal
            .segments
            .iter()
            .find(|s| s.index == chunk.segment)
            .map(|s| s.sample_rate)
            .filter(|r| *r > 0)
        else {
            continue;
        };
        out.push(Placement {
            seq: chunk.seq,
            segment: chunk.segment,
            start: live_start_sample(time_map.recording_s(chunk.host_start_ns)),
            len: live_len(frames, rate),
            frames,
            rate,
        });
    }
    out
}

/// The journal position of live sample `sample`: the latest placement that
/// holds it (host-time jitter can make neighbours overlap by a sample), or
/// whose end it is.
pub fn journal_point(placements: &[Placement], sample: u64) -> Option<JournalPoint> {
    let p = placements
        .iter()
        .rev()
        .find(|p| p.start <= sample && sample < p.end())
        .or_else(|| placements.iter().rev().find(|p| p.end() == sample))?;
    let step = f64::from(p.rate) / f64::from(LIVE_SAMPLE_RATE);
    let frame = ((sample - p.start) as f64 * step).round() as u64;
    Some(JournalPoint {
        seq: p.seq,
        frame: frame.min(p.frames),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{JournalWriter, SessionMeta, Track};

    #[test]
    fn record_live_placements_follow_closed_chunks_only() {
        let tmp = tempfile::tempdir().unwrap();
        let mut journal =
            JournalWriter::create(tmp.path(), "anchors", SessionMeta::default(), 1.0, 1).unwrap();
        journal
            .begin_segment(Track::System, 0, None, 48_000, 0)
            .unwrap();
        // 2.5 s at 48 kHz: two closed 1 s chunks and an open one.
        let block = vec![0.1f32; 48_000];
        journal.append(Track::System, 0, &block).unwrap();
        journal
            .append(Track::System, 1_000_000_000, &block)
            .unwrap();
        journal
            .append(Track::System, 2_000_000_000, &block[..24_000])
            .unwrap();
        let manifest = crate::journal::load_manifest(journal.dir()).unwrap();
        let placed = placements(&manifest, Track::System);
        assert!(placed.iter().all(|p| p.rate == 48_000));
        let closed = manifest
            .system
            .chunks
            .iter()
            .filter(|c| c.frames.is_some())
            .count();
        assert_eq!(placed.len(), closed);
        assert_eq!(placed[0].start, 0);
        assert_eq!(placed[0].len, 16_000);
        let point = journal_point(&placed, 20_000).unwrap();
        assert_eq!(
            point,
            JournalPoint {
                seq: placed[1].seq,
                frame: 12_000
            }
        );
        assert!(placements(&manifest, Track::Mic).is_empty());
    }
}
