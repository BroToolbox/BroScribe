use super::metrics::*;
use super::worker::{WORKER_SCHEMA_VERSION, WorkerOptions, WorkerReport, run_worker};
use super::*;
use crate::testutil::Fixture;
use broscribe_core::schema::{Segment, Source, Speaker, Word};
use broscribe_core::{PersonId, SpeakerId};
use broscribe_models::DiarSegment;
use broscribe_models::fakes::FakeModelProvider;
use broscribe_platform::ProcessUsage;
use broscribe_platform::system::FakeResourceUsage;

fn turn(start: f64, end: f64, label: &str) -> LabelledTurn {
    LabelledTurn {
        start,
        end,
        label: label.to_owned(),
    }
}

fn words(s: &str) -> Vec<String> {
    normalize_words(s)
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ---- WER ----

#[test]
fn normalisation_drops_case_and_punctuation_and_keeps_numbers_as_written() {
    assert_eq!(
        words("Hello, World! It's 3.5 percent of 1,000 — well-known."),
        [
            "hello", "world", "its", "3.5", "percent", "of", "1,000", "well", "known"
        ]
    );
    assert_eq!(words("Olá, ÉPOCA"), ["olá", "época"]);
    // Numbers are never spelled out.
    assert_ne!(words("three"), words("3"));
}

#[test]
fn wer_counts_substitutions_deletions_and_insertions() {
    let e = word_errors(&words("a b c d"), &words("a x c d e"));
    assert_eq!(
        (
            e.substitutions,
            e.deletions,
            e.insertions,
            e.reference_words
        ),
        (1, 0, 1, 4)
    );
    assert_eq!(e.wer().get(), Some(0.5));
    let e = word_errors(&words("a b c"), &words(""));
    assert_eq!((e.deletions, e.edits()), (3, 3));
    assert_eq!(e.wer().get(), Some(1.0));
    let e = word_errors(&words("one two"), &words("One, two."));
    assert_eq!(e.wer().get(), Some(0.0));
    // No reference words: unavailable, never zero.
    let e = word_errors(&[], &words("noise"));
    assert!(e.wer().get().is_none());
}

#[test]
fn corpus_wer_sums_edits_over_reference_words() {
    let mut total = WordErrors::default();
    total.add(&word_errors(&words("a b"), &words("a b")));
    total.add(&word_errors(&words("c d e f"), &words("c x e")));
    // 2 edits over 6 words.
    assert!(close(total.wer().get().unwrap(), 2.0 / 6.0));
}

// ---- DER / JER ----

#[test]
fn der_is_zero_for_a_perfect_hypothesis_under_any_labels() {
    let reference = [turn(0.0, 10.0, "A"), turn(10.0, 20.0, "B")];
    let swapped = [turn(0.0, 10.0, "S2"), turn(10.0, 20.0, "S1")];
    for collar in [0.0, COLLAR_S] {
        let e = diarization_errors(&reference, &swapped, (0.0, 20.0), collar);
        assert!(close(e.der().get().unwrap(), 0.0), "{e:?}");
        assert!(close(e.jer().get().unwrap(), 0.0));
    }
}

#[test]
fn der_components_on_synthetic_references() {
    let reference = [turn(0.0, 10.0, "A"), turn(10.0, 20.0, "B")];
    // One cluster for both people: half the speech is confused.
    let one = [turn(0.0, 20.0, "S1")];
    let e = diarization_errors(&reference, &one, (0.0, 20.0), 0.0);
    assert!(close(e.confusion_s, 10.0) && close(e.reference_s, 20.0));
    assert!(close(e.der().get().unwrap(), 0.5));
    // With the 0.25 s collar around 0, 10 and 20: 1 s is not scored, and
    // 9.5 s of each person remain.
    let e = diarization_errors(&reference, &one, (0.0, 20.0), COLLAR_S);
    assert!(close(e.reference_s, 19.0), "{e:?}");
    assert!(close(e.confusion_s, 9.5));
    // Missed speech.
    let half = [turn(0.0, 10.0, "S1")];
    let e = diarization_errors(&reference, &half, (0.0, 20.0), 0.0);
    assert!(close(e.missed_s, 10.0) && close(e.der().get().unwrap(), 0.5));
    // False alarm beyond the reference speech.
    let extra = [
        turn(0.0, 10.0, "S1"),
        turn(10.0, 20.0, "S2"),
        turn(20.0, 25.0, "S3"),
    ];
    let e = diarization_errors(&reference, &extra, (0.0, 25.0), 0.0);
    assert!(close(e.false_alarm_s, 5.0) && close(e.der().get().unwrap(), 0.25));
}

#[test]
fn overlapping_reference_speech_is_scored_per_speaker() {
    let reference = [turn(0.0, 10.0, "A"), turn(5.0, 10.0, "B")];
    let hyp = [turn(0.0, 10.0, "S1")];
    let e = diarization_errors(&reference, &hyp, (0.0, 10.0), 0.0);
    assert!(close(e.reference_s, 15.0));
    assert!(close(e.missed_s, 5.0));
    assert!(close(e.der().get().unwrap(), 1.0 / 3.0));
}

#[test]
fn jer_averages_reference_speakers() {
    let reference = [turn(0.0, 10.0, "A"), turn(10.0, 20.0, "B")];
    let hyp = [turn(0.0, 5.0, "S1"), turn(10.0, 20.0, "S2")];
    let e = diarization_errors(&reference, &hyp, (0.0, 20.0), 0.0);
    // A: 1 - 5/10; B: 0.
    assert_eq!(e.jer_speakers, 2);
    assert!(close(e.jer().get().unwrap(), 0.25));
    // No reference speech: unavailable, never zero.
    let e = diarization_errors(&[], &hyp, (0.0, 20.0), 0.0);
    assert!(e.der().get().is_none() && e.jer().get().is_none());
}

#[test]
fn assignment_maximises_total_overlap() {
    // Greedy would take (0,0)=5 and then (1,1)=1: total 6; optimal 4 + 4.
    let w = vec![vec![5.0, 4.0], vec![4.0, 1.0]];
    assert_eq!(max_assignment(&w), vec![Some(1), Some(0)]);
    // More rows than columns; zero-weight pairs stay unmapped.
    let w = vec![vec![1.0], vec![3.0], vec![0.0]];
    assert_eq!(max_assignment(&w), vec![None, Some(0), None]);
    let w = vec![vec![0.0, 0.0]];
    assert_eq!(max_assignment(&w), vec![None]);
    assert!(max_assignment(&[]).is_empty());
}

#[test]
fn clusters_per_true_speaker_counts_over_segmentation() {
    let reference = [turn(0.0, 10.0, "A"), turn(10.0, 20.0, "B")];
    let hyp = [
        turn(0.0, 5.0, "S1"),
        turn(5.0, 10.0, "S2"),
        turn(10.0, 19.5, "S3"),
        turn(19.5, 20.0, "S4"),
    ];
    let c = clusters_per_speaker(&reference, &hyp, (0.0, 20.0));
    assert_eq!(c.get("A"), Some(&2));
    // S4 overlaps B by under a second.
    assert_eq!(c.get("B"), Some(&1));
}

#[test]
fn naming_accuracy_and_estimated_fixes() {
    let ana = "p-00000a";
    let reference = [
        turn(0.0, 10.0, ana),
        turn(10.0, 20.0, "hint:bo"),
        turn(20.0, 30.0, ana),
    ];
    let hyp = [
        turn(0.0, 10.0, "S1"),
        turn(10.0, 20.0, "S2"),
        turn(20.0, 30.0, "S3"),
    ];
    let id = |label: &str, assignment, person: Option<&str>, suggested: Option<&str>| HypIdentity {
        label: label.into(),
        assignment,
        person: person.map(Into::into),
        suggested: suggested.map(Into::into),
    };
    let identities = [
        id("S1", Assignment::Auto, Some(ana), None),
        // Someone outside the library left unnamed: right, but still a fix.
        id("S2", Assignment::None, None, None),
        // Over-split cluster wrongly suggested.
        id("S3", Assignment::Suggested, None, Some("p-00000b")),
    ];
    let known = |l: &str| l == ana;
    let out = naming(&reference, &hyp, &identities, &known, (0.0, 30.0));
    assert!(close(out.reference_s, 30.0));
    // S1 right; S2 right (not named); S3 not named while Ana is known.
    assert!(close(out.correct_s, 20.0));
    assert_eq!((out.speakers, out.fixes), (3, 2));
}

// ---- unavailable never zero ----

#[test]
fn unavailable_metrics_serialise_with_a_reason_and_never_as_zero() {
    let m = Metric::unavailable("no reference text");
    let v = serde_json::to_value(&m).unwrap();
    assert_eq!(v, serde_json::json!({ "unavailable": "no reference text" }));
    assert!(v.get("value").is_none());
    let back: Metric = serde_json::from_value(v).unwrap();
    assert_eq!(back, m);
    assert!(Metric::ratio(0.0, 0.0, "x", "none").get().is_none());
    let v = serde_json::to_value(Metric::value(0.0, 3.0, "reference words")).unwrap();
    assert_eq!(v["value"], 0.0);
    assert_eq!(v["of"], 3.0);
}

// ---- set, candidates ----

fn data_dir(fx: &Fixture) -> PathBuf {
    let d = fx._tmp.path().join("data");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn sid(n: u32) -> SpeakerId {
    SpeakerId::new(n).unwrap()
}

fn seg(id: u32, speaker: u32, start: f64, text: &[&str]) -> Segment {
    let words: Vec<Word> = text
        .iter()
        .enumerate()
        .map(|(i, w)| Word {
            text: (*w).to_owned(),
            start: start + i as f64,
            end: start + i as f64 + 0.8,
        })
        .collect();
    Segment {
        speech_overlap: Vec::new(),
        id,
        track: "t0".into(),
        speaker: Some(sid(speaker)),
        start,
        end: words.last().map_or(start, |w| w.end),
        text: text.join(" "),
        words,
    }
}

#[test]
fn excerpt_add_copies_text_and_confirmed_turns_only() {
    let fx = Fixture::new();
    let data = data_dir(&fx);
    let ana = PersonId::parse("p-00000a").unwrap();
    let mut m = Meeting::new(
        MeetingId::parse("m-0000aa").unwrap(),
        Source::Import,
        schema::now(),
    );
    m.duration_s = 20.0;
    let mut s1 = Speaker::new(sid(1), "t0");
    s1.assignment = Assignment::Confirmed;
    s1.person_id = Some(ana.clone());
    m.speakers = vec![s1, Speaker::new(sid(2), "t0")];
    m.segments = vec![
        seg(0, 1, 0.0, &["alpha", "beta", "gamma"]),
        seg(1, 1, 3.0, &["delta"]),
        seg(2, 2, 10.0, &["epsilon", "zeta"]),
    ];
    fx.lib
        .write(|tx| {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)
        })
        .unwrap();

    let added = add_excerpt(&fx.env(), &data, "s", &m.id, 1.0, 5.0).unwrap();
    assert_eq!(added.excerpt_id, "e1");
    assert_eq!(added.reference_words, 3);
    assert_eq!(added.reference_turns, Some(1));
    let set = load_set(&data, "s").unwrap();
    let e = &set.excerpts[0];
    assert_eq!(e.reference_text.as_deref(), Some("beta gamma delta"));
    assert!(!e.reference_checked);
    assert_eq!(e.turns_source, Some(ReferenceSource::Confirmed));
    let turns = e.reference_turns.as_ref().unwrap();
    assert_eq!(turns[0].label, ana.to_string());
    assert!(close(turns[0].start, 1.0) && close(turns[0].end, 3.8));

    // Unconfirmed speech in the range: no reference turns.
    let added = add_excerpt(&fx.env(), &data, "s", &m.id, 2.0, 12.0).unwrap();
    assert_eq!(added.excerpt_id, "e2");
    assert_eq!(added.reference_turns, None);
    assert_eq!(load_set(&data, "s").unwrap().excerpts.len(), 2);

    assert!(add_excerpt(&fx.env(), &data, "../x", &m.id, 0.0, 1.0).is_err());
    assert!(add_excerpt(&fx.env(), &data, "s", &m.id, 5.0, 1.0).is_err());
}

#[test]
fn candidate_specs_replace_models_by_role() {
    let registry = Registry::builtin().unwrap();
    let active = Profile::default();
    assert_eq!(
        candidate_profile("active", &active, &registry).unwrap(),
        active
    );
    let p = candidate_profile(
        "whisper-large-v3-turbo+nemo-titanet-small",
        &active,
        &registry,
    )
    .unwrap();
    assert_eq!(p.asr, "whisper-large-v3-turbo");
    assert_eq!(p.speaker_embedding, "nemo-titanet-small");
    assert_eq!(p.diarization, active.diarization);
    assert!(
        candidate_profile(
            "parakeet-tdt-0.6b-v3+whisper-large-v3-turbo",
            &active,
            &registry
        )
        .is_err()
    );
    assert!(candidate_profile("potion-multilingual-128m", &active, &registry).is_err());
    assert!(candidate_profile("no-such-model", &active, &registry).is_err());
}

// ---- worker contract and run ----

/// A set with one audio-file excerpt of two synthetic voices (4 s each)
/// and hand-written turns for two people outside the library.
fn audio_set(fx: &Fixture, data: &Path, checked: bool) {
    let wav = fx.wav("talk.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
    let set = BenchSet {
        schema_version: SET_SCHEMA_VERSION,
        excerpts: vec![Excerpt {
            id: "e1".into(),
            meeting_id: None,
            audio_path: Some(wav),
            start: 0.0,
            end: 8.0,
            reference_text: Some("hello world".into()),
            reference_checked: checked,
            reference_turns: Some(vec![turn(0.0, 4.0, "hint:ana"), turn(4.0, 8.0, "hint:bo")]),
            turns_source: None,
            created_at: None,
        }],
    };
    save_set(data, "s", &set).unwrap();
}

fn provider() -> FakeModelProvider {
    FakeModelProvider {
        schedule: vec![
            DiarSegment {
                start_s: 0.0,
                end_s: 4.0,
                speaker: 0,
            },
            DiarSegment {
                start_s: 4.0,
                end_s: 8.0,
                speaker: 1,
            },
        ],
        ..FakeModelProvider::default()
    }
}

const USAGE: FakeResourceUsage = FakeResourceUsage {
    usage: Some(ProcessUsage {
        peak_rss_bytes: 123_456_789,
        user_cpu_s: 1.0,
        system_cpu_s: 0.5,
    }),
};

#[test]
fn worker_report_round_trips_as_json() {
    let fx = Fixture::new();
    let data = data_dir(&fx);
    audio_set(&fx, &data, true);
    let opts = WorkerOptions {
        set: "s".into(),
        candidate: "active".into(),
        manifest: None,
        calibrate_only: false,
    };
    let report = run_worker(&fx.env(), &provider(), &data, &opts, &USAGE).unwrap();
    assert_eq!(report.schema_version, WORKER_SCHEMA_VERSION);
    assert_eq!(report.peak_rss_bytes, Some(123_456_789));
    assert_eq!(report.thresholds_source, "none");
    assert_eq!(report.people_with_voiceprints, 0);
    let x = &report.excerpts[0];
    assert!(x.error.is_none(), "{x:?}");
    assert!(close(x.audio_s, 8.0));
    assert_eq!(x.speakers.len(), 2);
    assert!(x.turns.iter().all(|t| t.label.starts_with('S')));

    let text = serde_json::to_string(&report).unwrap();
    let back: WorkerReport = serde_json::from_str(&text).unwrap();
    assert_eq!(back, report);
    back.check("active").unwrap();
    assert!(back.check("other").is_err());
    let mut old = back.clone();
    old.schema_version = WORKER_SCHEMA_VERSION + 1;
    assert!(old.check("active").is_err());

    // A platform that cannot measure: no figure, a reason.
    let none = FakeResourceUsage { usage: None };
    let report = run_worker(&fx.env(), &provider(), &data, &opts, &none).unwrap();
    assert_eq!(report.peak_rss_bytes, None);
    assert!(report.usage_error.is_some());
}

#[test]
fn benchmark_analyzes_every_source_on_the_meeting_timeline() {
    let fx = Fixture::new();
    let data = data_dir(&fx);
    audio_set(&fx, &data, true);
    let first = fx.wav("first.wav", &[(Some(0), 4.0)]);
    let second = fx.wav("second.wav", &[(Some(1), 4.0)]);
    let id = fx.lib.new_meeting_id().unwrap();
    let mut meeting = Meeting::new(id.clone(), Source::Import, schema::now());
    meeting.recording = Some(schema::Recording {
        state: schema::RecordingState::Available,
        handling: schema::Handling::Reference,
        path: "not-a-primary-file.wav".into(),
        original_path: None,
        sha256: "test".into(),
        bytes: 0,
        source_sha256: None,
        retention_override: None,
        delete_after: None,
        sources: vec![
            schema::RecordingSource {
                path: first,
                track_index: 0,
                offset_s: 2.0,
                sha256: "first".into(),
            },
            schema::RecordingSource {
                path: second,
                track_index: 0,
                offset_s: 6.0,
                sha256: "second".into(),
            },
        ],
    });
    fx.lib
        .write(|tx| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)
        })
        .unwrap();
    let mut set = load_set(&data, "s").unwrap();
    set.excerpts[0].meeting_id = Some(id);
    set.excerpts[0].audio_path = None;
    set.excerpts[0].start = 3.0;
    set.excerpts[0].end = 10.0;
    save_set(&data, "s", &set).unwrap();
    let report = run_worker(
        &fx.env(),
        &provider(),
        &data,
        &WorkerOptions {
            set: "s".into(),
            candidate: "active".into(),
            ..Default::default()
        },
        &USAGE,
    )
    .unwrap();
    let excerpt = &report.excerpts[0];
    assert!(excerpt.error.is_none(), "{:?}", excerpt.error);
    assert_eq!(excerpt.audio_s, 7.0);
    assert_eq!(excerpt.speakers.len(), 2);
    assert_eq!(
        excerpt
            .turns
            .iter()
            .map(|t| (t.start, t.end))
            .collect::<Vec<_>>(),
        [(3.0, 6.0), (6.0, 10.0)]
    );
    assert_ne!(excerpt.turns[0].label, excerpt.turns[1].label);
}

