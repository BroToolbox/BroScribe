//! `broscribe-worker live-speakers <session>`: the companion live-speakers
//! worker (plan D). It never touches the capture pipe or the ASR worker: it
//! polls the session journal for closed, fsynced chunks, analyses one
//! bounded window per hop per track at utility priority and writes
//! [`SpeakersEvent`] lines. A heartbeat thread reports pending audio and
//! how long the current window has run, so the daemon can tell a stuck
//! window (no useful progress) from a live process.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use broscribe_core::{Error, Result};
use broscribe_ipc::live::{SpeakersEvent, SpeakersInput, SpeakersStart};
use broscribe_pipeline::live_identity::{Driver, EvidenceSource, SpeakerModels};

/// How often the heartbeat thread writes.
pub const HEARTBEAT: Duration = Duration::from_secs(1);

type Out = Arc<Mutex<Box<dyn Write + Send>>>;

fn write_event(out: &Out, event: &SpeakersEvent) -> Result<()> {
    let line = event
        .to_line()
        .map_err(|e| Error::Internal(e.to_string()))?;
    let mut out = out
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(|e| Error::Internal(format!("live speakers output: {e}")))
}

/// Reads the first stdin line: the session's [`SpeakersStart`].
pub fn read_start(input: &mut dyn BufRead) -> Result<SpeakersStart> {
    let mut line = String::new();
    input
        .read_line(&mut line)
        .map_err(|e| Error::InvalidInput(format!("live speakers start: {e}")))?;
    serde_json::from_str(line.trim_end())
        .map_err(|e| Error::InvalidInput(format!("live speakers start: {e}")))
}

/// Loads the speaker models through `load`, writing
/// [`SpeakersEvent::Loading`] on `out` meanwhile (see
/// [`crate::while_loading`]); [`run`] then writes `Ready`.
pub fn load_models<T>(
    out: &mut (dyn Write + Send),
    diarizer: &str,
    embedder: &str,
    load: impl FnOnce() -> Result<T>,
) -> Result<T> {
    crate::while_loading(
        |elapsed_ms| {
            let line = SpeakersEvent::Loading {
                diarizer: diarizer.to_owned(),
                embedder: embedder.to_owned(),
                elapsed_ms,
            }
            .to_line()
            .map_err(|e| Error::Internal(e.to_string()))?;
            writeln!(out, "{line}")
                .and_then(|()| out.flush())
                .map_err(|e| Error::Internal(format!("live speakers output: {e}")))
        },
        load,
    )
}

