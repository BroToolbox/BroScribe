//! Synthetic replay tests of the live speaker preview (plan D): journals of
//! generated tones, a tone diarizer and the tone embedder. No capture and
//! no real audio.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use broscribe_capture::journal::{JournalWriter, SessionMeta, Track as CaptureTrack};
use broscribe_capture::timeline::{Timeline, TimelineOptions};
use broscribe_core::config::Thresholds;
use broscribe_core::live_identity::{
    LiveAction, LiveOutcome, LiveRange, NewDecision, read_sidecar, record_decision,
};
use broscribe_core::ops::reapply_utterance_people;
use broscribe_core::schema::{self, Meeting, ReviewSource, Segment, Source};
use broscribe_core::{Library, PersonId, Result};
use broscribe_ipc::Track;
use broscribe_ipc::live::{
    LiveIdentitySettings, SpeakerWindow, SpeakersEvent, SpeakersInput, SpeakersStart,
    SuggestionSource,
};
use broscribe_models::fakes::ToneEmbedder;
use broscribe_models::{DiarSegment, Diarizer};

use super::*;

const DEVICE_RATE: u32 = 48_000;

/// Labels 0.25 s blocks by their dominant tone (20 Hz buckets); silence is
/// no speech. Deterministic and window-local, like a real diarizer's labels.
struct ToneDiarizer;

impl Diarizer for ToneDiarizer {
    fn diarize(&self, samples: &[f32]) -> Result<Vec<DiarSegment>> {
        let block = 4_000;
        let mut labels: Vec<u64> = Vec::new();
        let mut out: Vec<DiarSegment> = Vec::new();
        for (i, chunk) in samples.chunks(block).enumerate() {
            let rms = (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt();
            if rms < 1e-3 || chunk.len() < block / 2 {
                continue;
            }
            let bucket = (ToneEmbedder::dominant_frequency(chunk) / 20.0).round() as u64;
            let speaker = match labels.iter().position(|b| *b == bucket) {
                Some(p) => p,
                None => {
                    labels.push(bucket);
                    labels.len() - 1
                }
            } as u32;
            let start = (i * block) as f64 / 16_000.0;
            let end = start + chunk.len() as f64 / 16_000.0;
            match out.last_mut() {
                Some(last) if last.speaker == speaker && (last.end_s - start).abs() < 1e-9 => {
                    last.end_s = end;
                }
                _ => out.push(DiarSegment {
                    start_s: start,
                    end_s: end,
                    speaker,
                }),
            }
        }
        Ok(out)
    }
}

/// Fixed evidence: the given people exist, nobody has stored samples.
struct FixedEvidence(Arc<Evidence>);

impl EvidenceSource for FixedEvidence {
    fn current(&mut self) -> Result<Arc<Evidence>> {
        Ok(Arc::clone(&self.0))
    }
}

fn evidence(people: &[PersonId], thresholds: Option<Thresholds>) -> FixedEvidence {
    FixedEvidence(Arc::new(Evidence {
        fingerprint: "evidence-1".into(),
        people: Vec::new(),
        thresholds,
        person_ids: people.iter().cloned().collect::<BTreeSet<_>>(),
        forget_fences: Default::default(),
        me: None,
        allow_uncalibrated_me: false,
    }))
}

/// `seconds` of a tone (`0` Hz: silence) at the device rate.
fn tone(freq: f64, seconds: f64) -> Vec<f32> {
    let n = (seconds * f64::from(DEVICE_RATE)) as usize;
    (0..n)
        .map(|i| {
            if freq == 0.0 {
                0.0
            } else {
                (0.3 * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(DEVICE_RATE))
                    .sin()) as f32
            }
        })
        .collect()
}

/// Appends `parts` (Hz, seconds) to the system track from `at_s`, in 1 s
/// blocks; returns the end time.
fn append(journal: &mut JournalWriter, at_s: f64, parts: &[(f64, f64)]) -> f64 {
    let mut t = at_s;
    for &(freq, seconds) in parts {
        let audio = tone(freq, seconds);
        for (i, block) in audio.chunks(DEVICE_RATE as usize).enumerate() {
            let host = ((t + i as f64) * 1e9) as u64;
            journal.append(CaptureTrack::System, host, block).unwrap();
        }
        t += seconds;
    }
    t
}

fn journal(root: &Path) -> JournalWriter {
    let mut journal = JournalWriter::create(
        root,
        "live-s1",
        SessionMeta::default(),
        5.0,
        std::process::id(),
    )
    .unwrap();
    journal
        .begin_segment(CaptureTrack::System, 0, None, DEVICE_RATE, 0)
        .unwrap();
    journal
}

fn start(dir: &Path) -> SpeakersStart {
    SpeakersStart {
        session_id: "live-s1".into(),
        journal_dir: dir.to_path_buf(),
        epoch: 1,
        settings: LiveIdentitySettings::default(),
    }
}

/// Runs steps until nothing more is due; returns the events.
fn drain(
    driver: &mut Driver,
    evidence: &mut dyn EvidenceSource,
    embedder: &ToneEmbedder,
) -> (Vec<SpeakersEvent>, Step) {
    let models = SpeakerModels {
        diarizer: &ToneDiarizer,
        embedder,
    };
    let mut events = Vec::new();
    let mut last = Step::default();
    for _ in 0..50 {
        last = driver
            .step(&models, evidence, &|_| {}, &mut |e| {
                events.push(e);
                Ok(())
            })
            .unwrap();
        if last.finished || last.windows == 0 {
            break;
        }
    }
    (events, last)
}

/// Appends `parts` second by second from `at_s`, running the driver after
/// every 5 s of audio (one closed chunk, the worker's pace); returns the end
/// time.
fn feed(
    journal: &mut JournalWriter,
    at_s: f64,
    parts: &[(f64, f64)],
    driver: &mut Driver,
    evidence: &mut dyn EvidenceSource,
    embedder: &ToneEmbedder,
    events: &mut Vec<SpeakersEvent>,
) -> f64 {
    let mut t = at_s;
    for &(freq, seconds) in parts {
        for _ in 0..seconds as usize {
            append(journal, t, &[(freq, 1.0)]);
            t += 1.0;
            if (t as u64).is_multiple_of(5) {
                events.extend(drain(driver, evidence, embedder).0);
            }
        }
    }
    t
}

fn windows(events: &[SpeakersEvent]) -> Vec<&SpeakerWindow> {
    events
        .iter()
        .filter_map(|e| match e {
            SpeakersEvent::Window(w) => Some(w),
            _ => None,
        })
        .collect()
}

/// The voice of the span covering live second `at`.
fn voice_at(windows: &[&SpeakerWindow], at: f64) -> Option<u32> {
    let s = sample(at);
    windows
        .iter()
        .rev()
        .flat_map(|w| w.spans.iter())
        .find(|span| span.from_sample <= s && s < span.to_sample && !span.overlap)
        .map(|span| span.voice)
}