#[test]
fn benchmark_naming_excludes_confirmed_and_weak_samples_from_its_meeting() {
    let mut fx = Fixture::new();
    let data = data_dir(&fx);
    audio_set(&fx, &data, true);
    let file = fx.wav("named.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
    let imported = crate::import::import_file(
        &fx.env(),
        &mut crate::testutil::models(),
        &file,
        &crate::import::ImportOptions::default(),
    )
    .unwrap();
    let meeting = fx.lib.load_meeting(&imported.meeting_id).unwrap();
    let person = schema::Person::new(fx.lib.new_person_id(), "Ari Example");
    let mut vps = schema::Voiceprints::empty(person.id.clone());
    for (i, speaker) in meeting.speakers.iter().enumerate() {
        let centroid = speaker.centroid.as_ref().unwrap();
        vps.embeddings.push(schema::Voiceprint {
            id: format!("sample-{i}"),
            model: centroid.model.clone(),
            dim: centroid.dim,
            vector_b64: centroid.vector_b64.clone(),
            source: schema::VoiceprintSource {
                kind: schema::VoiceprintKind::Hint,
                meeting_id: Some(meeting.id.clone()),
                sha256: None,
                start: 0.0,
                end: 8.0,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: if i == 0 {
                schema::Strength::Confirmed
            } else {
                schema::Strength::Weak
            },
            created_at: schema::now(),
        });
    }
    fx.lib
        .write(|tx| {
            tx.save_person(&person)?;
            tx.save_voiceprints(&vps)
        })
        .unwrap();
    fx.config.identity.insert(
        meeting.speakers[0].centroid.as_ref().unwrap().model.clone(),
        Thresholds {
            auto_name: Some(0.1),
            suggest: Some(0.05),
            margin: Some(0.0),
            merge: None,
            auto_merge: None,
        },
    );
    // Held out by meeting ID, and by content hash when the excerpt names the
    // previously imported audio file instead.
    for by_meeting in [true, false] {
        let mut set = load_set(&data, "s").unwrap();
        if by_meeting {
            set.excerpts[0].meeting_id = Some(meeting.id.clone());
            set.excerpts[0].audio_path = None;
        } else {
            set.excerpts[0].meeting_id = None;
            set.excerpts[0].audio_path = Some(file.clone());
        }
        save_set(&data, "s", &set).unwrap();
        let report = run_worker(
            &fx.env(),
            &provider(),
            &data,
            &WorkerOptions {
                set: "s".into(),
                candidate: "active".into(),
                ..Default::default()
            },
            &USAGE,
        )
        .unwrap();
        assert!(
            report.excerpts[0].error.is_none(),
            "{:?}",
            report.excerpts[0]
        );
        assert!(
            report.excerpts[0]
                .speakers
                .iter()
                .all(|s| s.person.is_none() && s.suggested.is_none()),
            "by_meeting={by_meeting}: {:?}",
            report.excerpts[0].speakers
        );
    }
}

/// A headphones meeting recorded as a mic + system pair (the system track
/// starting 2 s in, one remote voice), set as the only excerpt (0–8 s).
fn paired_meeting(
    fx: &Fixture,
    data: &Path,
    mic_turns: &[(Option<usize>, f64)],
) -> (MeetingId, PathBuf, PathBuf) {
    use broscribe_core::schema::{Track, TrackRole};
    audio_set(fx, data, true);
    let mic = fx.wav("mic.wav", mic_turns);
    let system = fx.wav("system.wav", &[(Some(2), 6.0)]);
    let id = fx.lib.new_meeting_id().unwrap();
    let mut meeting = Meeting::new(id.clone(), Source::Import, schema::now());
    meeting.setup = Some("headphones".into());
    meeting.processing.echo_report = Some(serde_json::json!({"requested": "none"}));
    meeting.tracks = vec![
        Track {
            id: "t0".into(),
            role: TrackRole::Mic,
        },
        Track {
            id: "t1".into(),
            role: TrackRole::System,
        },
    ];
    meeting.recording = Some(schema::Recording {
        state: schema::RecordingState::Available,
        handling: schema::Handling::Reference,
        path: mic.clone(),
        original_path: None,
        sha256: "mic".into(),
        bytes: 0,
        source_sha256: None,
        retention_override: None,
        delete_after: None,
        sources: [(&mic, 0.0), (&system, 2.0)]
            .into_iter()
            .map(|(path, offset_s)| schema::RecordingSource {
                path: path.clone(),
                track_index: 0,
                offset_s,
                sha256: path.display().to_string(),
            })
            .collect(),
    });
    fx.lib
        .write(|tx| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)
        })
        .unwrap();
    let mut set = load_set(data, "s").unwrap();
    set.excerpts[0].meeting_id = Some(id.clone());
    set.excerpts[0].audio_path = None;
    set.excerpts[0].start = 0.0;
    set.excerpts[0].end = 8.0;
    save_set(data, "s", &set).unwrap();
    (id, mic, system)
}

