//! One-producer/one-consumer preallocated PCM packets. Realtime callbacks
//! (PipeWire monitors, the CPAL microphone) only write atomics; alignment,
//! mixing and timestamping happen on the delivery worker. Packet
//! publication covers its samples as well.
//!
//! Every delivered chunk carries `host_time`: the [`Instant`] of its first
//! sample, taken from the audio clock rather than from when the worker got
//! to it, so holdback, delivery stalls and pause boundaries never move audio
//! on the recorder's timeline.
//!
//! System audio: sample positions come from sample counts and each stream's
//! own graph-clock ticks, never from the delivery worker's clock. PipeWire
//! rebases every stream's ticks to that stream's start, so ticks are only
//! compared within one stream. A stream's first packet (and a packet whose
//! ticks went backwards) is placed on the session timeline through its
//! cycle time (`pw_time.now`, the driver's `CLOCK_MONOTONIC` cycle time,
//! shared by every stream of the graph): the clock stream (the default-sink
//! monitor) starts the session; any other stream joins at its own cycle,
//! relative to the latest clock packet. Later packets follow the stream's
//! own ticks, keeping sample continuity while the two agree within
//! [`ALIGN_TOLERANCE`]. Audio is never moved later to dodge frames already
//! delivered: such audio fails the capture instead.
use broscribe_platform::{Error, MicChunk, MicSink, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const PACKETS: usize = 1024;
pub(crate) const RATE: u32 = 48_000;
/// Sample capacity of a recorded system stream: two seconds.
pub(crate) const CAPACITY: usize = RATE as usize * 2;
/// Largest difference (in samples) between a stream's continuous position
/// and its tick-mapped one that is treated as rounding, not as a gap.
const ALIGN_TOLERANCE: i64 = 32;
const NO_FRAME: i64 = i64::MIN;
const NANOS: i128 = 1_000_000_000;

/// Maps one audio clock (nanoseconds) onto [`Instant`] through a pair of
/// readings taken together.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClockAnchor {
    ns: i128,
    instant: Instant,
}
impl ClockAnchor {
    pub fn new(ns: i128, instant: Instant) -> Self {
        Self { ns, instant }
    }
    /// `CLOCK_MONOTONIC`, PipeWire's graph time base. Rust's [`Instant`] is
    /// the same clock on Linux, so this anchor is exact up to the few
    /// nanoseconds between the two readings.
    pub fn monotonic() -> Result<Self> {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let instant = Instant::now();
        // SAFETY: `now` is a valid, writable timespec.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let ns = i128::from(now.tv_sec) * NANOS + i128::from(now.tv_nsec);
        Ok(Self::new(ns, instant))
    }
    /// The instant of clock reading `ns`; `None` beyond `Instant`'s range.
    pub fn instant_at(&self, ns: i128) -> Option<Instant> {
        let delta = ns - self.ns;
        let magnitude = Duration::from_nanos(u64::try_from(delta.unsigned_abs()).ok()?);
        if delta >= 0 {
            self.instant.checked_add(magnitude)
        } else {
            self.instant.checked_sub(magnitude)
        }
    }
}