/// Runs until stdin ends (the daemon stops it), or the session stopped and
/// everything durable was analysed, or its journal is gone. `input` carries
/// the [`SpeakersInput`] lines after the start line; it is read on its own
/// thread.
pub fn run(
    start: &SpeakersStart,
    input: Box<dyn BufRead + Send>,
    out: Box<dyn Write + Send>,
    models: &SpeakerModels<'_>,
    model_ids: (&str, &str),
    evidence: &mut dyn EvidenceSource,
) -> Result<()> {
    let out: Out = Arc::new(Mutex::new(out));
    let mut driver = Driver::new(start)?;
    let poll = Duration::from_millis(start.settings.poll_ms);
    let (tx, rx) = mpsc::channel::<SpeakersInput>();
    // Detached: a blocked read must not keep the worker alive; the
    // process exits when `run` returns.
    std::thread::Builder::new()
        .name("speakers-stdin".into())
        .spawn(move || {
            for line in input.lines() {
                let Ok(line) = line else {
                    return;
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<SpeakersInput>(&line) {
                    Ok(message) => {
                        if tx.send(message).is_err() {
                            return;
                        }
                    }
                    // Unknown input is ignored, never fatal.
                    Err(_) => continue,
                }
            }
        })
        .map_err(|e| Error::Internal(format!("live speakers input thread: {e}")))?;

    let epoch = Instant::now();
    let busy_since = AtomicU64::new(0);
    let pending_ms = AtomicU64::new(0);
    let done = AtomicBool::new(false);
    write_event(
        &out,
        &SpeakersEvent::Ready {
            epoch: start.epoch,
            diarizer: model_ids.0.to_owned(),
            embedder: model_ids.1.to_owned(),
        },
    )?;
    std::thread::scope(|scope| {
        let heartbeat_out = Arc::clone(&out);
        let (busy_since, pending_ms, done) = (&busy_since, &pending_ms, &done);
        scope.spawn(move || {
            while !done.load(Ordering::Acquire) {
                let since = busy_since.load(Ordering::Acquire);
                let now = epoch.elapsed().as_millis() as u64;
                let busy_ms = if since == 0 {
                    0
                } else {
                    now.saturating_sub(since)
                };
                let beat = SpeakersEvent::Heartbeat {
                    pending_s: pending_ms.load(Ordering::Acquire) as f64 / 1000.0,
                    busy_ms,
                };
                if write_event(&heartbeat_out, &beat).is_err() {
                    return;
                }
                std::thread::park_timeout(HEARTBEAT);
            }
        });
        let busy = |on: bool| {
            let at = if on {
                (epoch.elapsed().as_millis() as u64).max(1)
            } else {
                0
            };
            busy_since.store(at, Ordering::Release);
        };
        let mut emit = |event: SpeakersEvent| write_event(&out, &event);
        let result = (|| -> Result<()> {
            loop {
                loop {
                    match rx.try_recv() {
                        Ok(message) => driver.input(message, models, evidence, &busy, &mut emit)?,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return Ok(()),
                    }
                }
                let step = driver.step(models, evidence, &busy, &mut emit)?;
                pending_ms.store((step.pending_s * 1000.0) as u64, Ordering::Release);
                if step.finished {
                    return Ok(());
                }
                match rx.recv_timeout(poll) {
                    Ok(message) => driver.input(message, models, evidence, &busy, &mut emit)?,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
            }
        })();
        done.store(true, Ordering::Release);
        result
    })
}

/// `live-speakers-replay` inputs.
#[derive(Clone, Debug)]
pub struct ReplayFile {
    /// Raw mono little-endian f32 PCM.
    pub pcm: std::path::PathBuf,
    pub rate: u32,
    pub track: broscribe_ipc::Track,
    pub pace: f64,
    pub drift_ppm: f64,
    pub pause: Option<(f64, f64)>,
    pub window_s: Option<f64>,
    pub hop_s: Option<f64>,
    pub journal_root: Option<std::path::PathBuf>,
    pub events: bool,
}

/// Replays a PCM file through a synthetic journal and the live-speakers
/// analysis (see `broscribe_pipeline::live_identity::replay`), writing the
/// events (when asked) and then one `{"type":"summary",…}` line. A default
/// journal folder is temporary and removed afterwards.
pub fn replay_file(
    args: &ReplayFile,
    models: &SpeakerModels<'_>,
    evidence: &mut dyn EvidenceSource,
    out: &mut dyn Write,
) -> Result<()> {
    use broscribe_pipeline::live_identity::{ReplayOptions, replay};
    let bytes = std::fs::read(&args.pcm).map_err(|e| Error::io_at(&args.pcm, e))?;
    if bytes.len() % 4 != 0 {
        return Err(Error::InvalidInput(
            "PCM must be little-endian f32 samples".into(),
        ));
    }
    let pcm: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let temporary = args.journal_root.is_none();
    let root = args.journal_root.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("broscribe-live-replay-{}", std::process::id()))
    });
    let mut opts = ReplayOptions::new(root.clone(), args.rate);
    opts.track = args.track;
    opts.pace = args.pace;
    opts.drift_ppm = args.drift_ppm;
    opts.pause = args.pause;
    if let Some(w) = args.window_s {
        opts.settings.window_s = w;
    }
    if let Some(h) = args.hop_s {
        opts.settings.hop_s = h;
    }
    let outcome = replay(&pcm, &opts, models, evidence, &mut |event| {
        if args.events {
            let line = event
                .to_line()
                .map_err(|e| Error::Internal(e.to_string()))?;
            writeln!(out, "{line}").map_err(|e| Error::Internal(format!("replay output: {e}")))?;
        }
        Ok(())
    });
    if temporary {
        let _ = std::fs::remove_dir_all(&root);
    }
    let summary = outcome?;
    let mut value = serde_json::to_value(&summary)
        .map_err(|e| Error::Internal(format!("replay summary: {e}")))?;
    if let Some(map) = value.as_object_mut() {
        map.insert("type".into(), "summary".into());
    }
    writeln!(out, "{value}")
        .and_then(|()| out.flush())
        .map_err(|e| Error::Internal(format!("replay output: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_capture::journal::{JournalWriter, SessionMeta, Track};
    use broscribe_ipc::live::LiveIdentitySettings;
    use broscribe_models::fakes::{ScheduleDiarizer, ToneEmbedder, sine};
    use broscribe_pipeline::live_identity::Evidence;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct Fixed(Arc<Evidence>);

    impl EvidenceSource for Fixed {
        fn current(&mut self) -> Result<Arc<Evidence>> {
            Ok(Arc::clone(&self.0))
        }
    }

    #[test]
    fn record_live_speakers_worker_runs_to_the_end_of_a_stopped_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut journal =
            JournalWriter::create(tmp.path(), "s1", SessionMeta::default(), 5.0, 1).unwrap();
        journal
            .begin_segment(Track::System, 0, None, 16_000, 0)
            .unwrap();
        let second = sine(200.0, 1.0, 0.3);
        for t in 0..12u64 {
            journal
                .append(Track::System, t * 1_000_000_000, &second)
                .unwrap();
        }
        let dir = journal.dir().to_owned();
        journal.finish(12_000_000_000, "user").unwrap();
        let start = SpeakersStart {
            session_id: "s1".into(),
            journal_dir: dir,
            epoch: 7,
            settings: LiveIdentitySettings {
                poll_ms: 10,
                ..Default::default()
            },
        };
        let (reader, writer) = std::io::pipe().unwrap();
        let out = Shared::default();
        let embedder = ToneEmbedder::new("tone", 16);
        run(
            &start,
            Box::new(std::io::BufReader::new(reader)),
            Box::new(out.clone()),
            &SpeakerModels {
                diarizer: &ScheduleDiarizer::default(),
                embedder: &embedder,
            },
            ("diarizer", "embedder"),
            &mut Fixed(Arc::new(Evidence {
                fingerprint: "e".into(),
                ..Default::default()
            })),
        )
        .unwrap();
        drop(writer);
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let events: Vec<SpeakersEvent> = text
            .lines()
            .map(|l| SpeakersEvent::parse(l).unwrap())
            .collect();
        assert!(matches!(events[0], SpeakersEvent::Ready { epoch: 7, .. }));
        let windows: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SpeakersEvent::Window(w) => Some(w),
                _ => None,
            })
            .collect();
        assert_eq!(windows.len(), 3, "5 s, 10 s and the stopped tail");
        assert!(windows.iter().all(|w| w.epoch == 7 && !w.spans.is_empty()));
        assert!(!text.contains("embedding"));
    }

    /// Evidence that takes `delay` to load, as a slow library read would.
    struct Slow(Arc<Evidence>, Duration);

    impl EvidenceSource for Slow {
        fn current(&mut self) -> Result<Arc<Evidence>> {
            std::thread::sleep(self.1);
            Ok(Arc::clone(&self.0))
        }
    }

    #[test]
    fn record_live_session_sample_counts_as_busy_for_the_watchdog() {
        let tmp = tempfile::tempdir().unwrap();
        let mut journal =
            JournalWriter::create(tmp.path(), "s1", SessionMeta::default(), 5.0, 1).unwrap();
        journal
            .begin_segment(Track::System, 0, None, 16_000, 0)
            .unwrap();
        let start = SpeakersStart {
            session_id: "s1".into(),
            journal_dir: journal.dir().to_owned(),
            epoch: 1,
            settings: LiveIdentitySettings {
                poll_ms: 10,
                ..Default::default()
            },
        };
        let (reader, mut writer) = std::io::pipe().unwrap();
        let sample = SpeakersInput::SessionSample {
            turn_id: "t1".into(),
            decision: 1,
            forget_fence: Some("fence".into()),
            track: broscribe_ipc::Track::System,
            from_sample: 0,
            to_sample: 16_000,
            person_id: "p1".into(),
        };
        writeln!(writer, "{}", serde_json::to_string(&sample).unwrap()).unwrap();
        // End of stdin after the sample: the worker exits once it is handled.
        drop(writer);
        let out = Shared::default();
        let embedder = ToneEmbedder::new("tone", 16);
        run(
            &start,
            Box::new(std::io::BufReader::new(reader)),
            Box::new(out.clone()),
            &SpeakerModels {
                diarizer: &ScheduleDiarizer::default(),
                embedder: &embedder,
            },
            ("diarizer", "embedder"),
            &mut Slow(
                Arc::new(Evidence {
                    fingerprint: "e".into(),
                    ..Default::default()
                }),
                HEARTBEAT + HEARTBEAT / 2,
            ),
        )
        .unwrap();
        drop(journal);
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let events: Vec<SpeakersEvent> = text
            .lines()
            .map(|l| SpeakersEvent::parse(l).unwrap())
            .collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SpeakersEvent::Heartbeat { busy_ms, .. } if *busy_ms > 0)),
            "a heartbeat during the session sample reports busy time: {events:?}"
        );
        assert!(
            events.iter().any(
                |e| matches!(e, SpeakersEvent::SessionSample { turn_id, .. } if turn_id == "t1")
            )
        );
    }
}