/// The active candidate's only excerpt, which must not have failed.
fn bench_excerpt(fx: &Fixture, data: &Path) -> super::worker::WorkerExcerpt {
    let report = run_worker(
        &fx.env(),
        &provider(),
        data,
        &WorkerOptions {
            set: "s".into(),
            candidate: "active".into(),
            ..Default::default()
        },
        &USAGE,
    )
    .unwrap();
    let excerpt = report.excerpts.into_iter().next().unwrap();
    assert!(excerpt.error.is_none(), "{:?}", excerpt.error);
    excerpt
}

/// Saves `person` with one confirmed sample: the candidate embedder's
/// voiceprint of `wav`, derived from `meeting` when given. Returns the
/// embedder's model.
fn enroll(fx: &Fixture, person: &Person, wav: &Path, meeting: Option<&MeetingId>) -> String {
    use broscribe_models::ModelProvider;
    let models = provider().load(&fx.config.profile).unwrap();
    let samples = crate::audio::decode_16k(&fx.chain, wav).unwrap().samples;
    let v = models.embedder.embed(&samples).unwrap().unwrap();
    let model = models.embedder.model_id().to_owned();
    let mut vps = schema::Voiceprints::empty(person.id.clone());
    vps.embeddings.push(schema::Voiceprint {
        id: format!("sample-{}", person.id),
        model: model.clone(),
        dim: v.len(),
        vector_b64: schema::encode_vector(&v),
        source: schema::VoiceprintSource {
            kind: schema::VoiceprintKind::Enrollment,
            meeting_id: meeting.cloned(),
            sha256: None,
            start: 0.0,
            end: 8.0,
            ranges: Vec::new(),
            speaker_id: None,
            speaker_namespace: String::new(),
        },
        device: None,
        strength: schema::Strength::Confirmed,
        created_at: schema::now(),
    });
    fx.lib
        .write(|tx| {
            tx.save_person(person)?;
            tx.save_voiceprints(&vps)
        })
        .unwrap();
    model
}