#[test]
fn record_live_worker_chain_links_voices_by_overlap_only_and_stays_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let mut journal = journal(tmp.path());
    let dir = journal.dir().to_owned();
    let mut driver = Driver::new(&start(&dir)).unwrap();
    let embedder = ToneEmbedder::new("tone", 16);
    let mut ev = evidence(&[], None);
    let mut events = Vec::new();
    // A 0–12 s, B 12–20 s, silence 20–40 s, A again 40–50 s.
    let end = feed(
        &mut journal,
        0.0,
        &[(200.0, 12.0), (440.0, 8.0), (0.0, 20.0), (200.0, 10.0)],
        &mut driver,
        &mut ev,
        &embedder,
        &mut events,
    );
    journal.finish((end * 1e9) as u64, "user").unwrap();
    let (more, last) = drain(&mut driver, &mut ev, &embedder);
    events.extend(more);
    assert!(last.finished);
    let ws = windows(&events);
    // One window per 5 s hop, never a full-history pass.
    assert_eq!(ws.len(), 10);
    assert!(
        ws.iter()
            .all(|w| w.to_sample - w.from_sample <= sample(10.0))
    );
    assert!(
        ws.iter()
            .all(|w| w.epoch == 1 && w.evidence == "evidence-1")
    );
    let a1 = voice_at(&ws, 3.0).unwrap();
    assert_eq!(voice_at(&ws, 11.0), Some(a1), "A continues across windows");
    let b = voice_at(&ws, 16.0).unwrap();
    assert_ne!(a1, b);
    // After 20 s of silence no window overlap links A again: a new voice,
    // never a person-threshold reuse.
    let a2 = voice_at(&ws, 45.0).unwrap();
    assert_ne!(a2, a1);
    assert_ne!(a2, b);
    // Journal anchors on every span; window-local labels never leak.
    assert!(
        ws.iter()
            .flat_map(|w| &w.spans)
            .all(|s| s.from.is_some() && s.to.is_some())
    );
    // Bounded: at most `retain_s` of audio per track.
    assert!(driver.retained()[1] <= sample(60.0) as usize);
    assert_eq!(driver.retained()[0], 0);
    // No suggestions without evidence.
    assert!(
        ws.iter()
            .flat_map(|w| &w.voices)
            .all(|v| v.person_id.is_none())
    );
}

#[test]
fn record_live_lagging_worker_skips_backlog_with_a_gap() {
    let tmp = tempfile::tempdir().unwrap();
    let mut journal = journal(tmp.path());
    // 60 s closed before the worker first looks (a restart mid-meeting).
    append(&mut journal, 0.0, &[(200.0, 60.0)]);
    let dir = journal.dir().to_owned();
    let mut driver = Driver::new(&start(&dir)).unwrap();
    let embedder = ToneEmbedder::new("tone", 16);
    let mut ev = evidence(&[], None);
    let (events, last) = drain(&mut driver, &mut ev, &embedder);
    assert!(!last.finished, "the session is still recording");
    let gap = events.iter().find_map(|e| match e {
        SpeakersEvent::Gap {
            track,
            from_sample,
            to_sample,
        } => Some((*track, *from_sample, *to_sample)),
        _ => None,
    });
    assert_eq!(gap, Some((Track::System, 0, sample(50.0))));
    let ws = windows(&events);
    assert_eq!(ws.len(), 1, "only the latest window, not the backlog");
    assert!(ws[0].behind);
    assert!(
        sample(60.0) - ws[0].to_sample < 160,
        "ends at the durable audio"
    );
    assert!(driver.retained()[1] <= sample(15.0) as usize);
    assert!(last.pending_s < 1.0);
    // Ten more seconds: two ordinary windows, no longer behind.
    append(&mut journal, 60.0, &[(200.0, 10.0)]);
    let (events, _) = drain(&mut driver, &mut ev, &embedder);
    let ws = windows(&events);
    assert_eq!(ws.len(), 2);
    assert!(ws.iter().all(|w| !w.behind));
}

