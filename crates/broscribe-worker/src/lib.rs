//! Live preview workers. [`live`]: bounded per-track pseudo-streaming ASR
//! fed by the daemon (track labels only). [`speakers`]: the companion
//! live-speakers worker that analyses closed journal chunks for
//! provisional voices and suggestions (plan D).
use broscribe_core::{Error, Result};
use broscribe_ipc::{
    Frame, LiveEvent, LiveSegment, LiveSpeaker, Track,
    live::{LIVE_RATE, LiveWord, read_frame},
};
use broscribe_models::{Asr, Vad};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub mod speakers;

/// One track's buffered audio and where it sits on the journal clock.
/// Frames are appended back to back when the device clock drifted by at
/// most 10 ms, so the buffer is a series of pieces, each anchored at its own
/// frame's start sample: a buffer index maps to the journal time of the
/// piece it falls in, never to a time guessed by concatenation. Consumed
/// audio is skipped by an offset and compacted only once it is at least
/// half the buffer, so the retained audio is not moved on every flush.
#[derive(Default)]
struct Pending {
    samples: Vec<f32>,
    /// Start of the unconsumed audio in `samples`.
    head: usize,
    /// `(index in samples, journal sample)` of every piece start, ascending;
    /// the first is at or before `head`.
    anchors: Vec<(usize, u64)>,
    /// Journal sample just after the last frame.
    next_sample: u64,
}

impl Pending {
    /// The unconsumed audio; "live indexes" below index into it.
    fn live(&self) -> &[f32] {
        &self.samples[self.head..]
    }

    /// Adds a frame; into an empty buffer it is moved, not copied.
    fn append(&mut self, start_sample: u64, samples: Vec<f32>) {
        let n = samples.len() as u64;
        if self.live().is_empty() {
            self.samples = samples;
            self.head = 0;
            self.anchors.clear();
            self.anchors.push((0, start_sample));
        } else {
            // A frame that continues exactly needs no anchor of its own.
            if start_sample != self.next_sample {
                self.anchors.push((self.samples.len(), start_sample));
            }
            self.samples.extend_from_slice(&samples);
        }
        self.next_sample = start_sample + n;
    }

    /// Journal sample of absolute index `abs` (`anchors[0].0 <= abs`).
    fn sample_of(&self, abs: usize) -> u64 {
        let k = self.anchors.partition_point(|(at, _)| *at <= abs) - 1;
        let (at, sample) = self.anchors[k];
        sample + (abs - at) as u64
    }

    /// Journal sample where live index `i` starts.
    fn start_at(&self, i: usize) -> u64 {
        self.sample_of(self.head + i)
    }

    /// Journal sample where live index `i` ends (one past the previous
    /// sample), so a range ends on the clock of its own last piece.
    fn end_at(&self, i: usize) -> u64 {
        if i == 0 {
            self.start_at(0)
        } else {
            self.start_at(i - 1) + 1
        }
    }

    /// The first live index whose journal sample is at or after `sample`.
    fn index_of(&self, sample: u64) -> usize {
        for (k, &(at, base)) in self.anchors.iter().enumerate() {
            let end = self
                .anchors
                .get(k + 1)
                .map_or(self.samples.len(), |&(next, _)| next);
            if sample < base + (end - at) as u64 {
                let abs = at + sample.saturating_sub(base) as usize;
                return abs.max(self.head) - self.head;
            }
        }
        self.samples.len() - self.head
    }

    /// Drops the first `n` live samples, keeping the anchors of the rest.
    fn consume(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if n >= self.live().len() {
            self.samples.clear();
            self.head = 0;
            self.anchors.clear();
            return;
        }
        self.head += n;
        let first = self.sample_of(self.head);
        let k = self.anchors.partition_point(|(at, _)| *at <= self.head) - 1;
        self.anchors.drain(..k);
        self.anchors[0] = (self.head, first);
        if self.head * 2 >= self.samples.len() {
            self.samples.drain(..self.head);
            for anchor in &mut self.anchors {
                anchor.0 -= self.head;
            }
            self.head = 0;
        }
    }
}