fn calibrated(fx: &mut Fixture, model: String) {
    fx.config.identity.insert(
        model,
        Thresholds {
            auto_name: Some(0.8),
            suggest: Some(0.7),
            margin: Some(0.0),
            merge: None,
            auto_merge: None,
        },
    );
}

#[test]
fn benchmark_paired_me_verification_holds_out_samples_from_the_evaluated_meeting() {
    let mut fx = Fixture::new();
    let data = data_dir(&fx);
    let (id, mic, _) = paired_meeting(&fx, &data, &[(Some(0), 8.0)]);
    let mut me = Person::new(fx.lib.new_person_id(), "Me");
    me.is_self = true;
    let me_id = me.id.to_string();
    // Control: an enrollment outside the set verifies Me on the microphone,
    // as production does.
    let model = enroll(&fx, &me, &mic, None);
    calibrated(&mut fx, model);
    let excerpt = bench_excerpt(&fx, &data);
    assert!(
        excerpt
            .speakers
            .iter()
            .any(|s| s.person.as_deref() == Some(&me_id) && s.assignment == Assignment::Auto),
        "{:?}",
        excerpt.speakers
    );
    // The same sample taken from the benchmarked meeting is evaluation data:
    // the paired analysis must not verify Me with it.
    enroll(&fx, &me, &mic, Some(&id));
    let excerpt = bench_excerpt(&fx, &data);
    assert!(
        excerpt
            .speakers
            .iter()
            .all(|s| s.person.is_none() && s.suggested.is_none()),
        "{:?}",
        excerpt.speakers
    );
}