#[test]
fn record_live_named_turn_becomes_session_evidence_for_later_suggestions_only() {
    let tmp = tempfile::tempdir().unwrap();
    let mut journal = journal(tmp.path());
    let dir = journal.dir().to_owned();
    let person = Library::open(tmp.path().join("library"))
        .unwrap()
        .new_person_id();
    let calibrated = Thresholds {
        suggest: Some(0.9),
        ..Default::default()
    };
    let mut ev = evidence(std::slice::from_ref(&person), Some(calibrated));
    let embedder = ToneEmbedder::new("tone", 16);
    let models = SpeakerModels {
        diarizer: &ToneDiarizer,
        embedder: &embedder,
    };
    let mut driver = Driver::new(&start(&dir)).unwrap();
    let mut events = Vec::new();
    // The acquaintance (200 Hz) talks first, then B (440 Hz).
    feed(
        &mut journal,
        0.0,
        &[(200.0, 12.0), (440.0, 8.0)],
        &mut driver,
        &mut ev,
        &embedder,
        &mut events,
    );
    assert!(
        windows(&events)
            .iter()
            .flat_map(|w| &w.voices)
            .all(|v| v.person_id.is_none())
    );
    // The user names the turn 1–11 s: a transient session sample. Its
    // evidence load and embedding run inside `busy`, so the worker's
    // heartbeat shows a hang there as it does a hung window.
    let mut out = Vec::new();
    let busy_log = std::cell::RefCell::new(Vec::new());
    driver
        .input(
            SpeakersInput::SessionSample {
                turn_id: "system-16000".into(),
                decision: 1,
                forget_fence: never(),
                track: Track::System,
                from_sample: sample(1.0),
                to_sample: sample(11.0),
                person_id: person.as_str().to_owned(),
            },
            &models,
            &mut ev,
            &|on| busy_log.borrow_mut().push(on),
            &mut |e| {
                out.push(e);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        out,
        [SpeakersEvent::SessionSample {
            turn_id: "system-16000".into(),
            used: true
        }]
    );
    assert_eq!(driver.session_samples(), 1);
    assert_eq!(busy_log.into_inner(), [true, false]);
    // A mixed range (A and B) never becomes a sample.
    let mut out = Vec::new();
    driver
        .input(
            SpeakersInput::SessionSample {
                turn_id: "system-160000".into(),
                decision: 2,
                forget_fence: never(),
                track: Track::System,
                from_sample: sample(10.0),
                to_sample: sample(15.0),
                person_id: person.as_str().to_owned(),
            },
            &models,
            &mut ev,
            &|_| {},
            &mut |e| {
                out.push(e);
                Ok(())
            },
        )
        .unwrap();
    assert!(matches!(
        out.as_slice(),
        [SpeakersEvent::SessionSample { used: false, .. }]
    ));
    assert_eq!(driver.session_samples(), 1);
    // Later, after silence, the same voice returns as a new provisional
    // voice: suggested (from this session), never named.
    let mut events = Vec::new();
    feed(
        &mut journal,
        20.0,
        &[(0.0, 20.0), (200.0, 10.0)],
        &mut driver,
        &mut ev,
        &embedder,
        &mut events,
    );
    journal.finish(50_000_000_000, "user").unwrap();
    events.extend(drain(&mut driver, &mut ev, &embedder).0);
    let ws = windows(&events);
    let late = ws.last().unwrap();
    let voice = late.voices.iter().find(|v| v.clean_s > 0.0).unwrap();
    assert_eq!(voice.person_id.as_deref(), Some(person.as_str()));
    assert_eq!(voice.source, Some(SuggestionSource::Session));
    // B's voice was never suggested as the named person (calibrated
    // `suggest` keeps unrelated voices out).
    assert!(
        ws.iter()
            .flat_map(|w| &w.voices)
            .filter(|v| v.person_id.is_some())
            .all(|v| v.voice == voice.voice)
    );
    // Revoking (Clear, decision 3) removes the session evidence.
    let revoke = |decision| SpeakersInput::RevokeSessionSample {
        turn_id: "system-16000".into(),
        decision,
    };
    let name = |decision| SpeakersInput::SessionSample {
        turn_id: "system-16000".into(),
        decision,
        forget_fence: never(),
        track: Track::System,
        from_sample: sample(1.0),
        to_sample: sample(11.0),
        person_id: person.as_str().to_owned(),
    };
    let ignore: &mut dyn FnMut(SpeakersEvent) -> broscribe_core::Result<()> = &mut |_| Ok(());
    let idle = &|_: bool| {};
    driver
        .input(revoke(3), &models, &mut ev, idle, ignore)
        .unwrap();
    assert_eq!(driver.session_samples(), 0);
    // Clear (5) delivered before an older, delayed Name (4): the Name
    // never restores the evidence.
    driver
        .input(revoke(5), &models, &mut ev, idle, ignore)
        .unwrap();
    let mut out = Vec::new();
    driver
        .input(name(4), &models, &mut ev, idle, &mut |e| {
            out.push(e);
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        out.as_slice(),
        [SpeakersEvent::SessionSample { used: false, .. }]
    ));
    assert_eq!(driver.session_samples(), 0);
    // A newer decision on that turn still applies.
    driver
        .input(name(6), &models, &mut ev, idle, ignore)
        .unwrap();
    assert_eq!(driver.session_samples(), 1);
    driver
        .input(revoke(6), &models, &mut ev, idle, ignore)
        .unwrap();
    assert_eq!(driver.session_samples(), 1, "same decision again: ignored");
}

/// The forget fence of someone never forgotten.
fn never() -> Option<String> {
    Some(broscribe_core::live_identity::NEVER_FORGOTTEN.into())
}

#[test]
fn record_live_session_pool_drops_forgotten_people_and_voices() {
    let mut pool = SessionPool::new(2, 200);
    let lib_dir = tempfile::tempdir().unwrap();
    let lib = Library::open(lib_dir.path()).unwrap();
    let (ana, bia) = (lib.new_person_id(), lib.new_person_id());
    let fence = |f: &str| f.to_owned();
    pool.add("t1", ana.clone(), vec![1.0, 0.0], fence("never"));
    pool.add("t2", bia.clone(), vec![0.0, 1.0], fence("never"));
    pool.add("t3", bia.clone(), vec![0.0, 1.0], fence("never"));
    assert_eq!(pool.len(), 2, "bounded");
    let mut evidence = Evidence {
        person_ids: [ana.clone()].into_iter().collect(),
        ..Evidence::default()
    };
    // Bia was forgotten.
    pool.prune(&evidence);
    assert!(pool.is_empty());
    // A sample decided before Ana's voice was forgotten, one after.
    pool.add("t4", ana.clone(), vec![1.0, 0.0], fence("never"));
    pool.add("t5", ana.clone(), vec![1.0, 0.0], fence("forget-1"));
    evidence
        .forget_fences
        .insert(ana.clone(), fence("forget-1"));
    assert!(!SessionPool::allowed(&evidence, &ana, "never"));
    assert!(SessionPool::allowed(&evidence, &ana, "forget-1"));
    assert!(!SessionPool::allowed(&evidence, &bia, "never"));
    pool.prune(&evidence);
    assert_eq!(pool.len(), 1);
    // Any later forget invalidates every older fence; her person stays.
    evidence
        .forget_fences
        .insert(ana.clone(), fence("forget-2"));
    pool.prune(&evidence);
    assert!(pool.is_empty());
    assert!(evidence.person_ids.contains(&ana));
}

#[test]
fn record_live_decision_order_is_bounded_and_never_reopens_evicted_turns() {
    // Order is kept for two turns.
    let mut pool = SessionPool::new(4, 2);
    assert!(pool.admit("t1", 1));
    assert!(pool.admit("t2", 2));
    assert!(!pool.admit("t2", 2), "same decision");
    assert!(!pool.admit("t2", 1), "older decision");
    // t3 evicts t1: a delayed update of t1 is refused.
    assert!(pool.admit("t3", 3));
    assert!(!pool.admit("t1", 1));
    // Active turns keep their own order.
    assert!(!pool.admit("t3", 2));
    assert!(pool.admit("t2", 4));
    // t4 evicts the earliest-tracked turn, t2 (last decision 4), raising
    // the floor to 4: t3 keeps its own order, t2's delayed update stays
    // refused, and only a newer decision on it is admitted.
    assert!(pool.admit("t4", 5));
    assert!(!pool.admit("t3", 3));
    assert!(!pool.admit("t2", 4));
    assert!(!pool.admit("t1", 3), "below the floor");
    assert!(pool.admit("t2", 6));
    // A genuinely newer decision is never mistaken for a stale one.
    assert!(pool.admit("t5", 7));
}

#[test]
fn record_live_forget_voice_ends_suggestions_from_session_samples() {
    use broscribe_core::live_identity::forget_fence;
    use broscribe_core::ops;
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path().join("library")).unwrap();
    // A name-only person: no stored voice, only what this session learns.
    let ana = broscribe_core::schema::Person::new(lib.new_person_id(), "Ana Session");
    lib.write(|tx| tx.save_person(&ana)).unwrap();
    let mut evidence = LibraryEvidence::new(lib.clone(), None, "tone");
    let before = evidence.current().unwrap().fingerprint.clone();
    let embedder = ToneEmbedder::new("tone", 16);
    let models = SpeakerModels {
        diarizer: &ToneDiarizer,
        embedder: &embedder,
    };
    let mut journal = journal(tmp.path());
    let dir = journal.dir().to_owned();
    let mut driver = Driver::new(&start(&dir)).unwrap();
    let mut events = Vec::new();
    feed(
        &mut journal,
        0.0,
        &[(200.0, 12.0), (440.0, 8.0)],
        &mut driver,
        &mut evidence,
        &embedder,
        &mut events,
    );
    // The fence the daemon reads in each decision's locked commit.
    let fence = || Some(forget_fence(&lib, ana.id.as_str()).unwrap());
    let name = |turn: &str, decision, forget_fence| SpeakersInput::SessionSample {
        turn_id: turn.into(),
        decision,
        forget_fence,
        track: Track::System,
        from_sample: sample(1.0),
        to_sample: sample(11.0),
        person_id: ana.id.as_str().to_owned(),
    };
    let used = |driver: &mut Driver, evidence: &mut LibraryEvidence, input| {
        let mut out = Vec::new();
        driver
            .input(input, &models, evidence, &|_| {}, &mut |e| {
                out.push(e);
                Ok(())
            })
            .unwrap();
        matches!(
            out.as_slice(),
            [SpeakersEvent::SessionSample { used: true, .. }]
        )
    };
    let before_forget = fence();
    assert_eq!(before_forget, never());
    assert!(used(
        &mut driver,
        &mut evidence,
        name("system-16000", 1, before_forget.clone())
    ));
    assert_eq!(driver.session_samples(), 1);
    // Forget Ana's voice: she keeps her name (and attendance), and nothing
    // she said here may suggest her again.
    let forget = || {
        ops::with_index(&lib, |tx, index| {
            ops::forget_voice(tx, index, ana.id.as_str())
        })
        .unwrap()
    };
    forget();
    assert!(lib.load_person(&ana.id).is_ok());
    assert_ne!(evidence.current().unwrap().fingerprint, before);
    let after_forget = fence();
    assert_ne!(after_forget, before_forget);
    let mut events = Vec::new();
    feed(
        &mut journal,
        20.0,
        &[(0.0, 20.0), (200.0, 10.0)],
        &mut driver,
        &mut evidence,
        &embedder,
        &mut events,
    );
    assert_eq!(driver.session_samples(), 0);
    assert!(
        windows(&events)
            .iter()
            .flat_map(|w| &w.voices)
            .all(|v| v.person_id.is_none())
    );
    // A Name committed before the forget but delivered only now, after
    // the prune, still brings no evidence back; nor does one without a
    // fence (unknown).
    assert!(!used(
        &mut driver,
        &mut evidence,
        name("late-turn", 2, before_forget.clone())
    ));
    assert!(!used(&mut driver, &mut evidence, name("no-fence", 3, None)));
    assert_eq!(driver.session_samples(), 0);
    // A Name committed after the forget carries the new fence: accepted.
    assert!(used(
        &mut driver,
        &mut evidence,
        name("after-forget", 4, after_forget.clone())
    ));
    assert_eq!(driver.session_samples(), 1);
    // Another forget, whatever its timestamp, retires that fence too: the
    // next input drops the sample and refuses the old fence.
    forget();
    assert!(!used(
        &mut driver,
        &mut evidence,
        name("stale-fence", 5, after_forget)
    ));
    assert_eq!(driver.session_samples(), 0);
    assert!(used(&mut driver, &mut evidence, name("newest", 6, fence())));
}