struct Packet {
    /// System streams: graph-clock ticks of the packet's cycle, scaled to
    /// [`RATE`]. Unused (0) for the microphone.
    ticks: AtomicU64,
    /// Clock time of the packet (ns): the graph cycle's driver time for
    /// PipeWire monitors, CPAL's capture instant for the microphone.
    time_ns: AtomicU64,
    len: AtomicUsize,
}
pub(crate) struct TimedRing {
    /// Empty for a metadata-only ring (the inaudible clock monitor).
    samples: Box<[AtomicU32]>,
    packets: Box<[Packet]>,
    sample_write: AtomicUsize,
    sample_read: AtomicUsize,
    packet_write: AtomicUsize,
    packet_read: AtomicUsize,
    dropped: AtomicU64,
    /// Session frame after this stream's last placed packet (worker-only).
    next_frame: AtomicI64,
    /// Scaled ticks and session frame of this stream's placement origin
    /// (worker-only; `NO_FRAME`: not placed yet).
    origin_ticks: AtomicU64,
    origin_frame: AtomicI64,
}
impl TimedRing {
    /// `capacity` samples of storage; 0 keeps only clock/length metadata,
    /// so excluded audio is never copied out of PipeWire's buffer, even into
    /// our sample storage, and the clock monitor allocates no PCM storage.
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            packets: (0..PACKETS)
                .map(|_| Packet {
                    ticks: AtomicU64::new(0),
                    time_ns: AtomicU64::new(0),
                    len: AtomicUsize::new(0),
                })
                .collect(),
            sample_write: AtomicUsize::new(0),
            sample_read: AtomicUsize::new(0),
            packet_write: AtomicUsize::new(0),
            packet_read: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            next_frame: AtomicI64::new(NO_FRAME),
            origin_ticks: AtomicU64::new(0),
            origin_frame: AtomicI64::new(NO_FRAME),
        }
    }
    fn records(&self) -> bool {
        !self.samples.is_empty()
    }
    /// Producer side (one realtime thread): `n` samples produced by
    /// `sample(i)`, stamped with `ticks` (see [`scaled_ticks`]) and the
    /// packet's clock time `time_ns`. A full ring drops and counts them.
    pub fn push(&self, ticks: u64, time_ns: u64, n: usize, sample: impl Fn(usize) -> f32) {
        let capacity = self.samples.len();
        let sw = self.sample_write.load(Ordering::Relaxed);
        let sr = self.sample_read.load(Ordering::Acquire);
        let pw = self.packet_write.load(Ordering::Relaxed);
        let pr = self.packet_read.load(Ordering::Acquire);
        let samples_full = self.records() && n > capacity - sw.wrapping_sub(sr).min(capacity);
        if samples_full || pw.wrapping_sub(pr) >= PACKETS {
            self.dropped.fetch_add(n as u64, Ordering::Relaxed);
            return;
        }
        if self.records() {
            for i in 0..n {
                self.samples[sw.wrapping_add(i) % capacity]
                    .store(sample(i).to_bits(), Ordering::Relaxed);
            }
        }
        let packet = &self.packets[pw % PACKETS];
        packet.ticks.store(ticks, Ordering::Relaxed);
        packet.time_ns.store(time_ns, Ordering::Relaxed);
        packet.len.store(n, Ordering::Relaxed);
        self.sample_write
            .store(sw.wrapping_add(n), Ordering::Relaxed);
        self.packet_write
            .store(pw.wrapping_add(1), Ordering::Release);
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    fn sample(&self, index: usize) -> f32 {
        f32::from_bits(self.samples[index % self.samples.len()].load(Ordering::Relaxed))
    }
    /// Mixes every placeable packet into `mixer`. Packets of a non-clock
    /// stream wait in the ring until the clock stream has started.
    pub fn drain(&self, mixer: &mut Mixer, clock: bool) -> Result<()> {
        let mut packet_read = self.packet_read.load(Ordering::Relaxed);
        let packet_write = self.packet_write.load(Ordering::Acquire);
        let mut sample_read = self.sample_read.load(Ordering::Relaxed);
        while packet_read != packet_write {
            let packet = &self.packets[packet_read % PACKETS];
            let ticks = packet.ticks.load(Ordering::Relaxed);
            let time_ns = packet.time_ns.load(Ordering::Relaxed);
            let n = packet.len.load(Ordering::Relaxed);
            let Some(start) = mixer.position(self, ticks, time_ns, n, clock) else {
                break;
            };
            mixer.add(self, sample_read, start, n, self.records(), clock)?;
            sample_read = sample_read.wrapping_add(n);
            packet_read = packet_read.wrapping_add(1);
        }
        self.sample_read.store(sample_read, Ordering::Release);
        self.packet_read.store(packet_read, Ordering::Release);
        Ok(())
    }
    /// Fails when packets are still waiting for a clock that never started
    /// (after the last drain of a stream).
    pub fn ensure_drained(&self) -> Result<()> {
        if self.packet_read.load(Ordering::Relaxed) != self.packet_write.load(Ordering::Acquire) {
            return Err(crate::platform_error(
                "aligning PipeWire streams",
                "application audio arrived before the graph clock started",
            ));
        }
        Ok(())
    }
    /// Consumer side of a single-stream (microphone) ring: appends the next
    /// packet's samples to `out` and returns its clock time and length.
    fn pop_into(&self, out: &mut Vec<f32>) -> Option<(u64, usize)> {
        let packet_read = self.packet_read.load(Ordering::Relaxed);
        if packet_read == self.packet_write.load(Ordering::Acquire) {
            return None;
        }
        let packet = &self.packets[packet_read % PACKETS];
        let time_ns = packet.time_ns.load(Ordering::Relaxed);
        let n = packet.len.load(Ordering::Relaxed);
        let sample_read = self.sample_read.load(Ordering::Relaxed);
        if self.records() {
            out.extend((0..n).map(|i| self.sample(sample_read.wrapping_add(i))));
        }
        self.sample_read
            .store(sample_read.wrapping_add(n), Ordering::Release);
        self.packet_read
            .store(packet_read.wrapping_add(1), Ordering::Release);
        Some((time_ns, n))
    }
}