#[test]
fn benchmark_keeps_the_pair_when_the_excerpt_ends_before_the_system_track() {
    let mut fx = Fixture::new();
    let data = data_dir(&fx);
    let (id, mic, _) = paired_meeting(&fx, &data, &[(Some(0), 8.0)]);
    // The system source starts after the excerpt (0–8 s) ends: its part of
    // the excerpt is empty, but the meeting is still a mic + system pair.
    let mut meeting = fx.lib.load_meeting(&id).unwrap();
    meeting.recording.as_mut().unwrap().sources[1].offset_s = 9.0;
    fx.lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
    let mut me = Person::new(fx.lib.new_person_id(), "Me");
    me.is_self = true;
    let me_id = me.id.to_string();
    let model = enroll(&fx, &me, &mic, None);
    calibrated(&mut fx, model);
    let excerpt = bench_excerpt(&fx, &data);
    // Production analyzes the pair (silent system side) and verifies Me on
    // the microphone; the benchmark must do the same.
    assert!(
        excerpt
            .speakers
            .iter()
            .any(|s| s.person.as_deref() == Some(&me_id) && s.assignment == Assignment::Auto),
        "{:?}",
        excerpt.speakers
    );
}

#[test]
fn benchmark_keeps_the_pair_when_the_excerpt_ends_before_the_mic_track() {
    let mut fx = Fixture::new();
    let data = data_dir(&fx);
    let (id, mic, system) = paired_meeting(&fx, &data, &[(Some(0), 8.0)]);
    // The microphone source starts after the excerpt (0–8 s) ends: its part
    // of the excerpt is empty, but the meeting is still a mic + system pair.
    let mut meeting = fx.lib.load_meeting(&id).unwrap();
    meeting.recording.as_mut().unwrap().sources[0].offset_s = 9.0;
    fx.lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
    let mut me = Person::new(fx.lib.new_person_id(), "Me");
    me.is_self = true;
    let me_id = me.id.to_string();
    let model = enroll(&fx, &me, &mic, None);
    let bo = Person::new(fx.lib.new_person_id(), "Bo Example");
    let bo_id = bo.id.to_string();
    enroll(&fx, &bo, &system, None);
    calibrated(&mut fx, model);
    let excerpt = bench_excerpt(&fx, &data);
    // Production analyzes the pair (silent microphone side): the system
    // voice is resolved and nobody is taken for Me.
    assert!(
        excerpt
            .speakers
            .iter()
            .any(|s| s.person.as_deref() == Some(&bo_id)),
        "{:?}",
        excerpt.speakers
    );
    assert!(
        excerpt
            .speakers
            .iter()
            .all(|s| s.person.as_deref() != Some(&me_id) && s.suggested.as_deref() != Some(&me_id)),
        "{:?}",
        excerpt.speakers
    );
}

#[test]
fn benchmark_leaves_unknown_paired_mic_speech_unnamed_like_finalization() {
    let mut fx = Fixture::new();
    let data = data_dir(&fx);
    let (_, mic, system) = paired_meeting(&fx, &data, &[(Some(0), 8.0)]);
    // No Me enrolled: the microphone's speech is Unknown. A non-Me person
    // matching its centroid must not name it; production finalization
    // protects the paired microphone's speakers.
    let ari = Person::new(fx.lib.new_person_id(), "Ari Example");
    let model = enroll(&fx, &ari, &mic, None);
    // Control: the system track's speaker is still resolved.
    let bo = Person::new(fx.lib.new_person_id(), "Bo Example");
    enroll(&fx, &bo, &system, None);
    calibrated(&mut fx, model);
    let excerpt = bench_excerpt(&fx, &data);
    let ari_id = ari.id.to_string();
    assert!(
        excerpt.speakers.iter().all(
            |s| s.person.as_deref() != Some(&ari_id) && s.suggested.as_deref() != Some(&ari_id)
        ),
        "{:?}",
        excerpt.speakers
    );
    let bo_id = bo.id.to_string();
    assert!(
        excerpt
            .speakers
            .iter()
            .any(|s| s.person.as_deref() == Some(&bo_id)),
        "{:?}",
        excerpt.speakers
    );
}