fn segment(id: u32, start: f64, end: f64, text: &str) -> Segment {
    Segment {
        speech_overlap: Vec::new(),
        id,
        track: "system".into(),
        speaker: None,
        start,
        end,
        text: text.into(),
        words: Vec::new(),
    }
}

fn decide(
    lib: &Library,
    turn: &str,
    revision: u64,
    action: LiveAction,
    person: Option<&PersonId>,
    from_s: f64,
    to_s: f64,
) {
    lib.write(|tx| {
        record_decision(
            tx,
            "live-s1",
            NewDecision {
                turn_id: turn.into(),
                epoch: 1,
                turn_revision: revision,
                action,
                person_id: person.cloned(),
                range: LiveRange {
                    track: "system".into(),
                    from_sample: sample(from_s),
                    to_sample: sample(to_s),
                    from: None,
                    to: None,
                },
            },
        )
        .map(|_| ())
    })
    .unwrap();
}

fn outcome(meeting: &Meeting, turn: &str) -> LiveOutcome {
    meeting
        .live_attributions
        .iter()
        .find(|a| a.turn_id == turn)
        .unwrap()
        .outcome
}

/// Line attributions as (segment start, person, live decision?).
fn lines(meeting: &Meeting) -> Vec<(f64, PersonId, bool)> {
    meeting
        .utterance_people
        .iter()
        .map(|u| (u.start, u.person_id.clone(), u.live_decision.is_some()))
        .collect()
}