fn emit(out: &mut dyn Write, event: LiveEvent) -> Result<()> {
    let line = event
        .to_line()
        .map_err(|e| Error::Internal(e.to_string()))?;
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(|e| Error::Internal(format!("live output: {e}")))
}

/// How often a worker that is still loading its models says so.
pub const LOADING_INTERVAL: Duration = Duration::from_secs(1);

/// Runs `load` while a separate thread calls `report(elapsed_ms)` at once and
/// then about every [`LOADING_INTERVAL`]. The reporter has stopped for good
/// when this returns — the load finished, failed or panicked — so nothing it
/// writes can follow the caller's `Ready`. A failing report (the reader went
/// away) only ends the reports; the load goes on.
pub fn while_loading<T>(
    mut report: impl FnMut(u64) -> Result<()> + Send,
    load: impl FnOnce() -> Result<T>,
) -> Result<T> {
    /// Stops the reporter when the load ends, unwinding included.
    struct Stop<'a> {
        done: &'a AtomicBool,
        reporter: std::thread::Thread,
    }
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Release);
            self.reporter.unpark();
        }
    }

    let started = Instant::now();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let flag = &done;
        let reporter = scope.spawn(move || {
            let mut next = started;
            while !flag.load(Ordering::Acquire) {
                let now = Instant::now();
                if now < next {
                    std::thread::park_timeout(next - now);
                    continue;
                }
                let elapsed_ms =
                    u64::try_from(now.duration_since(started).as_millis()).unwrap_or(u64::MAX);
                if report(elapsed_ms).is_err() {
                    return;
                }
                next = (next + LOADING_INTERVAL).max(now);
            }
        });
        let stop = Stop {
            done: &done,
            reporter: reporter.thread().clone(),
        };
        let loaded = load();
        drop(stop);
        loaded
    })
}

/// Loads the live models through `load`, writing [`LiveEvent::Loading`] for
/// `model` on `out` meanwhile (see [`while_loading`]); [`live`] then writes
/// `Ready`.
pub fn load_live_models<T>(
    out: &mut (dyn Write + Send),
    model: &str,
    load: impl FnOnce() -> Result<T>,
) -> Result<T> {
    while_loading(
        |elapsed_ms| {
            emit(
                &mut *out,
                LiveEvent::Loading {
                    model: model.to_owned(),
                    elapsed_ms,
                },
            )
        },
        load,
    )
}

/// Consume binary frames, acknowledge input and transcribe VAD-closed regions.
/// Buffers at most 38 s per track (28 s segment + one ≤10 s input frame)
/// and flushes on a gap or end of input. Each piece of speech is recognised
/// in the session language span it falls in (language frames), split at
/// span boundaries; before the first span the runtime detects the language.
pub fn live(
    input: &mut dyn Read,
    out: &mut dyn Write,
    vad: &mut dyn Vad,
    asr: &dyn Asr,
    model: &str,
) -> Result<()> {
    emit(
        out,
        LiveEvent::Ready {
            model: model.into(),
        },
    )?;
    let mut pending = [Pending::default(), Pending::default()];
    let mut languages: Vec<(u64, String)> = Vec::new();
    let mut received: f64 = 0.0;
    while let Some(frame) =
        read_frame(input).map_err(|e| Error::InvalidInput(format!("live frame: {e}")))?
    {
        match frame {
            Frame::Audio(frame) => {
                let p = &mut pending[frame.track.index()];
                // Compare adjacent chunks: device and host clocks drift over time.
                if !p.live().is_empty()
                    && p.next_sample.abs_diff(frame.start_sample) > u64::from(LIVE_RATE / 100)
                {
                    flush(p, frame.track, true, out, vad, asr, &languages)?;
                }
                received = received.max(frame.end_s());
                p.append(frame.start_sample, frame.samples);
                flush(p, frame.track, false, out, vad, asr, &languages)?;
            }
            Frame::Language {
                start_sample,
                language,
            } => {
                let at = languages.partition_point(|(start, _)| *start <= start_sample);
                languages.insert(at, (start_sample, language));
            }
            Frame::Keepalive => {}
        }
        emit(
            out,
            LiveEvent::Ack {
                received_until_s: received,
            },
        )?;
    }
    for track in Track::ALL {
        flush(
            &mut pending[track.index()],
            track,
            true,
            out,
            vad,
            asr,
            &languages,
        )?;
    }
    Ok(())
}