#[test]
fn benchmark_routes_a_mic_system_pair_through_the_production_analyzer() {
    use crate::manifest::ManifestSource;
    use broscribe_core::schema::TrackRole;
    use broscribe_models::ModelProvider;
    let fx = Fixture::new();
    let data = data_dir(&fx);
    // Two voices on the microphone: a non-room mic is one person (plus
    // unverified speech), never diarized like a room.
    let (_, mic, system) = paired_meeting(&fx, &data, &[(Some(0), 4.0), (Some(1), 4.0)]);
    let excerpt = bench_excerpt(&fx, &data);

    let sources = [
        (mic.clone(), TrackRole::Mic, 0.0),
        (system.clone(), TrackRole::System, 2.0),
    ]
    .map(|(path, role, offset_s)| ManifestSource {
        path,
        audio_track_index: Some(0),
        role,
        person_hint: None,
        offset_s,
    });
    let mut models = provider().load(&fx.config.profile).unwrap();
    let expected = crate::multitrack::analyze_sources_with_setup(
        &fx.env(),
        &sources,
        &mut models,
        &[],
        Some(broscribe_dsp::Setup::Headphones),
        Some(broscribe_dsp::EchoHandling::None),
    )
    .unwrap();
    assert_eq!(excerpt.speakers.len(), expected.speakers.len());
    let spans = |turns: Vec<(f64, f64)>| {
        let mut turns = turns;
        turns.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
        turns
    };
    assert_eq!(
        spans(excerpt.turns.iter().map(|t| (t.start, t.end)).collect()),
        spans(
            expected
                .diar_segments
                .iter()
                .map(|t| (t.start, t.end))
                .collect()
        )
    );
    // Plain per-track analysis (the old bench path) would diarize the
    // microphone like a room and count more speakers.
    let plain: usize = [&mic, &system]
        .iter()
        .map(|path| {
            crate::analyze::analyze(
                &crate::audio::decode_16k(&fx.chain, path).unwrap().samples,
                &mut models,
                crate::analyze::AnalyzeOptions::default(),
            )
            .unwrap()
            .speakers
            .len()
        })
        .sum();
    assert!(
        excerpt.speakers.len() < plain,
        "{} vs {plain}",
        excerpt.speakers.len()
    );
}

#[test]
fn calibration_only_worker_never_runs_benchmark_excerpts() {
    let fx = Fixture::new();
    let data = data_dir(&fx);
    audio_set(&fx, &data, true);
    let manifest = data.join("empty.json");
    fsutil::write_json_atomic(
        &manifest,
        &serde_json::json!({
            "format": "broscribe-import-manifest", "version": 1, "meetings": []
        }),
    )
    .unwrap();
    let report = run_worker(
        &fx.env(),
        &provider(),
        &data,
        &WorkerOptions {
            set: "s".into(),
            candidate: "active".into(),
            manifest: Some(manifest),
            calibrate_only: true,
        },
        &USAGE,
    )
    .unwrap();
    assert!(report.calibration.is_some());
    assert!(report.excerpts.is_empty());
    assert_eq!(report.decode_s, 0.0);
}

#[test]
fn run_scores_candidates_and_stores_figures_only() {
    let fx = Fixture::new();
    let data = data_dir(&fx);
    audio_set(&fx, &data, false);
    let opts = RunOptions {
        set: "s".into(),
        candidates: vec!["active".into(), "nemo-titanet-small".into()],
        manifest: None,
        calibrate: false,
    };
    let env = fx.env();
    let mut requests = Vec::new();
    let mut launch = |req: &WorkerRequest| -> Result<WorkerReport> {
        requests.push(req.clone());
        let usage = if req.candidate == "active" {
            USAGE
        } else {
            FakeResourceUsage { usage: None }
        };
        let report = run_worker(
            &env,
            &provider(),
            &data,
            &WorkerOptions {
                set: req.set.clone(),
                candidate: req.candidate.clone(),
                manifest: req.manifest.clone(),
                calibrate_only: req.calibrate_only,
            },
            &usage,
        )?;
        // Through JSON, as from a child process.
        Ok(serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap())
    };
    let out = run(&env, &data, &opts, &mut launch).unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|r| !r.calibrate_only));
    assert_eq!(out.results.candidates.len(), 2);
    let a = out.results.candidates[0].metrics.as_ref().unwrap();
    // Unchecked text: WER unavailable, not zero.
    assert!(a.wer.get().is_none());
    assert!(a.der.get().unwrap() < 0.05, "{:?}", a.der);
    assert_eq!(a.diarization_reference.as_deref(), Some("manual"));
    assert!(close(a.clusters_per_true_speaker.get().unwrap(), 1.0));
    // Nobody has voiceprints: naming unavailable.
    assert!(a.naming_accuracy.get().is_none());
    assert!(a.estimated_manual_fixes_per_meeting.get().is_none());
    assert_eq!(a.peak_rss_bytes.get(), Some(123_456_789.0));
    let b = &out.results.candidates[1];
    assert_eq!(b.models.speaker_embedding, "nemo-titanet-small");
    assert!(b.metrics.as_ref().unwrap().peak_rss_bytes.get().is_none());

    assert!(out.path.starts_with(results_dir(&data)));
    let stored = std::fs::read_to_string(&out.path).unwrap();
    assert!(!stored.contains("hello"), "results must not hold text");
    let back: BenchResults = serde_json::from_str(&stored).unwrap();
    assert_eq!(back, out.results);

    // A failed worker is reported, not fatal.
    let mut failing =
        |_: &WorkerRequest| -> Result<WorkerReport> { Err(Error::ModelMissing("whisper".into())) };
    let out = run(&env, &data, &opts, &mut failing).unwrap();
    assert!(
        out.results
            .candidates
            .iter()
            .all(|c| c.error.is_some() && c.metrics.is_none())
    );
}