#[test]
fn record_live_final_reconcile_applies_only_lines_inside_reviewed_ranges() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path().join("library")).unwrap();
    let mut people = Vec::new();
    lib.write(|tx| {
        for name in ["Ana Live", "Bia Live", "Cris Live"] {
            let p = broscribe_core::schema::Person::new(tx.library().new_person_id(), name);
            tx.save_person(&p)?;
            people.push(p.id);
        }
        Ok(())
    })
    .unwrap();
    let (ana, bia, cris) = (&people[0], &people[1], &people[2]);
    // Journal timeline of 70 s (anchors absent: recording time).
    let mut journal = journal(tmp.path());
    append(&mut journal, 0.0, &[(200.0, 70.0)]);
    let dir = journal.dir().to_owned();
    let manifest = journal.finish(70_000_000_000, "user").unwrap();
    let timeline = Timeline::plan(
        &dir,
        &manifest,
        &TimelineOptions {
            sample_rate: Some(16_000),
            ..Default::default()
        },
    )
    .unwrap();
    // Two touching live turns of Ana cover final line 0 (2–9 s) exactly.
    decide(&lib, "a1", 1, LiveAction::Name, Some(ana), 2.0, 5.0);
    decide(&lib, "a2", 1, LiveAction::Name, Some(ana), 5.0, 9.0);
    // Bia covers only half of line 1 (10–20 s): partial.
    decide(&lib, "b1", 1, LiveAction::Name, Some(bia), 10.0, 15.0);
    // Cris and Bia both claim line 2 (22–26 s): conflicting.
    decide(&lib, "c1", 1, LiveAction::Confirm, Some(cris), 21.8, 26.2);
    decide(&lib, "c2", 1, LiveAction::Name, Some(bia), 22.0, 26.0);
    // Named then cleared: stays cleared.
    decide(&lib, "d1", 1, LiveAction::Name, Some(ana), 30.0, 34.0);
    decide(&lib, "d1", 2, LiveAction::Clear, None, 30.0, 34.0);
    // Ana's exact line 40–44 s; the adjacent 44–48 s line is another voice
    // nobody named.
    decide(&lib, "f1", 1, LiveAction::Name, Some(ana), 40.0, 44.0);
    // 0.2 s of the final line 50–54.2 s lies outside the reviewed range.
    decide(&lib, "g1", 1, LiveAction::Name, Some(ana), 50.0, 54.0);
    // Silence: no final line.
    decide(&lib, "e1", 1, LiveAction::Name, Some(cris), 60.0, 62.0);
    assert_eq!(
        read_sidecar(lib.root(), "live-s1")
            .unwrap()
            .unwrap()
            .decisions
            .len(),
        9
    );

    let mut meeting = Meeting::new(
        lib.new_meeting_id().unwrap(),
        Source::Recording,
        schema::now(),
    );
    meeting.segments = vec![
        segment(0, 2.0, 9.0, "first line"),
        segment(1, 10.0, 20.0, "second line"),
        segment(2, 22.0, 26.0, "third line"),
        segment(3, 30.0, 34.0, "fourth line"),
        segment(4, 40.0, 44.0, "ana line"),
        segment(5, 44.0, 48.0, "other voice"),
        segment(6, 50.0, 54.2, "slightly longer"),
    ];
    let speakers_before = meeting.speakers.clone();
    lib.write(|tx| apply_session_decisions(tx, &mut meeting, "live-s1", &timeline))
        .unwrap();
    assert_eq!(outcome(&meeting, "a1"), LiveOutcome::Applied);
    assert_eq!(outcome(&meeting, "a2"), LiveOutcome::Applied);
    assert_eq!(outcome(&meeting, "b1"), LiveOutcome::Partial);
    assert_eq!(outcome(&meeting, "c1"), LiveOutcome::Conflicting);
    assert_eq!(outcome(&meeting, "c2"), LiveOutcome::Conflicting);
    assert_eq!(outcome(&meeting, "d1"), LiveOutcome::Cleared);
    assert_eq!(outcome(&meeting, "f1"), LiveOutcome::Applied);
    assert_eq!(
        outcome(&meeting, "g1"),
        LiveOutcome::Partial,
        "no expansion"
    );
    assert_eq!(outcome(&meeting, "e1"), LiveOutcome::Unmapped);
    // Only lines 0 and 4, once each, marked as live-created; the adjacent
    // other voice is not named; speakers untouched.
    assert_eq!(
        lines(&meeting),
        [(2.0, ana.clone(), true), (40.0, ana.clone(), true)]
    );
    assert_eq!(meeting.speakers, speakers_before);
    let partial = meeting
        .live_attributions
        .iter()
        .find(|a| a.turn_id == "b1")
        .unwrap();
    assert_eq!(partial.supported, vec![[10.0, 15.0]]);
    let notes = meeting
        .review_notes
        .iter()
        .filter(|n| n.source == ReviewSource::LivePreview)
        .count();
    assert_eq!(notes, 5, "two partial, two conflicting, one unmapped");
    // The same five as structured records, in decision order, each with
    // the audio range and the final lines an explicit correction targets.
    let items = broscribe_core::live_identity::live_review_items(&meeting);
    assert_eq!(
        items.iter().map(|i| i.turn_id.as_str()).collect::<Vec<_>>(),
        ["b1", "c1", "c2", "g1", "e1"]
    );
    let b1 = &items[0];
    assert_eq!(b1.outcome, LiveOutcome::Partial);
    assert!(b1.mapped && b1.track == "system");
    assert_eq!(b1.lines.len(), 1);
    assert_eq!(b1.lines[0].start, 10.0);
    assert!(
        items[4].lines.is_empty(),
        "unmapped: silence, no final line"
    );
    // Idempotent: applying again changes nothing.
    let once = meeting.clone();
    lib.write(|tx| apply_session_decisions(tx, &mut meeting, "live-s1", &timeline))
        .unwrap();
    assert_eq!(meeting.utterance_people, once.utterance_people);
    assert_eq!(meeting.live_attributions, once.live_attributions);

    // The user's own correction of line 4 to Bia (as `set_utterance_people`
    // leaves it): the live line goes, the decision is overridden.
    meeting.utterance_people.retain(|u| u.start != 40.0);
    meeting
        .utterance_people
        .push(broscribe_core::schema::UtterancePerson {
            track: "system".into(),
            start: 40.0,
            end: 44.0,
            text_sha256: broscribe_core::schema::sha256_hex(b"ana line"),
            person_id: bia.clone(),
            at: schema::now(),
            live_decision: None,
        });
    for a in &mut meeting.live_attributions {
        if a.turn_id == "f1" {
            a.user_override = true;
        }
    }

    // Reprocessing: line 0 now starts 0.5 s before Ana's reviewed range
    // (same text, so the line correction moves with it); line 1 is split
    // at 15 s.
    meeting.segments = vec![
        segment(0, 1.5, 9.0, "first line"),
        segment(1, 10.0, 15.0, "second"),
        segment(7, 15.2, 20.0, "line"),
        segment(2, 22.0, 26.0, "third line"),
        segment(3, 30.0, 34.0, "fourth line"),
        segment(4, 40.0, 44.0, "ana line"),
        segment(5, 44.0, 48.0, "other voice"),
        segment(6, 50.0, 54.2, "slightly longer"),
    ];
    reapply_utterance_people(&mut meeting);
    recheck_meeting(&lib, &mut meeting).unwrap();
    // Applied → partial: exactly its live line is retracted.
    assert_eq!(outcome(&meeting, "a1"), LiveOutcome::Partial);
    assert_eq!(outcome(&meeting, "a2"), LiveOutcome::Partial);
    // Bia's half of line 1 is now a whole line inside her range.
    assert_eq!(outcome(&meeting, "b1"), LiveOutcome::Applied);
    // The user's correction stands; the overridden decision adds nothing.
    assert_eq!(outcome(&meeting, "f1"), LiveOutcome::Conflicting);
    assert_eq!(
        lines(&meeting),
        [(10.0, bia.clone(), true), (40.0, bia.clone(), false)]
    );
    // Forgetting Bia: her records read as forgotten, the sidecar too, and
    // her live line is retracted on the next check.
    assert!(broscribe_core::live_identity::forget_person_in_meeting(
        &mut meeting,
        bia
    ));
    assert_eq!(outcome(&meeting, "b1"), LiveOutcome::Forgotten);
    lib.write(|tx| broscribe_core::live_identity::forget_person_in_sidecars(tx, bia))
        .unwrap();
    let sidecar = read_sidecar(lib.root(), "live-s1").unwrap().unwrap();
    assert!(
        sidecar
            .decisions
            .iter()
            .all(|d| d.person_id.as_ref() != Some(bia))
    );
    recheck_meeting(&lib, &mut meeting).unwrap();
    assert_eq!(outcome(&meeting, "b1"), LiveOutcome::Forgotten);
    assert_eq!(outcome(&meeting, "d1"), LiveOutcome::Cleared);
    // Cris's claim on line 2 conflicted with Bia's; forgetting Bia withdraws
    // nothing the user said there, so it promotes nobody.
    assert_eq!(outcome(&meeting, "c1"), LiveOutcome::Conflicting);
    assert!(
        meeting
            .utterance_people
            .iter()
            .all(|u| u.live_decision.is_none()),
        "no forgotten or cleared decision keeps or regains a line"
    );
}

