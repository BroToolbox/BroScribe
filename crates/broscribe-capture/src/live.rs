//! Live-preview feed (plan M4 §13.1, §14.1): filled by the journal writer
//! after each block is journaled, drained by the daemon's sender thread.
//!
//! Each track has a bounded queue measured in seconds of audio. Pushing
//! never waits: when a track's queue is full the oldest audio is dropped and
//! replaced by a [`LiveItem::Gap`] carrying the dropped recording-time
//! range, so the preview can show it. Nothing here can stall capture.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::journal::Track;

#[derive(Clone, Debug, PartialEq)]
pub struct LiveAudio {
    pub track: Track,
    /// Recording time of the first sample (pauses removed).
    pub start_s: f64,
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

impl LiveAudio {
    pub fn duration_s(&self) -> f64 {
        self.samples.len() as f64 / f64::from(self.sample_rate.max(1))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LiveItem {
    Audio(LiveAudio),
    /// Audio of `track` between `from_s` and `to_s` (recording time) was
    /// dropped because the consumer fell behind.
    Gap {
        track: Track,
        from_s: f64,
        to_s: f64,
    },
}

#[derive(Default)]
struct TrackQueue {
    items: VecDeque<LiveItem>,
    /// Seconds of audio queued.
    seconds: f64,
}

/// Bounded drop-oldest queue per track.
pub struct LiveFeed {
    capacity_s: f64,
    queues: Mutex<[TrackQueue; 2]>,
    ready: Condvar,
}

impl LiveFeed {
    /// Keeps at most `capacity_s` seconds of audio per track.
    pub fn new(capacity_s: f64) -> Self {
        Self {
            capacity_s: capacity_s.max(0.1),
            queues: Mutex::new([TrackQueue::default(), TrackQueue::default()]),
            ready: Condvar::new(),
        }
    }

    /// Queues a journaled block; drops the oldest audio when full.
    pub fn push(&self, audio: LiveAudio) {
        let mut queues = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        let q = &mut queues[audio.track.index()];
        q.seconds += audio.duration_s();
        q.items.push_back(LiveItem::Audio(audio));
        while q.seconds > self.capacity_s {
            // The oldest audio item, possibly behind a leading gap.
            let Some(pos) = q.items.iter().position(|i| matches!(i, LiveItem::Audio(_))) else {
                break;
            };
            let Some(LiveItem::Audio(old)) = q.items.remove(pos) else {
                break;
            };
            q.seconds -= old.duration_s();
            let (from, to) = (old.start_s, old.start_s + old.duration_s());
            // Merge with a gap right before it, else insert one in its place.
            match pos.checked_sub(1).and_then(|p| q.items.get_mut(p)) {
                Some(LiveItem::Gap { to_s, .. }) => *to_s = to_s.max(to),
                _ => q.items.insert(
                    pos,
                    LiveItem::Gap {
                        track: old.track,
                        from_s: from,
                        to_s: to,
                    },
                ),
            }
        }
        drop(queues);
        self.ready.notify_all();
    }

    /// The oldest item of either track (mic first on ties), waiting up to
    /// `timeout` for one.
    pub fn pop_timeout(&self, timeout: Duration) -> Option<LiveItem> {
        let mut queues = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        if queues.iter().all(|q| q.items.is_empty()) {
            queues = self
                .ready
                .wait_timeout_while(queues, timeout, |qs| qs.iter().all(|q| q.items.is_empty()))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        let start = |q: &TrackQueue| {
            q.items.front().map(|i| match i {
                LiveItem::Audio(a) => a.start_s,
                LiveItem::Gap { from_s, .. } => *from_s,
            })
        };
        let pick = match (start(&queues[0]), start(&queues[1])) {
            (Some(a), Some(b)) => usize::from(b < a),
            (Some(_), None) => 0,
            (None, Some(_)) => 1,
            (None, None) => return None,
        };
        let q = &mut queues[pick];
        let item = q.items.pop_front()?;
        if let LiveItem::Audio(a) = &item {
            q.seconds -= a.duration_s();
        }
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(track: Track, start_s: f64) -> LiveAudio {
        LiveAudio {
            track,
            start_s,
            sample_rate: 10,
            samples: vec![0.0; 10],
        }
    }

    #[test]
    fn full_queue_drops_oldest_and_reports_the_range() {
        let feed = LiveFeed::new(3.0);
        for i in 0..6 {
            feed.push(block(Track::Mic, f64::from(i)));
        }
        assert!(feed.queues.lock().unwrap()[0].seconds <= 3.0 + 1e-9);
        let first = feed.pop_timeout(Duration::ZERO).unwrap();
        assert_eq!(
            first,
            LiveItem::Gap {
                track: Track::Mic,
                from_s: 0.0,
                to_s: 3.0
            }
        );
        let starts: Vec<f64> = std::iter::from_fn(|| feed.pop_timeout(Duration::ZERO))
            .map(|i| match i {
                LiveItem::Audio(a) => a.start_s,
                LiveItem::Gap { .. } => -1.0,
            })
            .collect();
        assert_eq!(starts, vec![3.0, 4.0, 5.0]);
    }

    #[test]
    fn interleaves_tracks_by_time_and_times_out() {
        let feed = LiveFeed::new(10.0);
        feed.push(block(Track::System, 0.5));
        feed.push(block(Track::Mic, 1.0));
        feed.push(block(Track::Mic, 0.0));
        let order: Vec<(Track, f64)> = std::iter::from_fn(|| feed.pop_timeout(Duration::ZERO))
            .map(|i| match i {
                LiveItem::Audio(a) => (a.track, a.start_s),
                LiveItem::Gap { .. } => unreachable!(),
            })
            .collect();
        // Per-track order is kept; across tracks the earlier head goes first.
        assert_eq!(
            order,
            vec![(Track::System, 0.5), (Track::Mic, 1.0), (Track::Mic, 0.0)]
        );
        assert!(feed.pop_timeout(Duration::from_millis(5)).is_none());
    }

    #[test]
    fn memory_stays_bounded_without_a_consumer() {
        let feed = LiveFeed::new(2.0);
        for i in 0..10_000 {
            feed.push(block(Track::System, f64::from(i)));
        }
        let queues = feed.queues.lock().unwrap();
        // Two seconds of audio plus one merged gap.
        assert!(queues[1].items.len() <= 3, "{}", queues[1].items.len());
    }
}