#[test]
fn calibrate_needs_a_manifest_and_set_names_are_checked() {
    let fx = Fixture::new();
    let data = data_dir(&fx);
    let mut never = |_: &WorkerRequest| -> Result<WorkerReport> { unreachable!() };
    let opts = RunOptions {
        set: "s".into(),
        candidates: vec!["active".into()],
        manifest: None,
        calibrate: true,
    };
    assert!(run(&fx.env(), &data, &opts, &mut never).is_err());
    for bad in ["", "results", "a/b", ".."] {
        assert!(validate_set_name(bad).is_err(), "{bad}");
    }
    // A missing set is not found.
    let opts = RunOptions {
        calibrate: false,
        ..opts
    };
    assert!(matches!(
        run(&fx.env(), &data, &opts, &mut never),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn absent_turn_references_are_unavailable_and_manifest_turns_are_labelled() {
    let fx = Fixture::new();
    let data = data_dir(&fx);
    audio_set(&fx, &data, true);
    let mut set = load_set(&data, "s").unwrap();
    set.excerpts[0].reference_turns = None;
    save_set(&data, "s", &set).unwrap();
    let env = fx.env();
    let mut launch = |req: &WorkerRequest| {
        run_worker(
            &env,
            &provider(),
            &data,
            &WorkerOptions {
                set: req.set.clone(),
                candidate: req.candidate.clone(),
                manifest: req.manifest.clone(),
                calibrate_only: false,
            },
            &USAGE,
        )
    };
    let mut opts = RunOptions {
        set: "s".into(),
        candidates: vec!["active".into()],
        ..RunOptions::default()
    };
    let out = run(&env, &data, &opts, &mut launch).unwrap();
    let metrics = out.results.candidates[0].metrics.as_ref().unwrap();
    assert!(metrics.der.get().is_none());
    assert!(metrics.jer.get().is_none());
    assert!(metrics.clusters_per_true_speaker.get().is_none());
    assert!(metrics.naming_accuracy.get().is_none());
    assert!(metrics.estimated_manual_fixes_per_meeting.get().is_none());
    let manifest = data.join("hints.json");
    fsutil::write_json_atomic(
        &manifest,
        &serde_json::json!({
            "format": "broscribe-import-manifest", "version": 1,
            "meetings": [{
                "audio_path": set.excerpts[0].audio_path,
                "speaker_hints": [
                    {"name": "Ari Example", "ranges": [[0.0, 4.0]]},
                    {"name": "Bea Example", "ranges": [[4.0, 8.0]]}
                ]
            }]
        }),
    )
    .unwrap();
    opts.manifest = Some(manifest);
    let out = run(&env, &data, &opts, &mut launch).unwrap();
    assert!(out.results.manifest_hints);
    let metrics = out.results.candidates[0].metrics.as_ref().unwrap();
    assert_eq!(
        metrics.diarization_reference.as_deref(),
        Some("hint-referenced")
    );
    assert!(metrics.der.get().unwrap() < 0.05);
}

#[test]
fn calibration_is_stored_per_model_and_only_active_model_is_reresolved() {
    let fx = Fixture::new();
    let data = data_dir(&fx);
    audio_set(&fx, &data, true);
    let manifest = data.join("hints.json");
    fsutil::write_json_atomic(
        &manifest,
        &serde_json::json!({
            "format": "broscribe-import-manifest", "version": 1, "meetings": []
        }),
    )
    .unwrap();
    // Stand in for the worker's fitting step, not for the parent: its
    // thresholds are a JSON protocol input and must be persisted by model.
    let env = fx.env();
    let fitted = Thresholds {
        auto_name: Some(0.9),
        suggest: Some(0.7),
        margin: Some(0.1),
        merge: Some(0.95),
        auto_merge: None,
    };
    let mut requests = Vec::new();
    let mut launch = |req: &WorkerRequest| {
        requests.push(req.clone());
        if requests.len() == 1 {
            // The user selects another embedding model while the worker
            // is running. Only the now-active model may re-resolve.
            let mut config = Config::load(&data)?;
            config.profile.speaker_embedding = "nemo-titanet-small".into();
            config.save(&data)?;
        }
        let mut report = run_worker(
            &env,
            &provider(),
            &data,
            &WorkerOptions {
                set: req.set.clone(),
                candidate: req.candidate.clone(),
                manifest: None,
                calibrate_only: false,
            },
            &USAGE,
        )?;
        report.thresholds = Some(fitted);
        // A measuring worker reads the stored thresholds from its config.
        report.thresholds_source = "config".into();
        if req.calibrate_only {
            report.thresholds_source = "calibrated".into();
            report.calibration = Some(worker::WorkerCalibration {
                thresholds: Some(fitted),
                ..worker::WorkerCalibration::default()
            });
        }
        Ok(report)
    };
    let opts = RunOptions {
        set: "s".into(),
        candidates: vec![
            "nemo-titanet-small".into(),
            "active".into(),
            "whisper-large-v3-turbo".into(),
        ],
        manifest: Some(manifest),
        calibrate: true,
    };
    let out = run(&env, &data, &opts, &mut launch).unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|r| r.calibrate_only)
            .collect::<Vec<_>>(),
        [true, false, true, false, false]
    );
    let config = Config::load(&data).unwrap();
    assert_eq!(config.thresholds("nemo-titanet-small"), Some(&fitted));
    assert_eq!(config.thresholds("nemo-titanet-large"), Some(&fitted));
    assert_eq!(config.profile.speaker_embedding, "nemo-titanet-small");
    let inactive = out.results.candidates[1].calibration.as_ref().unwrap();
    assert!(inactive.written);
    assert_eq!(inactive.re_resolved_changes, None);
    let active = out.results.candidates[0].calibration.as_ref().unwrap();
    assert!(active.written);
    assert_eq!(active.re_resolved_changes, Some(0));
    assert!(out.results.candidates[2].calibration.is_none());
    assert!(
        out.results
            .candidates
            .iter()
            .all(|c| c.thresholds_source == "calibrated"),
        "thresholds stored by this run are labelled calibrated"
    );
}

#[test]
fn jer_optimises_speaker_jaccard_instead_of_total_overlap() {
    let reference = [turn(0.0, 100.0, "A"), turn(100.0, 110.0, "B")];
    let hypothesis = [
        turn(0.0, 59.0, "X"),
        turn(100.0, 110.0, "X"),
        turn(59.0, 100.0, "Y"),
    ];
    let errors = diarization_errors(&reference, &hypothesis, (0.0, 110.0), 0.0);
    // DER maximises overlap: A→X (59 s), leaving B unmatched.
    assert!(close(errors.der().get().unwrap(), 51.0 / 110.0));
    // JER maximises per-speaker IoU: A→Y (41/100), B→X (10/69).
    assert!(close(
        errors.jer().get().unwrap(),
        (0.59 + 59.0 / 69.0) / 2.0
    ));
}

// ---- merge remap, calibration people, hotwords ----

#[test]
fn merged_wordless_cluster_scores_as_the_kept_speaker() {
    use broscribe_core::schema::Centroid;
    let mut talker = Speaker::new(sid(1), "t0");
    talker.talk_s = 50.0;
    talker.centroid = Some(Centroid::new("fake", &[1.0, 0.0], 2));
    // Diarized but no recognised words: no segment carries this speaker.
    let mut wordless = Speaker::new(sid(2), "t0");
    wordless.talk_s = 5.0;
    wordless.centroid = Some(Centroid::new("fake", &[0.99, 0.05], 2));
    let mut m = Meeting::new(
        MeetingId::parse("m-000001").unwrap(),
        Source::Import,
        schema::now(),
    );
    m.segments.push(seg(1, 1, 0.0, &["synthetic", "words"]));
    m.speakers = vec![talker, wordless];
    let diar = [
        crate::Turn {
            start: 0.0,
            end: 2.0,
            speaker: sid(1),
        },
        crate::Turn {
            start: 2.0,
            end: 3.0,
            speaker: sid(2),
        },
    ];
    let turns = super::worker::merge_turns(&mut m, "fake", Some(0.9), None, &diar).unwrap();
    assert_eq!(m.speakers.len(), 1, "the clusters merge");
    let kept = m.speakers[0].id.to_string();
    assert!(
        turns.iter().all(|t| t.label == kept),
        "every turn, the wordless cluster's included, is the kept speaker: {turns:?}"
    );
}

#[test]
fn calibration_library_links_hints_to_existing_people_without_touching_the_library() {
    let fx = Fixture::new();
    let mut ana = Person::new(fx.lib.new_person_id(), "Ana Synthetic");
    ana.aliases = vec!["Annie Fictional".into()];
    ana.emails = vec!["ana@example.test".into()];
    let wav = fx.wav("ana.wav", &[(Some(0), 4.0)]);
    enroll(&fx, &ana, &wav, None);
    let people_before = fx.lib.list_people().unwrap();
    let vps_path = fx.lib.person_dir(&ana.id).join("voiceprints.json");
    let vps_before = std::fs::read(&vps_path).ok();

    let tmp = tempfile::tempdir().unwrap();
    let iso = super::worker::isolated_library(&fx.lib, &tmp.path().join("library")).unwrap();
    let copied = iso.list_people().unwrap();
    let by_email = crate::manifest::match_person(&copied, None, Some("ANA@example.test"));
    assert_eq!(by_email.map(|p| &p.id), Some(&ana.id));
    let by_alias = crate::manifest::match_person(&copied, Some("annie fictional"), None);
    assert_eq!(by_alias.map(|p| &p.id), Some(&ana.id));
    assert!(
        !iso.person_dir(&ana.id).join("voiceprints.json").exists(),
        "voiceprints are not copied"
    );

    // Calibration writes stay in the scratch library.
    iso.write(|tx| tx.save_person(&Person::new(iso.new_person_id(), "Bo Invented")))
        .unwrap();
    assert_eq!(fx.lib.list_people().unwrap(), people_before);
    assert_eq!(std::fs::read(&vps_path).ok(), vps_before);
}

#[test]
fn every_excerpt_sets_its_own_hotwords() {
    use broscribe_core::schema::Participant;
    use broscribe_models::{Asr, AsrOutput, ModelProvider, ModelSet, SpeakerEmbedder};
    use std::sync::{Arc, Mutex};

    struct HotwordAsr {
        inner: broscribe_models::fakes::FakeAsr,
        log: Arc<Mutex<Vec<Vec<String>>>>,
    }
    impl Asr for HotwordAsr {
        fn transcribe(&self, samples_16k: &[f32]) -> broscribe_core::Result<AsrOutput> {
            self.inner.transcribe(samples_16k)
        }
        fn accepts_hotwords(&self) -> bool {
            true
        }
        fn set_hotwords(&mut self, terms: &[String]) -> broscribe_core::Result<()> {
            self.log.lock().unwrap().push(terms.to_vec());
            Ok(())
        }
    }
    struct HotwordProvider(Arc<Mutex<Vec<Vec<String>>>>);
    impl ModelProvider for HotwordProvider {
        fn load(&self, profile: &Profile) -> broscribe_core::Result<ModelSet> {
            let mut set = provider().load(profile)?;
            set.asr = Box::new(HotwordAsr {
                inner: broscribe_models::fakes::FakeAsr::new(&["hello", "world"]),
                log: self.0.clone(),
            });
            Ok(set)
        }
        fn load_vad(
            &self,
            model_id: &str,
        ) -> broscribe_core::Result<Box<dyn broscribe_models::Vad + Send>> {
            provider().load_vad(model_id)
        }
        fn load_embedder(
            &self,
            model_id: &str,
        ) -> broscribe_core::Result<Box<dyn SpeakerEmbedder + Send>> {
            provider().load_embedder(model_id)
        }
    }

    let fx = Fixture::new();
    let data = data_dir(&fx);
    let (id, _, _) = paired_meeting(&fx, &data, &[(Some(0), 8.0)]);
    let mut meeting = fx.lib.load_meeting(&id).unwrap();
    meeting.participants.push(Participant {
        name: Some("Zed Quillon".into()),
        ..Participant::default()
    });
    fx.lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
    // A second excerpt from an audio file outside the library.
    let mut set = load_set(&data, "s").unwrap();
    let mut outside = set.excerpts[0].clone();
    outside.id = "e2".into();
    outside.meeting_id = None;
    outside.audio_path = Some(fx.wav("other.wav", &[(Some(1), 4.0)]));
    outside.end = 4.0;
    set.excerpts.push(outside);
    save_set(&data, "s", &set).unwrap();

    let log = Arc::new(Mutex::new(Vec::new()));
    let report = run_worker(
        &fx.env(),
        &HotwordProvider(log.clone()),
        &data,
        &WorkerOptions {
            set: "s".into(),
            candidate: "active".into(),
            ..Default::default()
        },
        &USAGE,
    )
    .unwrap();
    assert!(
        report.excerpts.iter().all(|x| x.error.is_none()),
        "{report:?}"
    );
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 2, "hotwords are set once per excerpt: {log:?}");
    let has_zed = |terms: &Vec<String>| terms.iter().any(|t| t == "Zed Quillon");
    assert_eq!(
        log.iter().filter(|t| has_zed(t)).count(),
        1,
        "only the meeting's excerpt sees its attendee: {log:?}"
    );
}