#[test]
fn record_live_anchors_map_through_the_timeline() {
    let tmp = tempfile::tempdir().unwrap();
    let mut journal = journal(tmp.path());
    append(&mut journal, 0.0, &[(200.0, 12.0)]);
    let dir = journal.dir().to_owned();
    let manifest = journal.finish(12_000_000_000, "user").unwrap();
    let timeline = Timeline::plan(
        &dir,
        &manifest,
        &TimelineOptions {
            sample_rate: Some(16_000),
            ..Default::default()
        },
    )
    .unwrap();
    let placed = placements(&manifest, CaptureTrack::System);
    let from = journal_point(&placed, sample(6.5)).unwrap();
    let to = journal_point(&placed, sample(8.0)).unwrap();
    assert_eq!(from.seq, 1);
    let range = LiveRange {
        track: "system".into(),
        from_sample: sample(6.5),
        to_sample: sample(8.0),
        from: Some(from),
        to: Some(to),
    };
    let (a, b) = map_range(&timeline, &range).unwrap();
    assert!((a - 6.5).abs() < 1e-3 && (b - 8.0).abs() < 1e-3, "{a} {b}");
    // A chunk not on the timeline (cut off) does not map.
    let cut = Timeline::plan(
        &dir,
        &manifest,
        &TimelineOptions {
            sample_rate: Some(16_000),
            cutoff: Some(broscribe_capture::journal::Cutoff {
                mic: None,
                system: Some(0),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(map_range(&cut, &range), None);
}

#[test]
fn record_live_tail_reads_closed_chunks_only_and_caps_memory() {
    let tmp = tempfile::tempdir().unwrap();
    let mut journal = journal(tmp.path());
    append(&mut journal, 0.0, &[(200.0, 12.0)]);
    let dir = journal.dir().to_owned();
    let manifest = broscribe_capture::journal::load_manifest(&dir).unwrap();
    let placed = placements(&manifest, CaptureTrack::System);
    // Two closed 5 s chunks; the open third is not durable yet.
    assert_eq!(placed.len(), 2);
    let mut tail = TrackTail::new(CaptureTrack::System, sample(6.0) as usize);
    let polled = tail.poll(&dir, &placed, 0).unwrap();
    assert_eq!(polled.missing, 0);
    assert!(tail.retained() <= sample(6.0) as usize);
    assert!(tail.end() <= sample(10.0) && tail.end() >= sample(10.0) - 64);
    assert!(tail.slice(sample(0.0), sample(1.0)).is_none(), "trimmed");
    assert!(tail.caught_up(&placed));
    // Nothing new: a second poll reads nothing.
    assert_eq!(tail.poll(&dir, &placed, 0).unwrap().appended, 0);
}

#[test]
fn record_live_replay_anchors_round_trip_through_pause_and_final_timeline() {
    let tmp = tempfile::tempdir().unwrap();
    // 30 s: A, B, A at the device rate.
    let mut pcm = tone(200.0, 12.0);
    pcm.extend(tone(440.0, 8.0));
    pcm.extend(tone(200.0, 10.0));
    let embedder = ToneEmbedder::new("tone", 16);
    let models = SpeakerModels {
        diarizer: &ToneDiarizer,
        embedder: &embedder,
    };
    let mut opts = ReplayOptions::new(tmp.path().join("journal"), DEVICE_RATE);
    opts.pause = Some((12.0, 3.0));
    let mut events = 0;
    let summary = replay(&pcm, &opts, &models, &mut evidence(&[], None), &mut |e| {
        assert!(!e.to_line().unwrap().contains("embedding"));
        events += 1;
        Ok(())
    })
    .unwrap();
    assert!(events > 0);
    assert!((summary.audio_s - 30.0).abs() < 1e-9);
    assert_eq!(summary.windows, 6, "one per 5 s hop");
    assert_eq!(summary.gaps, 0);
    assert!(summary.retained_max_samples <= sample(60.0) as usize);
    assert!(summary.span_anchor_checks > 0 && summary.turn_anchor_checks >= 29);
    assert_eq!(summary.turns_unanchored, 0);
    // The live clock and the final timeline agree on every journal frame
    // (pause removed by both) up to rounding.
    assert!(summary.turn_live_vs_final_max_ms < 0.2, "{summary:?}");
    assert!(summary.span_live_vs_final_max_ms < 0.2, "{summary:?}");
    assert!(summary.anchor_roundtrip_max_samples <= 1);

    // A drifting device clock: the anchors still name journal frames; the
    // live/final difference is reported, not hidden.
    let mut drift = ReplayOptions::new(tmp.path().join("drift"), DEVICE_RATE);
    drift.drift_ppm = 300.0;
    let summary = replay(&pcm, &drift, &models, &mut evidence(&[], None), &mut |_| {
        Ok(())
    })
    .unwrap();
    assert_eq!(summary.turns_unanchored, 0);
    assert!(summary.anchor_roundtrip_max_samples <= 1);
    assert!(summary.turn_live_vs_final_max_ms < 50.0, "{summary:?}");
}

#[test]
fn record_live_replay_refuses_non_finite_or_backward_options() {
    let tmp = tempfile::tempdir().unwrap();
    let embedder = ToneEmbedder::new("tone", 16);
    let models = SpeakerModels {
        diarizer: &ToneDiarizer,
        embedder: &embedder,
    };
    let pcm = tone(200.0, 1.0);
    let cases: [fn(&mut ReplayOptions); 9] = [
        |o| o.block_s = f64::NAN,
        |o| o.block_s = 0.0,
        |o| o.turn_s = f64::INFINITY,
        |o| o.chunk_s = -1.0,
        |o| o.pace = f64::NAN,
        |o| o.pace = -1.0,
        |o| o.drift_ppm = -1e6,
        |o| o.drift_ppm = f64::NAN,
        |o| o.pause = Some((f64::NAN, 1.0)),
    ];
    for (i, set) in cases.iter().enumerate() {
        let mut opts = ReplayOptions::new(tmp.path().join(format!("j{i}")), DEVICE_RATE);
        set(&mut opts);
        let err = replay(&pcm, &opts, &models, &mut evidence(&[], None), &mut |_| {
            Ok(())
        })
        .expect_err("refused");
        assert!(
            matches!(err, broscribe_core::Error::InvalidInput(_)),
            "case {i}: {err:?}"
        );
    }
}

/// Returns the same turns for every window, whatever the audio.
struct ScriptedDiarizer(Vec<DiarSegment>);

impl Diarizer for ScriptedDiarizer {
    fn diarize(&self, _: &[f32]) -> Result<Vec<DiarSegment>> {
        Ok(self.0.clone())
    }
}

/// `seconds` of a 200 Hz tone at the live rate.
fn live_tone(seconds: f64) -> Vec<f32> {
    let n = sample(seconds) as usize;
    (0..n)
        .map(|i| (0.3 * (2.0 * std::f64::consts::PI * 200.0 * i as f64 / 16_000.0).sin()) as f32)
        .collect()
}

/// More clusters than `max_voices`: the excluded cluster gets no voice,
/// but its speech still overlaps the tracked voice's. That stretch is
/// labelled overlapped, never clean, and no session sample is taken over
/// a range that holds it.
#[test]
fn record_live_capped_voices_never_label_excluded_overlap_clean() {
    let diarizer = ScriptedDiarizer(vec![
        DiarSegment {
            start_s: 0.0,
            end_s: 8.0,
            speaker: 0,
        },
        // Less talk: left out by a cap of one voice.
        DiarSegment {
            start_s: 2.0,
            end_s: 4.0,
            speaker: 1,
        },
    ]);
    let embedder = ToneEmbedder::new("tone", 16);
    let models = SpeakerModels {
        diarizer: &diarizer,
        embedder: &embedder,
    };
    let settings = LiveIdentitySettings {
        max_voices: 1,
        ..LiveIdentitySettings::default()
    };
    let mut tracker = VoiceTracker::new();
    let analysed = tracker
        .analyse(
            0,
            &live_tone(8.0),
            &models,
            &Evidence::default(),
            &SessionPool::new(4, 4),
            &settings,
        )
        .unwrap();
    assert_eq!(analysed.voices.len(), 1, "capped");
    let voice = analysed.voices[0].voice;
    assert!(analysed.spans.iter().all(|s| s.voice == voice));
    let excluded = (sample(2.0), sample(4.0));
    for span in &analysed.spans {
        let inside = span.from_sample < excluded.1 && span.to_sample > excluded.0;
        assert_eq!(span.overlap, inside, "{span:?}");
    }
    let overlapped: u64 = analysed
        .spans
        .iter()
        .filter(|s| s.overlap)
        .map(|s| s.to_sample - s.from_sample)
        .sum();
    assert_eq!(overlapped, sample(2.0));
    // The embedded clean speech is clean of the excluded cluster too.
    assert!((analysed.voices[0].clean_s - 4.0).abs() < 1e-6);
    // A named range with the excluded speaker in it is no sample; one of
    // the tracked voice alone is.
    assert_eq!(tracker.clean_for(sample(1.0), sample(5.0)), None);
    assert_eq!(tracker.clean_for(sample(2.5), sample(3.5)), None);
    let clean = tracker.clean_for(sample(4.5), sample(8.0)).unwrap();
    assert!((crate::ranges::total(&clean) - 3.5).abs() < 1e-3);
}

/// A thresholds change in the configuration reaches the next window's
/// suggestions even though no voice evidence changed; unchanged evidence
/// is served from the cache; a forget is never served stale.
#[test]
fn record_live_evidence_reloads_on_thresholds_change_only_when_needed() {
    use broscribe_core::config::Config;
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path().join("library")).unwrap();
    let data = tmp.path().join("data");
    let set = |suggest: f32| {
        let mut config = Config::load(&data).unwrap_or_default();
        config.identity.insert(
            "tone".into(),
            Thresholds {
                suggest: Some(suggest),
                ..Default::default()
            },
        );
        config.save(&data).unwrap();
    };
    set(0.9);
    let mut evidence = LibraryEvidence::new(lib.clone(), Some(data.clone()), "tone");
    let first = evidence.current().unwrap();
    assert_eq!(first.thresholds.unwrap().suggest, Some(0.9));
    let again = evidence.current().unwrap();
    assert!(Arc::ptr_eq(&first, &again), "unchanged: cached");
    set(0.95);
    let changed = evidence.current().unwrap();
    assert_eq!(
        changed.fingerprint, first.fingerprint,
        "no voice evidence moved"
    );
    assert_eq!(changed.thresholds.unwrap().suggest, Some(0.95));
    assert!(!Arc::ptr_eq(&first, &changed));
}

/// A named turn longer than the embedder's declared input limit (TitaNet's
/// bound) is embedded in parts within it, never refused or sent whole.
#[test]
fn record_live_session_sample_respects_the_embedder_input_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut journal = journal(tmp.path());
    let dir = journal.dir().to_owned();
    let person = Library::open(tmp.path().join("library"))
        .unwrap()
        .new_person_id();
    let mut ev = evidence(std::slice::from_ref(&person), None);
    let mut embedder = ToneEmbedder::new("tone", 16);
    embedder.max_samples = Some(sample(3.0) as usize);
    let models = SpeakerModels {
        diarizer: &ToneDiarizer,
        embedder: &embedder,
    };
    let mut driver = Driver::new(&start(&dir)).unwrap();
    let mut events = Vec::new();
    feed(
        &mut journal,
        0.0,
        &[(200.0, 12.0), (440.0, 8.0)],
        &mut driver,
        &mut ev,
        &embedder,
        &mut events,
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SpeakersEvent::Warning { .. })),
        "{events:?}"
    );
    let mut out = Vec::new();
    driver
        .input(
            SpeakersInput::SessionSample {
                turn_id: "system-16000".into(),
                decision: 1,
                forget_fence: never(),
                track: Track::System,
                from_sample: sample(1.0),
                to_sample: sample(11.0),
                person_id: person.as_str().to_owned(),
            },
            &models,
            &mut ev,
            &|_| {},
            &mut |e| {
                out.push(e);
                Ok(())
            },
        )
        .unwrap();
    assert!(matches!(
        out.as_slice(),
        [SpeakersEvent::SessionSample { used: true, .. }]
    ));
}