/// Delivers a single-stream ring as chunks stamped with their first
/// sample's instant (`clock` maps the packets' clock onto [`Instant`]).
/// Packets continuing the previous one within [`ALIGN_TOLERANCE`] samples
/// join its chunk; a later packet (a stream gap) starts a new chunk at its
/// own time, so the recorder places it there. A packet stamped earlier than
/// the previous one's end follows on: the capture clock never runs backwards
/// within one stream, only its latency estimate does.
pub(crate) fn deliver_timed(
    ring: &TimedRing,
    rate: u32,
    clock: ClockAnchor,
    sink: &mut MicSink,
) -> Result<()> {
    let rate_i = i128::from(rate.max(1));
    let tolerance_ns = i128::from(ALIGN_TOLERANCE) * NANOS / rate_i;
    let mut samples = Vec::new();
    let mut start: Option<u64> = None;
    let mut emit = |samples: Vec<f32>, start_ns: u64| -> Result<()> {
        let mut chunk = MicChunk::new(samples, rate);
        chunk.host_time = Some(clock.instant_at(i128::from(start_ns)).ok_or_else(|| {
            Error::Internal("microphone timestamp outside the monotonic clock".into())
        })?);
        sink(chunk);
        Ok(())
    };
    loop {
        let before = samples.len();
        let Some((time_ns, _)) = ring.pop_into(&mut samples) else {
            break;
        };
        match start {
            None => start = Some(time_ns),
            Some(first) => {
                let expected = i128::from(first) + before as i128 * NANOS / rate_i;
                if i128::from(time_ns) > expected + tolerance_ns {
                    let next = samples.split_off(before);
                    emit(std::mem::replace(&mut samples, next), first)?;
                    start = Some(time_ns);
                }
            }
        }
    }
    match start {
        Some(first) if !samples.is_empty() => emit(samples, first),
        _ => Ok(()),
    }
}

/// Graph-clock `ticks` at `num/denom` seconds per tick, scaled to [`RATE`]
/// (rounded); `None` for an invalid rate.
pub(crate) fn scaled_ticks(ticks: u64, num: u32, denom: u32) -> Option<u64> {
    if num == 0 || denom == 0 {
        return None;
    }
    let denom = denom as u128;
    u64::try_from((ticks as u128 * RATE as u128 * num as u128 + denom / 2) / denom).ok()
}

fn late() -> Error {
    crate::platform_error(
        "aligning PipeWire streams",
        "application audio arrived after its graph-clock deadline",
    )
}