/// `[start, end)` (live indexes of `p`) cut where a language span starts on
/// the journal clock, each piece with the language in force at its start
/// (`None` before the first span).
fn language_pieces<'l>(
    p: &Pending,
    start: usize,
    end: usize,
    languages: &'l [(u64, String)],
) -> Vec<(usize, usize, Option<&'l str>)> {
    let first = languages.partition_point(|(at, _)| *at <= p.start_at(start));
    let mut language = first.checked_sub(1).map(|i| languages[i].1.as_str());
    let mut from = start;
    let mut pieces = Vec::new();
    for (at, next) in &languages[first..] {
        let cut = p.index_of(*at).clamp(start, end);
        if cut >= end {
            break;
        }
        if cut > from {
            pieces.push((from, cut, language));
            from = cut;
        }
        language = Some(next.as_str());
    }
    pieces.push((from, end, language));
    pieces
}

fn flush(
    p: &mut Pending,
    track: Track,
    ending: bool,
    out: &mut dyn Write,
    vad: &mut dyn Vad,
    asr: &dyn Asr,
    languages: &[(u64, String)],
) -> Result<()> {
    let len = p.live().len();
    if len == 0 {
        return Ok(());
    }
    let rate = f64::from(LIVE_RATE);
    let max_call = LIVE_RATE as usize * 28;
    let duration = len as f64 / rate;
    let forced = duration >= 28.0;
    let regions = vad.segments(p.live())?;
    let mut consumed = 0;
    let no_speech = regions.is_empty();
    for region in regions {
        if !ending && !forced && duration - region.end_s < 0.5 {
            break;
        }
        let start = (region.start_s.max(0.0) * rate) as usize;
        let end = ((region.end_s * rate) as usize).min(len);
        if end <= start {
            continue;
        }
        for (from, to, language) in language_pieces(p, start, end, languages) {
            // VAD implementations may return long regions: cap ASR calls at 28 s.
            for (part, audio) in p.live()[from..to].chunks(max_call).enumerate() {
                let offset = from + part * max_call;
                let result = match language {
                    Some(language) => asr.transcribe_with(audio, Some(language)),
                    None => asr.transcribe(audio),
                };
                // Times relative to the call, onto the journal clock of the
                // piece each falls in.
                let at = |seconds: f64, end: bool| {
                    let i = offset + ((seconds.max(0.0) * rate) as usize).min(audio.len());
                    let sample = if end {
                        p.end_at(i)
                    } else {
                        p.start_at(i.min(offset + audio.len() - 1))
                    };
                    sample as f64 / rate
                };
                match result {
                    Ok(result) if !result.text.trim().is_empty() => emit(
                        out,
                        LiveEvent::Segment(LiveSegment {
                            track,
                            start_s: p.start_at(offset) as f64 / rate,
                            end_s: p.end_at(offset + audio.len()) as f64 / rate,
                            words: broscribe_pipeline::analyze::tokens_to_words(
                                &result.tokens,
                                0.0,
                            )
                            .into_iter()
                            .map(|w| LiveWord {
                                text: w.text,
                                start_s: at(w.start, false),
                                end_s: at(w.end, true),
                            })
                            .collect(),
                            text: result.text,
                            speaker: if track == Track::Mic {
                                LiveSpeaker::Me
                            } else {
                                LiveSpeaker::Others
                            },
                        }),
                    )?,
                    Ok(_) => {}
                    Err(_) => emit(
                        out,
                        LiveEvent::Warning {
                            message: "A preview segment could not be transcribed".into(),
                        },
                    )?,
                }
            }
        }
        consumed = end;
    }
    if ending || forced {
        consumed = len;
    }
    // Keep only a short silence tail when VAD sees no speech.
    if consumed == 0 && duration > 1.0 && no_speech {
        consumed = len - LIVE_RATE as usize / 2;
    }
    p.consume(consumed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_models::{AsrOutput, SpeechSegment};
    struct Speech;
    impl Vad for Speech {
        fn segments(&mut self, s: &[f32]) -> Result<Vec<SpeechSegment>> {
            Ok(vec![SpeechSegment {
                start_s: 0.0,
                end_s: s.len() as f64 / 16000.0,
            }])
        }
    }
    impl Asr for Speech {
        fn transcribe(&self, _: &[f32]) -> Result<AsrOutput> {
            Ok(AsrOutput {
                text: "Synthetic words".into(),
                tokens: Vec::new(),
            })
        }
        fn accepts_hotwords(&self) -> bool {
            false
        }
    }

    #[test]
    fn loading_is_reported_about_once_a_second_and_never_after_ready() {
        let mut out = Vec::new();
        let loaded = load_live_models(&mut out, "live-m", || {
            std::thread::sleep(Duration::from_millis(1300));
            Ok(7)
        })
        .unwrap();
        assert_eq!(loaded, 7);
        live(
            &mut std::io::empty(),
            &mut out,
            &mut Speech,
            &Speech,
            "live-m",
        )
        .unwrap();
        let events: Vec<LiveEvent> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| LiveEvent::parse(l).unwrap())
            .collect();
        let (ready, loading) = events.split_last().unwrap();
        assert_eq!(
            ready,
            &LiveEvent::Ready {
                model: "live-m".into()
            }
        );
        let elapsed: Vec<u64> = loading
            .iter()
            .map(|e| match e {
                LiveEvent::Loading { model, elapsed_ms } if model == "live-m" => *elapsed_ms,
                other => panic!("only loading before ready: {other:?}"),
            })
            .collect();
        assert!((2..=3).contains(&elapsed.len()), "{elapsed:?}");
        assert!(elapsed[0] < 500, "reported at once: {elapsed:?}");
        assert!(
            elapsed.windows(2).all(|w| w[1] >= w[0] + 900),
            "{elapsed:?}"
        );
    }

    #[test]
    fn failed_loads_and_gone_readers_end_the_reports_cleanly() {
        struct Gone;
        impl Write for Gone {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // A reader that went away stops the reports, not the load.
        let loaded = load_live_models(&mut Gone, "m", || {
            std::thread::sleep(Duration::from_millis(50));
            Ok("models")
        });
        assert_eq!(loaded.unwrap(), "models");
        // A failed load is returned as is, after the reports stopped.
        let mut out = Vec::new();
        let err = speakers::load_models(&mut out, "diar", "emb", || -> Result<()> {
            Err(Error::ModelMissing("synthetic".into()))
        })
        .unwrap_err();
        assert_eq!(err.code(), "model_missing");
        for line in String::from_utf8(out).unwrap().lines() {
            assert!(matches!(
                broscribe_ipc::live::SpeakersEvent::parse(line).unwrap(),
                broscribe_ipc::live::SpeakersEvent::Loading { ref diarizer, ref embedder, .. }
                    if diarizer == "diar" && embedder == "emb"
            ));
        }
    }

    #[test]
    fn keeps_tracks_and_recording_offsets_separate() {
        let mut bytes = Vec::new();
        for track in Track::ALL {
            broscribe_ipc::live::write_frame(
                &mut bytes,
                &Frame::Audio(broscribe_ipc::LiveFrame {
                    track,
                    start_sample: 32000,
                    samples: vec![0.1; 16000],
                }),
            )
            .unwrap();
        }
        let mut output = Vec::new();
        live(
            &mut bytes.as_slice(),
            &mut output,
            &mut Speech,
            &Speech,
            "synthetic",
        )
        .unwrap();
        let segments: Vec<_> = String::from_utf8(output)
            .unwrap()
            .lines()
            .filter_map(|l| match LiveEvent::parse(l).unwrap() {
                LiveEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].start_s, 2.0);
        assert_eq!(segments[1].end_s, 3.0);
        assert_eq!(segments[0].speaker, LiveSpeaker::Me);
        assert_eq!(segments[1].speaker, LiveSpeaker::Others);
    }

    #[test]
    fn record_live_clock_drift_does_not_split_contiguous_preview() {
        let mut bytes = Vec::new();
        for start in [0, 16005, 32010] {
            broscribe_ipc::write_frame(
                &mut bytes,
                &Frame::Audio(broscribe_ipc::LiveFrame {
                    track: Track::Mic,
                    start_sample: start,
                    samples: vec![0.1; 16000],
                }),
            )
            .unwrap();
        }
        let mut output = Vec::new();
        live(
            &mut bytes.as_slice(),
            &mut output,
            &mut Speech,
            &Speech,
            "synthetic",
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        let segments: Vec<_> = output
            .lines()
            .filter_map(|line| match LiveEvent::parse(line).unwrap() {
                LiveEvent::Segment(segment) => Some(segment),
                _ => None,
            })
            .collect();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start_s, 0.0);
        // The end sits on the last frame's own anchor, not on 3 s of
        // concatenated audio.
        assert_eq!(segments[0].end_s, 48_010.0 / 16_000.0);
    }

    /// Records the language and length of every call and answers with two
    /// words: one at the start of the call, one 0.25 s before its end.
    #[derive(Default)]
    struct Timed(std::cell::RefCell<Vec<(Option<String>, usize)>>);
    impl Asr for Timed {
        fn transcribe(&self, s: &[f32]) -> Result<AsrOutput> {
            self.transcribe_with(s, None)
        }
        fn transcribe_with(&self, s: &[f32], language: Option<&str>) -> Result<AsrOutput> {
            self.0
                .borrow_mut()
                .push((language.map(str::to_owned), s.len()));
            let last = s.len() as f32 / 16_000.0 - 0.25;
            Ok(AsrOutput {
                text: "a b".into(),
                tokens: vec![
                    broscribe_models::AsrToken {
                        text: "\u{2581}a".into(),
                        start_s: 0.0,
                        duration_s: Some(0.1),
                    },
                    broscribe_models::AsrToken {
                        text: "\u{2581}b".into(),
                        start_s: last,
                        duration_s: Some(0.1),
                    },
                ],
            })
        }
        fn accepts_hotwords(&self) -> bool {
            false
        }
    }

    #[test]
    fn record_live_times_keep_each_frame_anchor_through_drift() {
        // The second frame starts 100 samples late (within the 10 ms that
        // keeps it contiguous); a language span starts inside it.
        let frames = [
            Frame::Language {
                start_sample: 24_100,
                language: "pt".into(),
            },
            Frame::Audio(broscribe_ipc::LiveFrame {
                track: Track::Mic,
                start_sample: 0,
                samples: vec![0.1; 16_000],
            }),
            Frame::Audio(broscribe_ipc::LiveFrame {
                track: Track::Mic,
                start_sample: 16_100,
                samples: vec![0.1; 16_000],
            }),
        ];
        let mut bytes = Vec::new();
        for frame in &frames {
            broscribe_ipc::write_frame(&mut bytes, frame).unwrap();
        }
        let asr = Timed::default();
        let mut output = Vec::new();
        live(
            &mut bytes.as_slice(),
            &mut output,
            &mut Speech,
            &asr,
            "synthetic",
        )
        .unwrap();
        // Cut where the span starts on the journal clock: 8 000 samples into
        // the second frame, i.e. buffer index 24 000.
        assert_eq!(
            *asr.0.borrow(),
            [(None, 24_000), (Some("pt".into()), 8_000)]
        );
        let segments: Vec<_> = String::from_utf8(output)
            .unwrap()
            .lines()
            .filter_map(|line| match LiveEvent::parse(line).unwrap() {
                LiveEvent::Segment(segment) => Some(segment),
                _ => None,
            })
            .collect();
        let s = |samples: u64| samples as f64 / 16_000.0;
        assert_eq!(segments.len(), 2);
        assert_eq!((segments[0].start_s, segments[0].end_s), (0.0, s(24_100)));
        assert_eq!(
            (segments[1].start_s, segments[1].end_s),
            (s(24_100), s(32_100))
        );
        // Words land on the anchor of the frame they fall in.
        let starts: Vec<Vec<f64>> = segments
            .iter()
            .map(|seg| seg.words.iter().map(|w| w.start_s).collect())
            .collect();
        assert_eq!(
            starts,
            [vec![0.0, s(16_100 + 4_000)], vec![s(24_100), s(28_100)]]
        );
    }

    #[test]
    fn retained_audio_keeps_its_anchors_without_moving_on_every_consume() {
        let mut p = Pending::default();
        p.append(0, vec![0.0; 16_000]);
        p.append(16_100, vec![0.0; 16_000]);
        p.consume(10_000);
        // Skipped by offset: nothing moved yet.
        assert_eq!((p.head, p.samples.len()), (10_000, 32_000));
        assert_eq!(p.start_at(0), 10_000);
        assert_eq!(p.start_at(6_000), 16_100);
        assert_eq!(p.index_of(16_100), 6_000);
        p.consume(8_000);
        // Half the buffer consumed: compacted once, anchors rebased.
        assert_eq!((p.head, p.samples.len()), (0, 14_000));
        assert_eq!(p.anchors, [(0, 18_100)]);
        assert_eq!(p.start_at(0), 18_100);
        assert_eq!(p.end_at(14_000), 32_100);
        p.consume(14_000);
        assert!(p.live().is_empty() && p.anchors.is_empty());
    }

    /// Records the language of every recognition call.
    #[derive(Default)]
    struct Languages(std::cell::RefCell<Vec<(Option<String>, usize)>>);
    impl Asr for Languages {
        fn transcribe(&self, s: &[f32]) -> Result<AsrOutput> {
            self.transcribe_with(s, None)
        }
        fn transcribe_with(&self, s: &[f32], language: Option<&str>) -> Result<AsrOutput> {
            self.0
                .borrow_mut()
                .push((language.map(str::to_owned), s.len()));
            Speech.transcribe(s)
        }
        fn accepts_hotwords(&self) -> bool {
            false
        }
    }

    #[test]
    fn live_recognition_follows_session_language_spans() {
        let mut bytes = Vec::new();
        let frames = [
            Frame::Language {
                start_sample: 16_000,
                language: "en".into(),
            },
            Frame::Audio(broscribe_ipc::LiveFrame {
                track: Track::Mic,
                start_sample: 0,
                samples: vec![0.1; 48_000],
            }),
            // A change made while the region is still open.
            Frame::Language {
                start_sample: 32_000,
                language: "pt".into(),
            },
        ];
        for frame in &frames {
            broscribe_ipc::write_frame(&mut bytes, frame).unwrap();
        }
        let asr = Languages::default();
        let mut output = Vec::new();
        live(
            &mut bytes.as_slice(),
            &mut output,
            &mut Speech,
            &asr,
            "synthetic",
        )
        .unwrap();
        assert_eq!(
            *asr.0.borrow(),
            [
                (None, 16_000),
                (Some("en".into()), 16_000),
                (Some("pt".into()), 16_000),
            ]
        );
        let starts: Vec<_> = String::from_utf8(output)
            .unwrap()
            .lines()
            .filter_map(|line| match LiveEvent::parse(line).unwrap() {
                LiveEvent::Segment(segment) => Some(segment.start_s),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [0.0, 1.0, 2.0]);
    }
}