/// A hop that would round to (almost) no live samples is refused up front
/// instead of analysing the same window forever.
#[test]
fn record_live_driver_refuses_hops_below_the_minimum() {
    let tmp = tempfile::tempdir().unwrap();
    for hop_s in [1e-6, 0.01, 0.2] {
        let mut s = start(tmp.path());
        s.settings.hop_s = hop_s;
        let err = Driver::new(&s).err().expect("refused");
        assert!(
            matches!(err, broscribe_core::Error::InvalidInput(_)),
            "{hop_s}"
        );
    }
    let mut s = start(tmp.path());
    s.settings.hop_s = broscribe_ipc::live::MIN_HOP_S;
    assert!(Driver::new(&s).is_ok());
}

/// Two adjacent decisions of the same person jointly support one final
/// line. The user clears that line through the public correction: both
/// decisions are overridden, the line never comes back from either on a
/// recheck or a repeated publication, and neither asks for review.
#[test]
fn record_live_public_clear_overrides_every_supporting_decision() {
    use broscribe_core::ops;
    use broscribe_core::schema::{Person, ProcessingStatus};
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path().join("library")).unwrap();
    let ana = Person::new(lib.new_person_id(), "Ana Joint");
    lib.write(|tx| tx.save_person(&ana)).unwrap();
    let mut journal = journal(tmp.path());
    append(&mut journal, 0.0, &[(200.0, 20.0)]);
    let dir = journal.dir().to_owned();
    let manifest = journal.finish(20_000_000_000, "user").unwrap();
    let timeline = Timeline::plan(
        &dir,
        &manifest,
        &TimelineOptions {
            sample_rate: Some(16_000),
            ..Default::default()
        },
    )
    .unwrap();
    decide(&lib, "j1", 1, LiveAction::Name, Some(&ana.id), 2.0, 5.0);
    decide(&lib, "j2", 1, LiveAction::Name, Some(&ana.id), 5.0, 9.0);
    let mut meeting = Meeting::new(
        lib.new_meeting_id().unwrap(),
        Source::Recording,
        schema::now(),
    );
    meeting.processing.status = ProcessingStatus::Done;
    meeting.segments = vec![segment(0, 2.0, 9.0, "joint line")];
    ops::with_index(&lib, |tx, index| {
        apply_session_decisions(tx, &mut meeting, "live-s1", &timeline)?;
        tx.create_meeting_dir(&meeting)?;
        tx.save_meeting(&meeting)?;
        index.upsert_meeting(tx, &meeting)
    })
    .unwrap();
    assert_eq!(lines(&meeting), [(2.0, ana.id.clone(), true)]);
    let line = ops::segment_fingerprint(&meeting.segments[0]);
    let mut cleared = ops::with_index(&lib, |tx, index| {
        ops::set_utterance_people(tx, index, &meeting.id, std::slice::from_ref(&line), None)
    })
    .unwrap();
    assert!(cleared.utterance_people.is_empty());
    assert!(
        cleared.live_attributions.iter().all(|a| a.user_override),
        "every supporting decision is overridden, not only the line's owner"
    );
    recheck_meeting(&lib, &mut cleared).unwrap();
    assert!(cleared.utterance_people.is_empty(), "not resurrected");
    lib.write(|tx| apply_session_decisions(tx, &mut cleared, "live-s1", &timeline))
        .unwrap();
    assert!(
        cleared.utterance_people.is_empty(),
        "nor by a new publication"
    );
    assert!(cleared.live_attributions.iter().all(|a| !a.needs_review()));
    assert!(
        !cleared
            .review_notes
            .iter()
            .any(|n| n.source == ReviewSource::LivePreview)
    );
}