pub(crate) struct Mixer {
    /// Maps the graph's driver time onto [`Instant`] for `host_time`.
    clock: ClockAnchor,
    /// Session frame of the next sample to deliver.
    cursor: Option<i64>,
    data: VecDeque<f32>,
    clock_end: i64,
    emitted: bool,
    /// Session frame and driver cycle time (ns) of the latest clock packet.
    clock_last: Option<(i64, u64)>,
    /// Driver cycle time (ns, ascending) and session frame of recent clock
    /// packets, preallocated ([`CLOCK_HISTORY`]).
    clock_cycles: VecDeque<(u64, i64)>,
}
/// Clock packets remembered for joining streams: longer than the holdback
/// and any placeable lateness at every PipeWire quantum.
const CLOCK_HISTORY: usize = 1024;
impl Mixer {
    pub fn new(clock: ClockAnchor) -> Self {
        Self {
            clock,
            cursor: None,
            data: VecDeque::new(),
            clock_end: 0,
            emitted: false,
            clock_last: None,
            clock_cycles: VecDeque::with_capacity(CLOCK_HISTORY),
        }
    }
    /// Session frame of driver cycle time `cycle_ns`. Every stream of the
    /// graph shares the driver's cycle times, so a cycle the clock also
    /// processed maps to the clock's own frame for it exactly; other times
    /// map from the nearest earlier clock cycle (or the oldest remembered),
    /// rounded to the nearest sample. Mapping from the *latest* clock packet
    /// instead would carry that packet's sub-sample phase into the result.
    fn frame_at(&self, cycle_ns: u64) -> Option<i64> {
        let later = self.clock_cycles.partition_point(|(ns, _)| *ns <= cycle_ns);
        let (clock_ns, clock_frame) = match later.checked_sub(1) {
            Some(index) => self.clock_cycles[index],
            None => *self.clock_cycles.front()?,
        };
        let offset = (i128::from(cycle_ns) - i128::from(clock_ns)) * i128::from(RATE);
        i64::try_from(i128::from(clock_frame) + (offset + NANOS / 2).div_euclid(NANOS)).ok()
    }
    /// Session frame of a packet's first sample (negative: before the
    /// session origin); `None` while a non-clock stream must wait for the
    /// clock.
    fn position(
        &mut self,
        ring: &TimedRing,
        ticks: u64,
        cycle_ns: u64,
        n: usize,
        clock: bool,
    ) -> Option<i64> {
        let expected = match ring.next_frame.load(Ordering::Relaxed) {
            NO_FRAME => None,
            frame => Some(frame),
        };
        let tick_mapped = match ring.origin_frame.load(Ordering::Relaxed) {
            NO_FRAME => None,
            origin => {
                let origin_ticks = ring.origin_ticks.load(Ordering::Relaxed);
                Some(origin + (ticks as i64).wrapping_sub(origin_ticks as i64))
            }
        };
        let rebase = |frame: i64| {
            ring.origin_ticks.store(ticks, Ordering::Relaxed);
            ring.origin_frame.store(frame, Ordering::Relaxed);
            frame
        };
        let frame = if clock {
            match (expected, tick_mapped) {
                (Some(expected), Some(mapped)) if mapped > expected + ALIGN_TOLERANCE => {
                    // A missed cycle moves the clock on by its ticks, so
                    // later packets of other streams never land on (and sum
                    // into) the quantum it skipped.
                    mapped
                }
                (Some(expected), Some(mapped)) if mapped + ALIGN_TOLERANCE >= expected => expected,
                // Ticks went backwards: the clock follows its sample count.
                (Some(expected), _) => rebase(expected),
                // The clock's first packet starts the session.
                (None, _) => rebase(0),
            }
        } else {
            match (expected, tick_mapped) {
                (Some(expected), Some(mapped))
                    if expected.abs_diff(mapped) <= ALIGN_TOLERANCE.unsigned_abs() =>
                {
                    expected
                }
                // A gap in the stream (paused playback) keeps its place.
                (Some(expected), Some(mapped)) if mapped > expected => mapped,
                // First packet, or ticks rebased backwards: join at the
                // packet's own cycle, never over the stream's own audio.
                _ => {
                    self.cursor?;
                    let joined = self.frame_at(cycle_ns)?;
                    rebase(expected.map_or(joined, |expected| joined.max(expected)))
                }
            }
        };
        ring.next_frame
            .store(frame.saturating_add(n as i64), Ordering::Relaxed);
        if clock {
            self.clock_last = Some((frame, cycle_ns));
            if self.clock_cycles.len() == CLOCK_HISTORY {
                self.clock_cycles.pop_front();
            }
            self.clock_cycles.push_back((cycle_ns, frame));
        }
        Some(frame)
    }
    fn add(
        &mut self,
        ring: &TimedRing,
        sample_read: usize,
        start: i64,
        n: usize,
        audible: bool,
        clock: bool,
    ) -> Result<()> {
        // Only the clock's first packet arrives before the session origin
        // exists (other streams wait in `position`).
        let cursor = *self.cursor.get_or_insert(start);
        let end = start.saturating_add(n as i64);
        if clock {
            self.clock_end = self.clock_end.max(end);
        }
        // Samples before the cursor were already delivered (or precede the
        // session): audible audio there after delivery began is late.
        let skip = usize::try_from(cursor.saturating_sub(start).clamp(0, n as i64)).unwrap_or(n);
        if audible && skip > 0 && self.emitted {
            return Err(late());
        }
        if skip == n {
            return Ok(());
        }
        let offset = usize::try_from(start + skip as i64 - cursor).unwrap_or(usize::MAX);
        let length = offset.saturating_add(n - skip);
        if length > CAPACITY {
            return Err(crate::platform_error(
                "aligning PipeWire streams",
                "graph clock jumped or delivery exceeded two seconds",
            ));
        }
        if self.data.len() < length {
            self.data.resize(length, 0.0);
        }
        if audible {
            for i in skip..n {
                self.data[offset + i - skip] += ring.sample(sample_read.wrapping_add(i));
            }
        }
        Ok(())
    }
    /// Delivers the mix up to the holdback before the clock's end, stamped
    /// with its first sample's instant on the graph clock.
    pub fn deliver(&mut self, sink: &mut MicSink, final_drain: bool) -> Result<()> {
        let Some(cursor) = self.cursor else {
            return Ok(());
        };
        // A short graph-clock holdback lets all per-app callbacks publish the
        // same cycle before it is mixed. No timestamps are based on arrival
        // time or the delivery worker's scheduling jitter.
        let holdback = if final_drain { 0 } else { i64::from(RATE / 20) };
        let end = (self.clock_end - holdback).max(cursor);
        let n = usize::try_from(end - cursor)
            .unwrap_or(0)
            .min(self.data.len());
        if n == 0 {
            return Ok(());
        }
        let (clock_frame, clock_ns) = self
            .clock_last
            .ok_or_else(|| Error::Internal("system audio mixed without a clock packet".into()))?;
        let start_ns = i128::from(clock_ns)
            + (i128::from(cursor) - i128::from(clock_frame)) * NANOS / i128::from(RATE);
        let host_time = self
            .clock
            .instant_at(start_ns)
            .ok_or_else(|| Error::Internal("graph time outside the monotonic clock".into()))?;
        let mut chunk = MicChunk::new(self.data.drain(..n).collect(), RATE);
        chunk.host_time = Some(host_time);
        self.cursor = Some(cursor + n as i64);
        self.emitted = true;
        sink(chunk);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A synthetic monotonic clock: `BASE_NS` is `anchor()`'s instant.
    const BASE_NS: i128 = 4_000_000_000;
    fn anchor() -> (ClockAnchor, Instant) {
        let at = Instant::now();
        (ClockAnchor::new(BASE_NS, at), at)
    }

    fn collecting() -> (MicSink, Arc<Mutex<Vec<MicChunk>>>) {
        let out = Arc::new(Mutex::new(Vec::<MicChunk>::new()));
        let sink_out = out.clone();
        let sink: MicSink = Box::new(move |chunk: MicChunk| sink_out.lock().unwrap().push(chunk));
        (sink, out)
    }
    fn samples(chunks: &[MicChunk]) -> Vec<f32> {
        chunks
            .iter()
            .flat_map(|c| c.samples_mono_f32.iter().copied())
            .collect()
    }

    /// A 44.1 kHz driver with a 512-tick quantum: the scaled ticks advance by
    /// 557.28 per cycle, while each stream's resampler hands over 557 or 558
    /// samples with its own phase, and the app's cycle ticks carry jitter.
    /// Positions follow sample counts: the clock track is gap-free and the
    /// app's audio lands once, contiguously, with no doubled samples.
    #[test]
    fn jittered_ticks_never_overlap_or_gap() {
        let (mut sink, out) = collecting();
        let clock = TimedRing::new(CAPACITY);
        let app = TimedRing::new(CAPACITY);
        let mut mixer = Mixer::new(anchor().0);
        let step = 512.0 * 48_000.0 / 44_100.0;
        let count = |c: u64, phase: f64| {
            ((c + 1) as f64 * step + phase).floor() as usize
                - (c as f64 * step + phase).floor() as usize
        };
        let jitter = [0i64, 2, -1, 1, -2, 0, 1];
        let (mut clock_total, mut app_total) = (0usize, 0usize);
        for c in 0..400u64 {
            let ticks = scaled_ticks(10_000_000 + c * 512, 1, 44_100).unwrap();
            let n = count(c, 0.7);
            let base = clock_total;
            clock.push(ticks, cycle_ns(c), n, |i| (base + i) as f32);
            clock_total += n;
            if (20..300).contains(&c) {
                let n = count(c, 0.2);
                let ticks = ticks.saturating_add_signed(jitter[c as usize % jitter.len()]);
                app.push(ticks, cycle_ns(c), n, |_| 0.5);
                app_total += n;
            }
            if c % 3 == 2 {
                clock.drain(&mut mixer, true).unwrap();
                app.drain(&mut mixer, false).unwrap();
                mixer.deliver(&mut sink, false).unwrap();
            }
        }
        clock.drain(&mut mixer, true).unwrap();
        app.drain(&mut mixer, false).unwrap();
        mixer.deliver(&mut sink, true).unwrap();

        let out = samples(&out.lock().unwrap());
        assert_eq!(out.len(), clock_total, "clock track gained or lost samples");
        let app_frames: Vec<usize> = out
            .iter()
            .enumerate()
            .map(|(k, v)| {
                let extra = v - k as f32;
                assert!(
                    extra == 0.0 || extra == 0.5,
                    "sample {k}: overlap or loss ({extra})"
                );
                k
            })
            .filter(|k| out[*k] != *k as f32)
            .collect();
        assert_eq!(
            app_frames.len(),
            app_total,
            "app audio was dropped or doubled"
        );
        assert_eq!(
            app_frames.last().unwrap() - app_frames[0] + 1,
            app_total,
            "app audio has a gap"
        );
    }

    /// Driver time of cycle `c` of a 44.1 kHz driver with a 512-tick quantum.
    fn cycle_ns(c: u64) -> u64 {
        5_000_000_000 + c * 512 * 1_000_000_000 / 44_100
    }
    fn step_count(c: u64) -> usize {
        let step = 512.0 * 48_000.0 / 44_100.0;
        ((c + 1) as f64 * step).floor() as usize - (c as f64 * step).floor() as usize
    }

    /// One app packet: its cycle, ticks and the cycle after whose drain it
    /// is pushed (its delivery may lag behind its own cycle).
    struct AppPacket {
        cycle: u64,
        ticks: u64,
        pushed_after: u64,
    }
    struct Mix {
        out: Vec<f32>,
        chunks: Vec<(usize, Instant)>,
        app_total: usize,
        frames: Vec<usize>,
        anchor_at: Instant,
    }
    /// The worker's mix of an inaudible clock (44.1 kHz driver, 512-tick
    /// quantum, cycles `0..400`, minus `skipped`) and one app stream playing
    /// 0.5 in `packets`. Drains every third cycle.
    fn mix(skipped: Option<u64>, packets: &[AppPacket]) -> Result<Mix> {
        let (mut sink, out) = collecting();
        let clock = TimedRing::new(0);
        let app_ring = TimedRing::new(CAPACITY);
        let (anchor, anchor_at) = anchor();
        let mut mixer = Mixer::new(anchor);
        let (mut app_total, mut frames, mut frame) = (0usize, Vec::new(), 0usize);
        let push_app = |after: u64, total: &mut usize| {
            for packet in packets.iter().filter(|p| p.pushed_after == after) {
                app_ring.push(
                    packet.ticks,
                    cycle_ns(packet.cycle),
                    step_count(packet.cycle),
                    |_| 0.5,
                );
                *total += step_count(packet.cycle);
            }
        };
        for c in 0..400u64 {
            frames.push(frame);
            frame += step_count(c);
            if skipped != Some(c) {
                let ticks = scaled_ticks(10_000_000 + c * 512, 1, 44_100).unwrap();
                clock.push(ticks, cycle_ns(c), step_count(c), |_| 0.0);
            }
            push_app(c, &mut app_total);
            if c % 3 == 2 {
                clock.drain(&mut mixer, true)?;
                app_ring.drain(&mut mixer, false)?;
                mixer.deliver(&mut sink, false)?;
            }
        }
        clock.drain(&mut mixer, true)?;
        app_ring.drain(&mut mixer, false)?;
        app_ring.ensure_drained()?;
        mixer.deliver(&mut sink, true)?;
        let chunks = out.lock().unwrap();
        let mut at = 0;
        let starts = chunks
            .iter()
            .map(|c| {
                let start = at;
                at += c.samples_mono_f32.len();
                (start, c.host_time.unwrap())
            })
            .collect();
        Ok(Mix {
            out: samples(&chunks),
            chunks: starts,
            app_total,
            frames,
            anchor_at,
        })
    }
    /// App packets for `cycles` pushed with their own cycle.
    fn app(cycles: std::ops::Range<u64>, ticks: impl Fn(u64) -> u64) -> Vec<AppPacket> {
        cycles
            .map(|c| AppPacket {
                cycle: c,
                ticks: ticks(c),
                pushed_after: c,
            })
            .collect()
    }
    fn graph_ticks(c: u64) -> u64 {
        scaled_ticks(10_000_000 + c * 512, 1, 44_100).unwrap()
    }

    /// The frames carrying app audio; panics on overlap (summed samples).
    fn app_frames(out: &[f32]) -> Vec<usize> {
        for (k, v) in out.iter().enumerate() {
            assert!(*v == 0.0 || *v == 0.5, "sample {k}: overlap ({v})");
        }
        (0..out.len()).filter(|k| out[*k] == 0.5).collect()
    }
    fn assert_contiguous(mix: &Mix) -> Vec<usize> {
        let app = app_frames(&mix.out);
        assert_eq!(app.len(), mix.app_total, "app audio was dropped or doubled");
        assert_eq!(app.last().unwrap() - app[0] + 1, mix.app_total, "gap");
        app
    }

    /// A clock cycle missed by the sink monitor moves the clock on by its
    /// ticks: the app keeps its own place, nothing overlaps, and the clock
    /// still covers (and delivers) all of the app's audio.
    #[test]
    fn a_skipped_clock_cycle_advances_the_clock() {
        let mix = mix(Some(100), &app(20..400, graph_ticks)).unwrap();
        assert_contiguous(&mix);
        assert!(mix.out.len().abs_diff(*mix.frames.last().unwrap() + 557) <= 2);
    }

    /// PipeWire rebases each stream's ticks to its own start: a playback
    /// stream appearing mid-session (ticks from 0) joins through its cycle
    /// time and then follows its own ticks, without failing as late audio
    /// or being dropped.
    #[test]
    fn a_stream_starting_at_tick_zero_joins_a_running_clock() {
        // Cycle 49 is drained after the clock's cycle 50 (`c % 3 == 2`).
        let ticks = |c: u64| scaled_ticks((c - 49) * 512, 1, 44_100).unwrap();
        let mix = mix(None, &app(49..300, ticks)).unwrap();
        let app = assert_contiguous(&mix);
        assert!(
            app[0].abs_diff(mix.frames[49]) <= 1,
            "{} vs {}",
            app[0],
            mix.frames[49]
        );
    }

    /// A first packet delivered late but still inside the holdback lands at
    /// its own cycle, not at the delivery cursor: the stream is not shifted.
    #[test]
    fn a_late_first_packet_within_the_holdback_keeps_its_cycle() {
        // Cycles 60.. are handed over three cycles (~35 ms with the drain
        // cadence) after their own, less than the 50 ms holdback.
        let packets: Vec<_> = (60..300)
            .map(|c| AppPacket {
                cycle: c,
                ticks: scaled_ticks((c - 60) * 512, 1, 44_100).unwrap(),
                pushed_after: c + 3,
            })
            .collect();
        let mix = mix(None, &packets).unwrap();
        let app = assert_contiguous(&mix);
        assert!(
            app[0].abs_diff(mix.frames[60]) <= 1,
            "{} vs {}",
            app[0],
            mix.frames[60]
        );
    }

    /// A first packet whose cycle was already delivered is not moved to the
    /// cursor (shifting the whole stream later) nor silently trimmed: the
    /// capture fails, so the recorder starts a fresh segment.
    #[test]
    fn a_first_packet_after_its_deadline_fails_instead_of_shifting() {
        let packets: Vec<_> = (60..300)
            .map(|c| AppPacket {
                cycle: c,
                ticks: scaled_ticks((c - 60) * 512, 1, 44_100).unwrap(),
                pushed_after: c + 30,
            })
            .collect();
        let error = mix(None, &packets).err().expect("late audio must fail");
        assert!(error.to_string().contains("deadline"), "{error}");
    }

    /// App packets handed over before the clock's first packet wait in
    /// their ring rather than being dropped, then land at their cycle.
    #[test]
    fn app_audio_before_the_first_clock_packet_waits_for_it() {
        let (mut sink, out) = collecting();
        let clock = TimedRing::new(0);
        let app_ring = TimedRing::new(CAPACITY);
        let mut mixer = Mixer::new(anchor().0);
        for c in 0..3 {
            app_ring.push(graph_ticks(c), cycle_ns(c), step_count(c), |_| 0.5);
        }
        app_ring.drain(&mut mixer, false).unwrap();
        assert!(app_ring.ensure_drained().is_err(), "packets were consumed");
        for c in 0..10 {
            clock.push(graph_ticks(c), cycle_ns(c), step_count(c), |_| 0.0);
        }
        clock.drain(&mut mixer, true).unwrap();
        app_ring.drain(&mut mixer, false).unwrap();
        app_ring.ensure_drained().unwrap();
        mixer.deliver(&mut sink, true).unwrap();
        let out = samples(&out.lock().unwrap());
        let app = app_frames(&out);
        let total: usize = (0..3).map(step_count).sum();
        assert_eq!(app, (0..total).collect::<Vec<_>>());
    }

    /// A stream whose ticks restart from zero mid-stream (rebased) rejoins
    /// through its cycle time instead of failing as late audio.
    #[test]
    fn a_stream_with_rebased_ticks_rejoins_at_its_cycle() {
        let ticks = |c: u64| {
            if c < 200 {
                graph_ticks(c)
            } else {
                scaled_ticks((c - 200) * 512, 1, 44_100).unwrap()
            }
        };
        let mix = mix(None, &app(20..300, ticks)).unwrap();
        let app = assert_contiguous(&mix);
        assert!(app[0].abs_diff(mix.frames[20]) <= 1);
    }

    /// Delivered chunks carry the graph-clock instant of their first sample,
    /// whatever the delivery cadence or holdback.
    #[test]
    fn system_chunks_carry_their_graph_clock_instant() {
        let mix = mix(None, &app(20..300, graph_ticks)).unwrap();
        assert!(mix.chunks.len() > 10);
        for (start, at) in &mix.chunks {
            let expected_ns = i128::from(cycle_ns(0)) + *start as i128 * NANOS / 48_000 - BASE_NS;
            let expected = mix.anchor_at + Duration::from_nanos(expected_ns as u64);
            let error = if *at > expected {
                *at - expected
            } else {
                expected - *at
            };
            assert!(
                error < Duration::from_micros(50),
                "frame {start}: {error:?}"
            );
        }
    }

    /// Microphone packets keep CPAL's capture times: contiguous packets
    /// share a chunk stamped with the first one, a gap starts a new chunk at
    /// its own time, and the time a stalled worker drains them is irrelevant.
    #[test]
    fn microphone_chunks_keep_capture_times_across_gaps_and_stalls() {
        let (mut sink, out) = collecting();
        let ring = TimedRing::new(48_000);
        let (clock, at) = anchor();
        let ms = 1_000_000u64;
        let base = BASE_NS as u64;
        // 3 × 10 ms contiguous, then a 50 ms gap, then 2 × 10 ms.
        for (k, t) in [0, 10, 20, 80, 90].into_iter().enumerate() {
            let value = k as f32;
            ring.push(0, base + t * ms, 160, |_| value);
        }
        std::thread::sleep(Duration::from_millis(20));
        deliver_timed(&ring, 16_000, clock, &mut sink).unwrap();
        let chunks = out.lock().unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].samples_mono_f32.len(), 480);
        assert_eq!(chunks[1].samples_mono_f32.len(), 320);
        assert_eq!(chunks[0].host_time, Some(at));
        assert_eq!(chunks[1].host_time, Some(at + Duration::from_millis(80)));
        assert_eq!(chunks[1].samples_mono_f32[0], 3.0);
        assert!(ring.pop_into(&mut Vec::new()).is_none());
    }

    #[test]
    fn a_clock_ring_stores_no_pcm() {
        let clock = TimedRing::new(0);
        clock.push(0, 1, 512, |_| panic!("clock PCM must never be read"));
        assert_eq!(clock.dropped(), 0);
        assert!(clock.samples.is_empty());
    }

    #[test]
    fn ticks_scale_to_the_capture_rate() {
        assert_eq!(scaled_ticks(48_000, 1, 48_000), Some(48_000));
        assert_eq!(scaled_ticks(44_100, 1, 44_100), Some(48_000));
        assert_eq!(scaled_ticks(1, 0, 48_000), None);
    }
}