/// PL38: without calibrated thresholds the user's own person is a live
/// suggestion only under the explicit opt-in, and every uncalibrated
/// suggestion says so; calibrated thresholds are untouched by the policy.
#[test]
fn record_live_uncalibrated_me_is_suggested_only_under_the_explicit_policy() {
    use broscribe_core::identity::PersonSamples;
    let lib_dir = tempfile::tempdir().unwrap();
    let lib = Library::open(lib_dir.path()).unwrap();
    let (me, ana) = (lib.new_person_id(), lib.new_person_id());
    let samples = |person: &PersonId, v: Vec<f32>| PersonSamples {
        person_id: person.clone(),
        confirmed: vec![v],
        weak: Vec::new(),
    };
    let mut evidence = Evidence {
        people: vec![samples(&me, vec![1.0, 0.0]), samples(&ana, vec![0.0, 1.0])],
        person_ids: [me.clone(), ana.clone()].into_iter().collect(),
        me: Some(me.clone()),
        ..Evidence::default()
    };
    let session = SessionPool::new(4, 16);
    let suggest = |evidence: &Evidence, centroid: &[f32]| {
        SuggestionPool::new(evidence, &session).suggest(centroid)
    };
    // Off (the default): the closest stored voice being Me suggests no one,
    // and no runner-up stands in for it.
    assert_eq!(suggest(&evidence, &[1.0, 0.0]), None);
    // Someone else stays a suggestion, honestly marked uncalibrated.
    let other = suggest(&evidence, &[0.0, 1.0]).unwrap();
    assert_eq!((other.person, other.uncalibrated), (ana.clone(), true));
    // Opted in: Me is suggested, marked uncalibrated.
    evidence.allow_uncalibrated_me = true;
    let mine = suggest(&evidence, &[1.0, 0.0]).unwrap();
    assert_eq!(
        mine,
        super::tracker::Suggestion {
            person: me.clone(),
            source: SuggestionSource::Library,
            uncalibrated: true,
        }
    );
    // Calibrated: the thresholds alone decide, whatever the policy.
    for allow in [false, true] {
        evidence.allow_uncalibrated_me = allow;
        evidence.thresholds = Some(Thresholds {
            auto_name: Some(0.9),
            suggest: Some(0.5),
            margin: Some(0.1),
            ..Default::default()
        });
        let mine = suggest(&evidence, &[1.0, 0.0]).unwrap();
        assert_eq!((mine.person, mine.uncalibrated), (me.clone(), false));
        assert_eq!(suggest(&evidence, &[-1.0, 0.0]), None, "below suggest");
    }
}

/// The library source reads the policy and (only while uncalibrated) the
/// user's own person on every call, so a policy change or a calibration
/// reaches the next window; the policy stays off unless set.
#[test]
fn record_live_evidence_reads_the_uncalibrated_me_policy() {
    use broscribe_core::config::Config;
    use broscribe_core::schema::Person;
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path().join("library")).unwrap();
    let data = tmp.path().join("data");
    let mut me = Person::new(lib.new_person_id(), "Bruno");
    me.is_self = true;
    lib.write(|tx| tx.save_person(&me)).unwrap();
    let mut evidence = LibraryEvidence::new(lib.clone(), Some(data.clone()), "tone");
    let off = evidence.current().unwrap();
    assert!(!off.allow_uncalibrated_me);
    assert_eq!(off.me.as_ref(), Some(&me.id));
    Config::update(&data, |c| {
        c.live.allow_uncalibrated_me_suggestions = true;
        Ok(())
    })
    .unwrap();
    let on = evidence.current().unwrap();
    assert!(on.allow_uncalibrated_me);
    assert_eq!(on.fingerprint, off.fingerprint, "no voice evidence moved");
    Config::update(&data, |c| {
        c.identity.insert(
            "tone".into(),
            Thresholds {
                suggest: Some(0.9),
                ..Default::default()
            },
        );
        Ok(())
    })
    .unwrap();
    let calibrated = evidence.current().unwrap();
    assert!(super::tracker::calibrated(calibrated.thresholds.as_ref()));
    assert_eq!(calibrated.me, None, "not needed once calibrated");
}
