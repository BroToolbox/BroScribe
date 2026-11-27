use chrono::DateTime;
use tempfile::TempDir;

use super::*;
use crate::identity::testutil::{DIM, Rng, basis};
use crate::index::SearchQuery;
use crate::schema::{Centroid, Participant, Segment, Source};

const MODEL: &str = "test-embedder";
const ANA: &str = "p-a1a1a1";
const BIA: &str = "p-b2b2b2";
const MA: &str = "m-0000a1";
const MB: &str = "m-0000b2";
const MC: &str = "m-0000c3";

fn pid(s: &str) -> PersonId {
    PersonId::parse(s).unwrap()
}

fn mid(s: &str) -> MeetingId {
    MeetingId::parse(s).unwrap()
}

fn sid(n: u32) -> SpeakerId {
    SpeakerId::new(n).unwrap()
}

fn spk(n: u32, voice: Option<Vec<f32>>) -> Speaker {
    let mut s = Speaker::new(sid(n), "t0");
    s.talk_s = 10.0 * f64::from(n);
    s.centroid = voice.map(|v| Centroid::new(MODEL, &v, 3));
    s
}

fn seg(id: u32, speaker: u32, start: f64, text: &str) -> Segment {
    Segment {
        speech_overlap: Vec::new(),
        id,
        track: "t0".into(),
        speaker: Some(sid(speaker)),
        start,
        end: start + 2.0,
        text: text.into(),
        words: Vec::new(),
    }
}

fn meeting(id: &str, day: u32, speakers: Vec<Speaker>) -> Meeting {
    let started = DateTime::parse_from_rfc3339(&format!("2026-11-{day:02}T10:00:00Z")).unwrap();
    let mut m = Meeting::new(mid(id), Source::Import, started);
    m.title = Some(format!("Sync {day}"));
    m.duration_s = 60.0;
    m.processing.status = ProcessingStatus::Done;
    m.segments = speakers
        .iter()
        .enumerate()
        .map(|(i, s)| {
            seg(
                i as u32,
                s.id.number(),
                i as f64 * 5.0,
                &format!("words of speaker {}", s.id),
            )
        })
        .collect();
    m.speakers = speakers;
    m
}

fn confirmed_print(person: &PersonId, v: &[f32], start: f64) -> Voiceprint {
    let mut vp = new_voiceprint(
        person,
        MODEL,
        &meeting(MC, 1, Vec::new()),
        v,
        start,
        start + 2.0,
        VoiceprintKind::Hint,
        Strength::Confirmed,
    );
    vp.source.meeting_id = None;
    vp
}

/// A library holding `people` (with optional voices) and `meetings`, indexed.
fn setup(people: Vec<(Person, Vec<Vec<f32>>)>, meetings: Vec<Meeting>) -> (TempDir, Library) {
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path()).unwrap();
    lib.write(|tx| {
        for (p, voices) in &people {
            tx.save_person(p)?;
            if !voices.is_empty() {
                let prints = voices
                    .iter()
                    .enumerate()
                    .map(|(i, v)| confirmed_print(&p.id, v, i as f64 * 3.0))
                    .collect();
                tx.upsert_voiceprints(&p.id, prints)?;
            }
        }
        for m in &meetings {
            tx.create_meeting_dir(m)?;
            tx.save_meeting(m)?;
        }
        Index::rebuild(tx)?;
        Ok(())
    })
    .unwrap();
    (tmp, lib)
}

fn thresholds() -> Thresholds {
    Thresholds {
        auto_name: Some(0.8),
        suggest: Some(0.5),
        margin: Some(0.1),
        merge: None,
        auto_merge: None,
    }
}

#[test]
fn identity_checks_revisit_old_and_new_names_without_changing_them() {
    let ana = pid(ANA);
    let mut me = Person::new(ana.clone(), "");
    me.is_self = true;
    let (_tmp, lib) = setup(
        vec![(me, vec![])],
        vec![
            meeting(MA, 2, vec![spk(1, Some(basis(0)))]),
            meeting(MB, 3, vec![spk(1, Some(basis(1)))]),
            meeting(MC, 4, vec![spk(1, Some(basis(1)))]),
        ],
    );
    let name = |meeting: &str, name: &str| {
        with_index(&lib, |tx, index| {
            name_speaker(tx, index, &mid(meeting), sid(1), name, vec![], MODEL)
        })
        .unwrap()
    };
    let check = || identity_checks(&lib, Some(&ana), MODEL, None).unwrap();
    name(MA, ANA);
    assert_eq!(check().compared, 0, "a speaker cannot corroborate itself");
    name(MB, ANA);
    let results = check();
    assert_eq!(results.compared, 2);
    assert_eq!(results.alerts.len(), 2);
    assert!(results.alerts.iter().all(|alert| alert.person_name == "Me"));
    let historical = results.alerts[0].decision.clone();
    assert_eq!(historical.meeting_id, mid(MA));
    let keep = || lib.write(|tx| keep_identification(tx, &historical, MODEL, None));
    keep().unwrap();
    keep().unwrap();
    assert_eq!(check().alerts[0].decision.meeting_id, mid(MB));
    assert_eq!(lib.load_voiceprints(&ana).unwrap().embeddings.len(), 2);

    name(MC, ANA);
    let results = check();
    assert_eq!(results.compared, 3);
    assert_eq!(results.alerts.len(), 1);
    assert_eq!(results.alerts[0].decision.meeting_id, mid(MA));
    assert_ne!(results.alerts[0].decision.revision, historical.revision);
    assert!(
        keep().is_err(),
        "a decision cannot acknowledge new evidence"
    );
    for id in [MA, MB, MC] {
        let meeting = lib.load_meeting(&mid(id)).unwrap();
        let speaker = meeting.speaker(sid(1)).unwrap();
        assert_eq!(speaker.assignment, Assignment::Confirmed);
        assert_eq!(speaker.person_id.as_ref(), Some(&ana));
    }

    name(MA, "Bia Example");
    assert!(check().alerts.is_empty());
    assert_eq!(lib.load_voiceprints(&ana).unwrap().embeddings.len(), 2);
    with_index(&lib, |tx, index| forget_voice(tx, index, ANA)).unwrap();
    assert!(!lib.person_dir(&ana).join("identity-checks.json").exists());
    assert_eq!(check().compared, 0);
}

#[test]
fn identity_checks_count_each_confirmed_recording_once() {
    let ana = pid(ANA);
    let mut current = meeting(MA, 2, vec![spk(1, Some(basis(0)))]);
    current.speakers[0].assignment = Assignment::Confirmed;
    current.speakers[0].person_id = Some(ana.clone());
    let (_tmp, lib) = setup(
        vec![(Person::new(ana.clone(), "Ana Example"), vec![])],
        vec![current],
    );
    let print = |origin, start, direction| {
        new_voiceprint(
            &ana,
            MODEL,
            &meeting(origin, 3, vec![]),
            &basis(direction),
            start,
            start + 1.0,
            VoiceprintKind::Confirmation,
            Strength::Confirmed,
        )
    };
    let mut prints = (0..4)
        .map(|i| print(MB, f64::from(i), 0))
        .collect::<Vec<_>>();
    prints.extend([print(MC, 0.0, 1), print("m-0000d4", 0.0, 1)]);
    let mut weak = print("m-0000e5", 0.0, 0);
    weak.strength = Strength::Weak;
    let mut other_model = print("m-0000f6", 0.0, 0);
    other_model.model = "other-model".into();
    prints.extend([weak, other_model]);
    lib.write(|tx| tx.upsert_voiceprints(&ana, prints)).unwrap();
    let checks = identity_checks(&lib, None, MODEL, None).unwrap();
    assert_eq!(checks.compared, 1);
    assert_eq!(checks.alerts.len(), 1);
    assert_eq!(checks.alerts[0].decision.meeting_id, mid(MA));
    assert!(checks.alerts[0].score.abs() < f32::EPSILON);
}

fn md(lib: &Library, id: &str) -> String {
    let dir = lib.meeting_dir(&mid(id)).unwrap();
    fs::read_to_string(dir.join(paths::MEETING_MD)).unwrap()
}

/// `basis(0)` tilted towards `basis(1)`: second-best match for a voice near `basis(0)`.
fn tilted() -> Vec<f32> {
    let mut v: Vec<f32> = basis(0)
        .iter()
        .zip(basis(1))
        .map(|(a, b)| a + 0.8 * b)
        .collect();
    identity::l2_normalize(&mut v);
    v
}

/// Re-resolves every speaker of every `done` meeting from its centroid, as
/// the pipeline's library re-resolution does without caption replay: the
/// core pieces (`resolve_meeting`, `sync_weak_samples`) under one lock.
fn re_resolve_all(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<Vec<SpeakerChange>> {
    let lib = tx.library().clone();
    let people = load_person_samples(&lib, model)?;
    let mut all = Vec::new();
    for dir in lib.list_meeting_dirs()? {
        let mut meeting = lib.load_meeting_at(&dir)?;
        if meeting.processing.status != ProcessingStatus::Done {
            continue;
        }
        let before = meeting.clone();
        let roster = meeting.roster_person_ids();
        let changes = resolve_meeting(&mut meeting, model, &people, thresholds, Some(&roster))?;
        if meeting != before {
            save_and_index(tx, index, &mut meeting)?;
        }
        sync_weak_samples(tx, &meeting, &changes)?;
        all.extend(changes);
    }
    Ok(all)
}

#[test]
fn name_creates_person_and_requests_reevaluation() {
    let mut rng = Rng::new(7);
    let a = meeting(
        MA,
        10,
        vec![
            spk(1, Some(rng.voice(0, 0.03))),
            spk(2, Some(rng.voice(1, 0.03))),
        ],
    );
    let b = meeting(
        MB,
        11,
        vec![
            spk(1, Some(rng.voice(0, 0.03))),
            spk(2, Some(rng.voice(2, 0.03))),
        ],
    );
    let mut c = meeting(MC, 12, vec![spk(1, Some(rng.voice(0, 0.03)))]);
    c.processing.status = ProcessingStatus::Analyzed;
    let (_tmp, lib) = setup(Vec::new(), vec![a, b, c]);
    let t = thresholds();
    let job = crate::learning::job_id(MODEL);

    let sample = ConfirmationSample {
        vector: rng.voice(0, 0.03),
        start: 0.0,
        end: 2.0,
        namespace: None,
    };
    let out = with_index(&lib, |tx, idx| {
        name_speaker(
            tx,
            idx,
            &mid(MA),
            sid(1),
            "Ana Example",
            vec![sample],
            MODEL,
        )
    })
    .unwrap();
    assert!(out.created && out.voiceprint_added);
    assert_eq!(out.previous_person_id, None);
    let ana = out.person_id.clone();
    // Naming scans nothing: it records a reevaluation request, this
    // meeting first, and leaves the other speakers for the worker.
    assert_eq!(out.learning.job_id, job);
    assert_eq!(out.learning.generation, 1);
    assert_eq!(out.learning.source_meeting, Some(mid(MA)));
    let status = crate::learning::status(&lib, MODEL).unwrap().unwrap();
    assert_eq!(status.state, crate::queue::JobState::Queued);
    assert_eq!(status.source_meetings, vec![mid(MA)]);
    assert_eq!(
        lib.load_meeting(&mid(MB))
            .unwrap()
            .speaker(sid(1))
            .unwrap()
            .assignment,
        Assignment::None
    );

    let m = lib.load_meeting(&mid(MA)).unwrap();
    let s1 = m.speaker(sid(1)).unwrap();
    assert_eq!(s1.assignment, Assignment::Confirmed);
    assert_eq!(s1.person_id.as_ref(), Some(&ana));
    assert!(md(&lib, MA).contains("Ana Example"));
    let vps = lib.load_voiceprints(&ana).unwrap();
    assert_eq!(vps.embeddings.len(), 1);
    assert_eq!(vps.embeddings[0].source.kind, VoiceprintKind::Confirmation);
    assert_eq!(vps.embeddings[0].strength, Strength::Confirmed);

    // What the reevaluation's planner does to every `done` meeting.
    let changes = with_index(&lib, |tx, idx| re_resolve_all(tx, idx, MODEL, Some(&t))).unwrap();
    assert_eq!(changes.len(), 1, "{changes:?}");
    let b = lib.load_meeting(&mid(MB)).unwrap();
    let s1 = b.speaker(sid(1)).unwrap();
    assert_eq!(s1.assignment, Assignment::Auto);
    assert_eq!(s1.person_id.as_ref(), Some(&ana));
    assert_eq!(b.speaker(sid(2)).unwrap().assignment, Assignment::None);
    // The auto-name added one weak sample.
    let vps = lib.load_voiceprints(&ana).unwrap();
    assert_eq!(
        vps.embeddings
            .iter()
            .filter(|e| e.strength == Strength::Weak)
            .count(),
        1
    );
    // The analyzed meeting was not touched.
    let c = lib.load_meeting(&mid(MC)).unwrap();
    assert_eq!(c.speaker(sid(1)).unwrap().assignment, Assignment::None);

    let review = review_queue(&lib, None).unwrap();
    assert!(
        !review
            .iter()
            .any(|r| r.item.meeting_id == mid(MB) && r.item.speaker_id == sid(1))
    );

    // Optional manual confirmation still promotes the automatic name's weak sample.
    let out = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MB), sid(1), ana.as_str(), Vec::new(), MODEL)
    })
    .unwrap();
    assert!(!out.created);
    assert_eq!(out.promoted, 1);
    let vps = lib.load_voiceprints(&ana).unwrap();
    assert!(
        vps.embeddings
            .iter()
            .all(|e| e.strength == Strength::Confirmed)
    );
}

#[test]
fn samples_from_an_earlier_analysis_are_refused_under_the_lock() {
    let mut rng = Rng::new(7);
    let mut m = meeting(MA, 10, vec![spk(1, Some(rng.voice(0, 0.03)))]);
    m.speaker_namespace = "after-reanalysis".into();
    let (_tmp, lib) = setup(Vec::new(), vec![m]);
    let mut sample = |namespace: &str| ConfirmationSample {
        vector: rng.voice(0, 0.03),
        start: 0.0,
        end: 2.0,
        namespace: Some(namespace.into()),
    };
    let stale = vec![sample("before-reanalysis")];
    let err = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), sid(1), "Ana Example", stale, MODEL)
    })
    .unwrap_err();
    assert_eq!(err.code(), "invalid_input");
    assert!(err.to_string().contains("reanalysed"), "{err}");
    // Nothing changed: no person, the speaker still unnamed.
    assert!(lib.list_people().unwrap().is_empty());
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(m.speaker(sid(1)).unwrap().assignment, Assignment::None);

    let current = vec![sample("after-reanalysis")];
    let out = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), sid(1), "Ana Example", current, MODEL)
    })
    .unwrap();
    assert!(out.voiceprint_added);
}

#[test]
fn name_validation_and_no_voice() {
    let dup1 = Person::new(pid(ANA), "Dup Name");
    let dup2 = Person::new(pid(BIA), " dup name ");
    let m = meeting(MA, 10, vec![spk(1, None)]);
    let (_tmp, lib) = setup(vec![(dup1, Vec::new()), (dup2, Vec::new())], vec![m]);

    let err = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), sid(1), "DUP NAME", Vec::new(), MODEL)
    })
    .unwrap_err();
    assert_eq!(err.code(), "invalid_input");
    assert!(err.to_string().contains(ANA) && err.to_string().contains(BIA));

    let err = with_index(&lib, |tx, idx| {
        name_speaker(
            tx,
            idx,
            &mid(MA),
            sid(1),
            "nobody@example.com",
            Vec::new(),
            MODEL,
        )
    })
    .unwrap_err();
    assert_eq!(err.code(), "not_found");

    let err = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), sid(9), ANA, Vec::new(), MODEL)
    })
    .unwrap_err();
    assert_eq!(err.code(), "not_found");

    let out = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), sid(1), ANA, Vec::new(), MODEL)
    })
    .unwrap();
    assert!(!out.voiceprint_added);
    assert!(
        !lib.person_dir(&pid(ANA))
            .join(paths::VOICEPRINTS_JSON)
            .exists()
    );
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(m.speaker(sid(1)).unwrap().assignment, Assignment::Confirmed);
}

#[test]
fn rejection_persists_through_rebuild_and_confirmed_is_kept() {
    let mut rng = Rng::new(11);
    let ana = Person::new(pid(ANA), "Ana Example");
    let bia = Person::new(pid(BIA), "Bia Sample");
    let mut s1 = spk(1, Some(rng.voice(0, 0.03)));
    s1.assignment = Assignment::Suggested;
    s1.suggestion = Some(Suggestion {
        person_id: pid(ANA),
        score: 0.9,
    });
    // Confirmed as Bia although Ana scores higher: never changed.
    let mut s2 = spk(2, Some(rng.voice(0, 0.03)));
    s2.assignment = Assignment::Confirmed;
    s2.person_id = Some(pid(BIA));
    let m = meeting(MA, 10, vec![s1, s2]);
    let (_tmp, lib) = setup(
        vec![
            (ana, rng.voices(0, 3, 0.03)),
            (bia, vec![tilted(), tilted()]),
        ],
        vec![m],
    );

    let out = with_index(&lib, |tx, idx| {
        reject_suggestion(tx, idx, &mid(MA), sid(1), None, MODEL, None)
    })
    .unwrap();
    assert_eq!(out.rejected, pid(ANA));
    assert_eq!(out.after.assignment, Assignment::Suggested);
    assert_eq!(out.after.person_id, Some(pid(BIA)));

    // Rejecting the confirmed person is refused.
    let err = with_index(&lib, |tx, idx| {
        reject_suggestion(tx, idx, &mid(MA), sid(2), Some(BIA), MODEL, None)
    })
    .unwrap_err();
    assert_eq!(err.code(), "invalid_input");

    lib.write(|tx| Index::rebuild(tx)).unwrap();
    let review = review_queue(&lib, None).unwrap();
    let item = review.iter().find(|r| r.item.speaker_id == sid(1)).unwrap();
    assert_eq!(item.item.suggested_person_id, Some(pid(BIA)));
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(
        m.speaker(sid(1)).unwrap().rejected_person_ids,
        vec![pid(ANA)]
    );

    let changes = with_index(&lib, |tx, idx| re_resolve_all(tx, idx, MODEL, None)).unwrap();
    assert!(changes.is_empty(), "{changes:?}");
    let m = lib.load_meeting(&mid(MA)).unwrap();
    let s2 = m.speaker(sid(2)).unwrap();
    assert_eq!(s2.assignment, Assignment::Confirmed);
    assert_eq!(s2.person_id, Some(pid(BIA)));

    // With thresholds the suggestion drops below `suggest`… still never Ana.
    let strict = Thresholds {
        auto_name: Some(0.99),
        suggest: Some(0.95),
        margin: None,
        merge: None,
        auto_merge: None,
    };
    let changes = with_index(&lib, |tx, idx| {
        re_resolve_all(tx, idx, MODEL, Some(&strict))
    })
    .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].after.assignment, Assignment::None);
    assert_eq!(changes[0].after.person_id, None);
}

fn write_hints(lib: &Library) -> std::path::PathBuf {
    let dir = lib.root().join(paths::QUEUE_DIR).join("j-0123456789abcdef");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(HINTS_JSON);
    let hints = serde_json::json!({
        "schema_version": 1,
        "meeting_id": MB,
        "people": [
            {"person_key": "ana@example.com", "person_id": ANA, "email": "ana@example.com",
             "samples": [{"start": 0.0, "end": 2.0}]},
            {"person_key": "ana example", "person_id": null, "name": "Ana Example", "email": null},
            {"person_key": "bia sample", "person_id": BIA, "name": "Bia Sample", "email": null}
        ],
        "speakers": [
            {"speaker": "S1", "majority_key": "ana example"},
            {"speaker": "S2", "majority_key": "bia sample"}
        ]
    });
    fsutil::write_json_atomic(&path, &hints).unwrap();
    path
}

#[test]
fn forget_voice_clears_centroids_and_hints() {
    let mut rng = Rng::new(3);
    let mut ana = Person::new(pid(ANA), "Ana Example");
    ana.emails.push("Ana@Example.com".into());
    let mut s1 = spk(1, Some(rng.voice(0, 0.03)));
    s1.assignment = Assignment::Confirmed;
    s1.person_id = Some(pid(ANA));
    let mut s2 = spk(2, Some(rng.voice(0, 0.03)));
    s2.assignment = Assignment::Suggested;
    s2.suggestion = Some(Suggestion {
        person_id: pid(ANA),
        score: 0.7,
    });
    let s3 = spk(3, Some(rng.voice(1, 0.03)));
    let (_tmp, lib) = setup(
        vec![(ana, rng.voices(0, 2, 0.03))],
        vec![meeting(MA, 10, vec![s1, s2, s3])],
    );
    let hints = write_hints(&lib);

    let out = with_index(&lib, |tx, idx| forget_voice(tx, idx, "ana@example.com")).unwrap();
    assert!(out.voiceprints_deleted && !out.person_deleted);
    assert_eq!(out.meetings_changed, vec![mid(MA)]);
    assert_eq!(out.hints_removed, 3);
    assert!(
        !lib.person_dir(&pid(ANA))
            .join(paths::VOICEPRINTS_JSON)
            .exists()
    );

    let m = lib.load_meeting(&mid(MA)).unwrap();
    let s1 = m.speaker(sid(1)).unwrap();
    assert!(s1.centroid.is_none());
    assert_eq!(s1.assignment, Assignment::Confirmed);
    let s2 = m.speaker(sid(2)).unwrap();
    assert_eq!(s2.assignment, Assignment::None);
    assert!(s2.suggestion.is_none() && s2.centroid.is_some());
    assert!(m.speaker(sid(3)).unwrap().centroid.is_some());

    let v: Value = fsutil::read_json(&hints).unwrap();
    assert_eq!(v["people"].as_array().unwrap().len(), 1);
    assert_eq!(v["people"][0]["person_id"], BIA);
    assert_eq!(v["speakers"].as_array().unwrap().len(), 1);
    assert_eq!(v["speakers"][0]["speaker"], "S2");
    assert_eq!(v["meeting_id"], MB);

    // Re-resolving cannot bring the forgotten voice back.
    with_index(&lib, |tx, idx| re_resolve_all(tx, idx, MODEL, None)).unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(m.speaker(sid(1)).unwrap().centroid.is_none());
    assert!(
        !lib.person_dir(&pid(ANA))
            .join(paths::VOICEPRINTS_JSON)
            .exists()
    );
}

#[test]
fn forget_person_anonymises() {
    let mut rng = Rng::new(5);
    let ana = Person::new(pid(ANA), "Ana Example");
    let mut s1 = spk(1, Some(rng.voice(0, 0.03)));
    s1.assignment = Assignment::Confirmed;
    s1.person_id = Some(pid(ANA));
    let mut s2 = spk(2, Some(rng.voice(1, 0.03)));
    s2.rejected_person_ids = vec![pid(ANA)];
    let mut m = meeting(MA, 10, vec![s1, s2]);
    m.participants.push(Participant {
        email: Some("ana@example.com".into()),
        person_id: Some(pid(ANA)),
        ..Participant::default()
    });
    let (_tmp, lib) = setup(vec![(ana, rng.voices(0, 2, 0.03))], vec![m]);
    assert!(md(&lib, MA).contains("Ana Example"));
    let hints = write_hints(&lib);

    let out = with_index(&lib, |tx, idx| forget_person(tx, idx, "ana example")).unwrap();
    assert!(out.person_deleted && out.voiceprints_deleted);
    assert_eq!(out.speakers_changed, 2);
    assert!(!lib.person_dir(&pid(ANA)).exists());

    let m = lib.load_meeting(&mid(MA)).unwrap();
    let s1 = m.speaker(sid(1)).unwrap();
    assert_eq!(s1.assignment, Assignment::None);
    assert!(s1.person_id.is_none() && s1.centroid.is_none() && s1.suggestion.is_none());
    assert!(m.speaker(sid(2)).unwrap().rejected_person_ids.is_empty());
    assert!(m.participants[0].person_id.is_none());
    assert!(!md(&lib, MA).contains("Ana Example"));

    // The name-keyed hint entry (no person_id) is gone too.
    let v: Value = fsutil::read_json(&hints).unwrap();
    assert_eq!(v["people"].as_array().unwrap().len(), 1);

    let idx = Index::open_readonly(lib.root()).unwrap();
    let by_person = SearchQuery {
        person: Some(ANA.into()),
        ..SearchQuery::default()
    };
    assert!(idx.search_meetings(&by_person).unwrap().is_empty());

    // Same result after a rebuild from the files.
    lib.write(|tx| Index::rebuild(tx)).unwrap();
    let idx = Index::open_readonly(lib.root()).unwrap();
    assert!(idx.search_meetings(&by_person).unwrap().is_empty());
}

#[test]
fn tags_show_in_index_search() {
    let (_tmp, lib) = setup(Vec::new(), vec![meeting(MA, 10, vec![spk(1, None)])]);
    let tags = with_index(&lib, |tx, idx| {
        tag(
            tx,
            idx,
            &mid(MA),
            &[
                " decision ".into(),
                "Sprint".into(),
                "sprint".into(),
                "".into(),
            ],
            &[],
        )
    })
    .unwrap();
    assert_eq!(tags, vec!["decision".to_owned(), "Sprint".to_owned()]);
    let by_tag = |t: &str| SearchQuery {
        tag: Some(t.into()),
        ..SearchQuery::default()
    };
    let idx = Index::open_readonly(lib.root()).unwrap();
    let hits = idx.search_meetings(&by_tag("decision")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].meeting_id, mid(MA));
    drop(idx);

    let tags = with_index(&lib, |tx, idx| {
        tag(tx, idx, &mid(MA), &[], &["DECISION".into()])
    })
    .unwrap();
    assert_eq!(tags, vec!["Sprint".to_owned()]);
    let idx = Index::open_readonly(lib.root()).unwrap();
    assert!(idx.search_meetings(&by_tag("decision")).unwrap().is_empty());
    assert!(md(&lib, MA).contains("Sprint"));
    drop(idx);

    // The removal is recorded and hides an enrichment tag of the same name.
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(m.tags_removed, vec!["DECISION".to_owned()]);
    lib.write(|tx| {
        let mut m = tx.library().load_meeting(&mid(MA))?;
        m.enrichment = Some(schema::Enrichment {
            provider: "test".into(),
            model: None,
            requested_model: None,
            model_reported: false,
            created_at: schema::now(),
            stale: false,
            revision: String::new(),
            attribution_revision: String::new(),
            line_people: Default::default(),
            summary: String::new(),
            topics: Vec::new(),
            keywords: Vec::new(),
            tags: vec!["decision".into(), "roadmap".into()],
            decisions: Vec::new(),
            actions: Vec::new(),
            instructions: None,
        });
        tx.save_meeting(&m)
    })
    .unwrap();
    let tags = with_index(&lib, |tx, idx| tag(tx, idx, &mid(MA), &[], &[])).unwrap();
    assert_eq!(tags, vec!["Sprint".to_owned(), "roadmap".to_owned()]);

    // `tag +decision` clears the removal.
    let tags = with_index(&lib, |tx, idx| {
        tag(tx, idx, &mid(MA), &["Decision".into()], &[])
    })
    .unwrap();
    assert_eq!(
        tags,
        vec![
            "Sprint".to_owned(),
            "Decision".to_owned(),
            "roadmap".to_owned()
        ]
    );
    assert!(lib.load_meeting(&mid(MA)).unwrap().tags_removed.is_empty());
}

#[test]
fn review_queue_includes_sample_text() {
    let mut m = meeting(MA, 10, vec![spk(1, None), spk(2, None)]);
    m.segments = vec![
        seg(0, 1, 0.0, "first line"),
        seg(1, 2, 3.0, "other speaker"),
        seg(2, 1, 6.0, "second line"),
        seg(3, 1, 9.0, "third line"),
    ];
    let (_tmp, lib) = setup(Vec::new(), vec![m]);
    let review = review_queue(&lib, None).unwrap();
    assert_eq!(review.len(), 2);
    let s1 = review.iter().find(|r| r.item.speaker_id == sid(1)).unwrap();
    assert_eq!(s1.sample_text, vec!["first line", "second line"]);
    let json = serde_json::to_value(s1).unwrap();
    assert_eq!(json["speaker_id"], "S1");
    assert_eq!(json["sample_text"][1], "second line");
}

#[test]
fn review_separates_untranscribed_clusters_without_resolving_them() {
    let mut m = meeting(
        MA,
        10,
        vec![
            spk(1, None),
            spk(2, None),
            spk(3, None),
            spk(4, Some(basis(0))),
        ],
    );
    m.segments = vec![seg(0, 1, 0.0, "yes"), seg(1, 3, 1.0, "")];
    m.segments[0].end = 0.1;
    m.speakers[0].talk_s = 0.1;
    let original = m.clone();
    let (_tmp, lib) = setup(Vec::new(), vec![m.clone()]);
    let rows = review_queue(&lib, Some(1)).unwrap();
    assert_eq!(rows.iter().filter(|r| !r.without_transcript).count(), 1);
    assert_eq!(rows.iter().filter(|r| r.without_transcript).count(), 2);
    assert_eq!(review_naming_count(&lib).unwrap(), 2);
    assert_eq!(lib.load_meeting(&mid(MA)).unwrap(), original);

    // The index still lacks this word: only canonical evidence can classify it.
    m.segments
        .push(seg(2, 2, 4.0, "a word missed by the index"));
    lib.write(|tx| tx.save_meeting(&m)).unwrap();
    let rows = review_queue(&lib, None).unwrap();
    assert!(
        !rows
            .iter()
            .find(|r| r.item.speaker_id == sid(2))
            .unwrap()
            .without_transcript
    );

    for status in [
        ProcessingStatus::Queued,
        ProcessingStatus::Analyzed,
        ProcessingStatus::Provisional,
        ProcessingStatus::Failed,
    ] {
        m.processing.status = status;
        lib.write(|tx| tx.save_meeting(&m)).unwrap();
        assert!(
            review_queue(&lib, None)
                .unwrap()
                .iter()
                .all(|r| !r.without_transcript)
        );
    }
    m.processing.status = ProcessingStatus::Done;
    m.speaker_namespace.push_str("-reanalysis");
    // A reused ID now settled/automatic in the canonical namespace is never
    // moved to quality, even while the index still describes its old identity.
    for assignment in [Assignment::Auto, Assignment::Confirmed] {
        m.speakers[3].assignment = assignment;
        lib.write(|tx| tx.save_meeting(&m)).unwrap();
        assert!(
            review_queue(&lib, None)
                .unwrap()
                .iter()
                .all(|r| !r.without_transcript)
        );
    }
    m.speakers[3].assignment = Assignment::None;
    m.speakers[3].talk_s = 123.0;
    lib.write(|tx| tx.save_meeting(&m)).unwrap();
    let rows = review_queue(&lib, None).unwrap();
    let current = rows.iter().find(|r| r.item.speaker_id == sid(4)).unwrap();
    assert!(current.without_transcript);
    assert_eq!(current.item.talk_s, 123.0);
    m.speakers.retain(|s| s.id != sid(4));
    lib.write(|tx| tx.save_meeting(&m)).unwrap();
    assert!(
        !review_queue(&lib, None)
            .unwrap()
            .iter()
            .find(|r| r.item.speaker_id == sid(4))
            .unwrap()
            .without_transcript
    );
    let dir = lib.list_meeting_dirs().unwrap().pop().unwrap();
    std::fs::write(dir.join(crate::paths::MEETING_JSON), b"not-json").unwrap();
    let rows = review_queue(&lib, None).unwrap();
    assert!(
        rows.iter()
            .all(|r| !r.without_transcript && r.sample_text_error.is_some())
    );
    assert_eq!(review_naming_count(&lib).unwrap(), rows.len());
}

#[test]
fn weak_samples_are_capped() {
    let mut vps = Voiceprints::empty(pid(ANA));
    let m = meeting(MA, 10, Vec::new());
    let prints: Vec<Voiceprint> = (0..MAX_WEAK + 3)
        .map(|i| {
            let mut v = vec![0.0; DIM];
            v[i % DIM] = 1.0;
            new_voiceprint(
                &pid(ANA),
                MODEL,
                &m,
                &v,
                i as f64,
                i as f64 + 1.0,
                VoiceprintKind::Auto,
                Strength::Weak,
            )
        })
        .collect();
    vps.upsert(prints);
    cap_voiceprints(&mut vps, MODEL).unwrap();
    assert_eq!(vps.embeddings.len(), MAX_WEAK);
}

#[test]
fn reconciled_weak_samples_keep_the_meeting_date_so_the_cap_stays_by_age() {
    let mut rng = Rng::new(5);
    let mut s1 = spk(1, Some(rng.voice(0, 0.03)));
    s1.assignment = Assignment::Auto;
    s1.person_id = Some(pid(ANA));
    let a = meeting(MA, 1, vec![s1]);
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(ANA), "Ana Example"), Vec::new())],
        vec![a.clone()],
    );
    // Repairing a missing sample dates it by the meeting.
    lib.write(|tx| sync_weak_samples(tx, &a, &[])).unwrap();
    let vps = lib.load_voiceprints(&pid(ANA)).unwrap();
    assert_eq!(vps.embeddings.len(), 1);
    assert_eq!(vps.embeddings[0].created_at, a.started_at);
    let old_id = vps.embeddings[0].id.clone();

    // Newer weak samples fill the cap and evict it as the oldest.
    let b = meeting(MB, 2, Vec::new());
    lib.write(|tx| {
        let mut vps = tx.library().load_voiceprints(&pid(ANA))?;
        vps.upsert(
            (0..MAX_WEAK)
                .map(|i| {
                    new_voiceprint(
                        &pid(ANA),
                        MODEL,
                        &b,
                        &rng.voice(0, 0.03),
                        i as f64,
                        i as f64 + 1.0,
                        VoiceprintKind::Auto,
                        Strength::Weak,
                    )
                })
                .collect(),
        );
        cap_voiceprints(&mut vps, MODEL)?;
        tx.save_voiceprints(&vps)
    })
    .unwrap();
    let ids = |lib: &Library| -> BTreeSet<String> {
        let vps = lib.load_voiceprints(&pid(ANA)).unwrap();
        vps.embeddings.into_iter().map(|e| e.id).collect()
    };
    let capped = ids(&lib);
    assert_eq!(capped.len(), MAX_WEAK);
    assert!(!capped.contains(&old_id));

    // Reconciling again neither brings it back nor evicts a newer one.
    lib.write(|tx| sync_weak_samples(tx, &a, &[])).unwrap();
    assert_eq!(ids(&lib), capped);
}

#[test]
fn speaker_is_not_scored_against_its_own_auto_sample() {
    let mut rng = Rng::new(21);
    let ana = Person::new(pid(ANA), "Ana Example");
    let s1 = spk(1, Some(rng.voice(0, 0.05)));
    let raw = identity::cosine(&s1.centroid.as_ref().unwrap().vector().unwrap(), &basis(0));
    let (_tmp, lib) = setup(vec![(ana, vec![basis(0)])], vec![meeting(MA, 10, vec![s1])]);
    let loose = Thresholds {
        auto_name: Some(raw - 0.01),
        suggest: Some(0.5),
        margin: None,
        merge: None,
        auto_merge: None,
    };
    with_index(&lib, |tx, idx| re_resolve_all(tx, idx, MODEL, Some(&loose))).unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(m.speaker(sid(1)).unwrap().assignment, Assignment::Auto);
    // The auto-name left a weak sample (the centroid itself) on Ana.
    let vps = lib.load_voiceprints(&pid(ANA)).unwrap();
    assert!(
        vps.embeddings
            .iter()
            .any(|e| e.source.kind == VoiceprintKind::Auto)
    );

    // Above the raw score, the speaker must not lift itself with its own sample.
    let strict = Thresholds {
        auto_name: Some(raw + 0.001),
        ..loose
    };
    with_index(&lib, |tx, idx| {
        re_resolve_all(tx, idx, MODEL, Some(&strict))
    })
    .unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    let s = m.speaker(sid(1)).unwrap();
    assert_eq!(s.assignment, Assignment::Suggested);
    assert_eq!(s.suggestion.as_ref().unwrap().person_id, pid(ANA));
}

#[test]
fn forget_refuses_unreadable_meetings_and_changes_nothing() {
    let mut rng = Rng::new(9);
    let ana = Person::new(pid(ANA), "Ana Example");
    let mut s1 = spk(1, Some(rng.voice(0, 0.03)));
    s1.assignment = Assignment::Confirmed;
    s1.person_id = Some(pid(ANA));
    let (_tmp, lib) = setup(
        vec![(ana, rng.voices(0, 2, 0.03))],
        vec![meeting(MA, 10, vec![s1]), meeting(MB, 11, Vec::new())],
    );
    let bad = lib.meeting_dir(&mid(MB)).unwrap().join(paths::MEETING_JSON);
    fs::write(&bad, "{ not json").unwrap();

    let err = with_index(&lib, |tx, idx| forget_person(tx, idx, ANA)).unwrap_err();
    assert!(err.to_string().contains("unreadable"), "{err}");
    assert!(
        lib.person_dir(&pid(ANA))
            .join(paths::VOICEPRINTS_JSON)
            .exists()
    );
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(m.speaker(sid(1)).unwrap().person_id, Some(pid(ANA)));
    assert_eq!(forgotten_since(&lib, ANA).unwrap(), None);
}

#[test]
fn forget_person_deletes_unreadable_person_and_writes_tombstones() {
    let mut ana = Person::new(pid(ANA), "Ana Example");
    ana.emails.push("Ana@Example.com".into());
    let (_tmp, lib) = setup(vec![(ana, Vec::new())], vec![meeting(MA, 10, Vec::new())]);
    let before = schema::now();
    with_index(&lib, |tx, idx| forget_person(tx, idx, "ana example")).unwrap();
    for key in [ANA, "ana@example.com", " Ana Example "] {
        let t = forgotten_since(&lib, key).unwrap().expect(key);
        assert!(t.timestamp() >= before.timestamp(), "{key}");
    }
    assert_eq!(forgotten_since(&lib, "someone else").unwrap(), None);

    // An unreadable person.md does not stop forget-person by ID.
    let bia = Person::new(pid(BIA), "Bia Sample");
    lib.write(|tx| tx.save_person(&bia)).unwrap();
    let md = lib.person_dir(&pid(BIA)).join(paths::PERSON_MD);
    fs::write(&md, "---\nnot: [valid\n").unwrap();
    let out = with_index(&lib, |tx, idx| forget_person(tx, idx, BIA)).unwrap();
    assert!(out.person_deleted);
    assert!(!lib.person_dir(&pid(BIA)).exists());
}

#[test]
fn forgotten_names_match_caption_spellings() {
    let mut ana = Person::new(pid(ANA), "Dária Solís");
    ana.aliases.push("Niko Example".into());
    let (_tmp, lib) = setup(vec![(ana, Vec::new())], vec![meeting(MA, 10, Vec::new())]);
    with_index(&lib, |tx, idx| forget_person(tx, idx, ANA)).unwrap();
    for name in [
        "Daria Solis",
        "Solis, Daria",
        "SOLÍS, Dária (External)",
        "Niko  Example (Guest)",
    ] {
        assert!(
            name_forgotten_since(&lib, name).unwrap().is_some(),
            "{name}"
        );
    }
    assert_eq!(name_forgotten_since(&lib, "Daria").unwrap(), None);
}

#[test]
fn confirming_one_auto_speaker_does_not_promote_another_speakers_sample() {
    let ana = Person::new(pid(ANA), "Ana Example");
    let m = meeting(MA, 10, vec![spk(1, Some(basis(0))), spk(2, Some(basis(0)))]);
    let (_tmp, lib) = setup(vec![(ana, vec![basis(0)])], vec![m]);
    with_index(&lib, |tx, idx| {
        re_resolve_all(tx, idx, MODEL, Some(&thresholds()))
    })
    .unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(m.speakers.iter().all(|s| s.assignment == Assignment::Auto));
    let second = weak_sample(&m, sid(2), &pid(ANA)).unwrap().unwrap().id;
    let out = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &m.id, sid(1), ANA, Vec::new(), MODEL)
    })
    .unwrap();
    assert_eq!(out.promoted, 1);
    assert_eq!(
        lib.load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .iter()
            .find(|v| v.id == second)
            .unwrap()
            .strength,
        Strength::Weak
    );
    with_index(&lib, |tx, idx| {
        reject_suggestion(
            tx,
            idx,
            &m.id,
            sid(2),
            Some(ANA),
            MODEL,
            Some(&thresholds()),
        )
    })
    .unwrap();
    assert!(
        !lib.load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .iter()
            .any(|v| v.id == second)
    );
}

#[test]
fn correcting_one_speaker_removes_only_its_samples_from_previous_person() {
    for assignment in [Assignment::Auto, Assignment::Confirmed] {
        let mut m = meeting(MA, 10, vec![spk(1, Some(basis(0))), spk(2, Some(basis(1)))]);
        for s in &mut m.speakers {
            s.assignment = assignment;
            s.person_id = Some(pid(ANA));
        }
        let (_tmp, lib) = setup(
            vec![(Person::new(pid(ANA), "Ana Example"), Vec::new())],
            vec![m.clone()],
        );
        let mut prints = Vec::new();
        for speaker in [sid(1), sid(2)] {
            prints.push(weak_sample(&m, speaker, &pid(ANA)).unwrap().unwrap());
            let (start, end) = speaker_span(&m, speaker);
            for kind in [VoiceprintKind::Confirmation, VoiceprintKind::Captions] {
                prints.push(new_voiceprint(
                    &pid(ANA),
                    MODEL,
                    &m,
                    &basis(speaker.number() as usize - 1),
                    start,
                    end,
                    kind,
                    if kind == VoiceprintKind::Confirmation {
                        Strength::Confirmed
                    } else {
                        Strength::Weak
                    },
                ));
            }
        }
        let preserved: BTreeSet<_> = prints
            .iter()
            .filter(|p| p.source.start == 5.0)
            .map(|p| p.id.clone())
            .collect();
        lib.write(|tx| tx.upsert_voiceprints(&pid(ANA), prints))
            .unwrap();
        with_index(&lib, |tx, idx| {
            name_speaker(tx, idx, &m.id, sid(1), "Bia Example", Vec::new(), MODEL)
        })
        .unwrap();
        let remaining: BTreeSet<_> = lib
            .load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(remaining, preserved, "{assignment:?}");
    }
}

#[test]
fn caption_samples_cannot_self_match_and_rejection_removes_them() {
    let mut m = meeting(MA, 10, vec![spk(1, Some(basis(0))), spk(2, Some(basis(1)))]);
    for s in &mut m.speakers {
        s.assignment = Assignment::Auto;
        s.person_id = Some(pid(ANA));
    }
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(ANA), "Ana Example"), Vec::new())],
        vec![m.clone()],
    );
    let prints = [0.0, 5.0]
        .into_iter()
        .map(|start| {
            new_voiceprint(
                &pid(ANA),
                MODEL,
                &m,
                &basis(0),
                start,
                start + 2.0,
                VoiceprintKind::Captions,
                Strength::Weak,
            )
        })
        .collect();
    lib.write(|tx| tx.upsert_voiceprints(&pid(ANA), prints))
        .unwrap();
    let people = load_person_samples(&lib, MODEL).unwrap();
    assert!(people.for_meeting(&m.id).is_empty());
    assert_eq!(people.for_meeting(&mid(MB)).len(), 1);
    with_index(&lib, |tx, idx| {
        reject_suggestion(tx, idx, &m.id, sid(1), Some(ANA), MODEL, None)
    })
    .unwrap();
    let prints = lib.load_voiceprints(&pid(ANA)).unwrap().embeddings;
    assert_eq!(prints.len(), 2); // other speaker's caption and reconciled auto sample
    assert!(prints.iter().all(|p| p.source.start == 5.0));
}

#[test]
fn weak_sample_retains_exact_contributing_turns() {
    let mut m = meeting(MA, 10, vec![spk(1, Some(basis(0))), spk(2, Some(basis(1)))]);
    m.segments = vec![
        seg(0, 1, 0.0, "first"),
        seg(1, 2, 3.0, "other"),
        seg(2, 1, 7.0, "last"),
    ];
    m.speaker_mut(sid(1))
        .unwrap()
        .centroid
        .as_mut()
        .unwrap()
        .ranges = vec![[0.0, 2.0], [7.0, 9.0]];
    let sample = weak_sample(&m, sid(1), &pid(ANA)).unwrap().unwrap();
    assert_eq!((sample.source.start, sample.source.end), (0.0, 9.0));
    assert_eq!(
        sample.source.audio_ranges().collect::<Vec<_>>(),
        vec![[0.0, 2.0], [7.0, 9.0]]
    );
}

#[test]
fn forgetting_deletes_enrollment_audio_under_write_transaction() {
    for anonymise in [false, true] {
        let (_tmp, lib) = setup(
            vec![(Person::new(pid(ANA), "Ana Example"), vec![basis(0)])],
            Vec::new(),
        );
        let dir = lib.person_dir(&pid(ANA)).join("enrollment");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("synthetic.wav"), b"synthetic").unwrap();
        let out = with_index(&lib, |tx, idx| {
            if anonymise {
                forget_person(tx, idx, ANA)
            } else {
                forget_voice(tx, idx, ANA)
            }
        })
        .unwrap();
        assert_eq!(out.person_id, pid(ANA));
        assert!(!dir.exists());
        assert_eq!(lib.person_dir(&pid(ANA)).exists(), !anonymise);
    }
}

#[test]
fn caption_only_people_surface_in_review_even_when_speaker_is_confirmed() {
    let mut person = Person::new(pid(ANA), "Caption Guest");
    person.needs_review = true;
    let mut m = meeting(MA, 10, vec![spk(1, None)]);
    m.speaker_mut(sid(1)).unwrap().assignment = Assignment::Confirmed;
    m.speaker_mut(sid(1)).unwrap().person_id = Some(pid(ANA));
    let (_tmp, lib) = setup(vec![(person, Vec::new())], vec![m]);
    let items = review_queue(&lib, None).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].item.person_id, Some(pid(ANA)));
    with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), sid(1), ANA, Vec::new(), MODEL)
    })
    .unwrap();
    assert!(!lib.load_person(&pid(ANA)).unwrap().needs_review);
    assert!(review_queue(&lib, None).unwrap().is_empty());
}

#[test]
fn benchmark_pool_excludes_all_samples_from_held_out_meeting() {
    let m = meeting(MA, 10, Vec::new());
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(ANA), "Ana Example"), vec![basis(1)])],
        vec![m.clone()],
    );
    let mut prints = Vec::new();
    for strength in [Strength::Confirmed, Strength::Weak] {
        for kind in [
            VoiceprintKind::Auto,
            VoiceprintKind::Captions,
            VoiceprintKind::Hint,
            VoiceprintKind::Enrollment,
            VoiceprintKind::Confirmation,
        ] {
            let start = prints.len() as f64;
            prints.push(new_voiceprint(
                &pid(ANA),
                MODEL,
                &m,
                &basis(0),
                start,
                start + 1.0,
                kind,
                strength,
            ));
        }
    }
    lib.write(|tx| tx.upsert_voiceprints(&pid(ANA), prints))
        .unwrap();
    let people = load_person_samples(&lib, MODEL).unwrap();
    assert_eq!(people.all()[0].confirmed.len(), 6);
    assert_eq!(people.all()[0].weak.len(), 5);
    let held_out = people.for_benchmark(&m.id);
    assert_eq!(held_out.all()[0].confirmed, vec![basis(1)]);
    assert!(held_out.all()[0].weak.is_empty());
    assert_eq!(held_out.for_meeting(&m.id).len(), 1);
}

#[test]
fn meeting_resolver_uses_only_independent_roster_evidence() {
    let cosine = |x: f32| vec![x, (1.0 - x * x).sqrt()];
    let mut m = meeting(MA, 10, vec![spk(1, Some(vec![1.0, 0.0]))]);
    m.participants = vec![
        Participant {
            person_id: Some(pid(ANA)),
            evidence: vec!["captions".into()],
            ..Default::default()
        },
        Participant {
            person_id: Some(pid(BIA)),
            evidence: vec!["calendar".into()],
            ..Default::default()
        },
    ];
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), vec![cosine(0.79)]),
            (Person::new(pid(BIA), "Bia Example"), vec![cosine(0.78)]),
        ],
        Vec::new(),
    );
    let roster = m.roster_person_ids();
    assert_eq!(roster, BTreeSet::from([pid(BIA)]));
    resolve_meeting(
        &mut m,
        MODEL,
        &load_person_samples(&lib, MODEL).unwrap(),
        Some(&thresholds()),
        Some(&roster),
    )
    .unwrap();
    assert_eq!(
        m.speaker(sid(1))
            .unwrap()
            .suggestion
            .as_ref()
            .unwrap()
            .person_id,
        pid(BIA)
    );
    assert_eq!(m.speaker(sid(1)).unwrap().assignment, Assignment::Suggested);
}

#[test]
fn recording_restore_requires_current_missing_snapshot() {
    let mut m = meeting(MA, 10, Vec::new());
    let mut recording = schema::tests::sample_meeting().recording.unwrap();
    recording.state = schema::RecordingState::Missing;
    m.recording = Some(recording.clone());
    let (_tmp, lib) = setup(Vec::new(), vec![m.clone()]);
    let mut stale = recording.clone();
    stale.sha256 = "different".into();
    assert!(
        !with_index(&lib, |tx, idx| restore_recording_available(
            tx, idx, &m.id, &stale
        ))
        .unwrap()
    );
    assert_eq!(
        lib.load_meeting(&m.id).unwrap().recording.unwrap().state,
        schema::RecordingState::Missing
    );
    assert!(
        with_index(&lib, |tx, idx| restore_recording_available(
            tx, idx, &m.id, &recording
        ))
        .unwrap()
    );
    assert_eq!(
        lib.load_meeting(&m.id).unwrap().recording.unwrap().state,
        schema::RecordingState::Available
    );
    assert!(
        !with_index(&lib, |tx, idx| restore_recording_available(
            tx, idx, &m.id, &recording
        ))
        .unwrap()
    );
}

#[test]
fn correction_attributes_legacy_confirmation_ranges_without_precision_loss() {
    let mut m = meeting(MA, 10, vec![spk(1, Some(basis(0))), spk(2, Some(basis(1)))]);
    m.segments[0].start = 0.2;
    m.segments[0].end = 2.2;
    for s in &mut m.speakers {
        s.assignment = Assignment::Confirmed;
        s.person_id = Some(pid(ANA));
    }
    let print = new_voiceprint(
        &pid(ANA),
        MODEL,
        &m,
        &basis(0),
        0.2,
        2.2,
        VoiceprintKind::Confirmation,
        Strength::Confirmed,
    );
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(ANA), "Ana Example"), Vec::new())],
        vec![m.clone()],
    );
    lib.write(|tx| tx.upsert_voiceprints(&pid(ANA), vec![print]))
        .unwrap();
    with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &m.id, sid(1), "Bia Example", Vec::new(), MODEL)
    })
    .unwrap();
    assert!(
        lib.load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .is_empty()
    );
}

/// Alex (speaker 1), Bob (speaker 2), Alex again.
fn interleaved() -> Meeting {
    let mut m = meeting(MA, 10, vec![spk(1, Some(basis(0))), spk(2, Some(basis(1)))]);
    m.segments = vec![
        seg(0, 1, 0.0, "first"),
        seg(1, 2, 3.0, "other"),
        seg(2, 1, 7.0, "last"),
    ];
    m
}

fn weak_print(m: &Meeting, model: &str, kind: VoiceprintKind, ranges: &[[f32; 2]]) -> Voiceprint {
    let mut vp = new_voiceprint(
        &pid(ANA),
        model,
        m,
        &basis(0),
        f64::from(ranges[0][0]),
        f64::from(ranges[ranges.len() - 1][1]),
        kind,
        Strength::Weak,
    );
    vp.source.ranges = ranges.to_vec();
    vp
}

fn rename_speaker_1(lib: &Library, m: &Meeting, to: &str) {
    with_index(lib, |tx, idx| {
        name_speaker(tx, idx, &m.id, sid(1), to, Vec::new(), MODEL)
    })
    .unwrap();
}

#[test]
fn correction_removes_the_speakers_auto_samples_of_every_model() {
    let mut m = interleaved();
    for s in &mut m.speakers {
        s.assignment = Assignment::Auto;
        s.person_id = Some(pid(ANA));
    }
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(ANA), "Ana Example"), Vec::new())],
        vec![m.clone()],
    );
    // Old-model auto samples kept by a model switch: a legacy one (ranges
    // only) and a recorded one, plus the other speaker's.
    let legacy = weak_print(
        &m,
        "old-model",
        VoiceprintKind::Auto,
        &[[0.0, 2.0], [7.0, 9.0]],
    );
    let mut recorded = weak_print(&m, "older-model", VoiceprintKind::Auto, &[[0.0, 2.0]]);
    recorded.source.speaker_id = Some(sid(1));
    let other = weak_print(&m, "old-model", VoiceprintKind::Auto, &[[3.0, 5.0]]);
    let keep = other.id.clone();
    lib.write(|tx| tx.upsert_voiceprints(&pid(ANA), vec![legacy, recorded, other]))
        .unwrap();
    rename_speaker_1(&lib, &m, "Bia Example");
    let left: BTreeSet<_> = lib
        .load_voiceprints(&pid(ANA))
        .unwrap()
        .embeddings
        .into_iter()
        .filter(|v| v.model != MODEL)
        .map(|v| v.id)
        .collect();
    assert_eq!(left, BTreeSet::from([keep]));
}

#[test]
fn caption_samples_of_interleaved_turns_follow_confirmation_and_correction() {
    let m = interleaved();
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(ANA), "Ana Example"), Vec::new())],
        vec![m.clone()],
    );
    // Caption-agreeing ASR spans run past the diarized segments, though
    // never into Bob's turn.
    let mut alex = weak_print(
        &m,
        MODEL,
        VoiceprintKind::Captions,
        &[[0.0, 2.3], [6.8, 9.0]],
    );
    alex.source.speaker_id = Some(sid(1));
    let mut bob = weak_print(&m, MODEL, VoiceprintKind::Captions, &[[3.0, 5.0]]);
    bob.source.speaker_id = Some(sid(2));
    let (alex_id, bob_id) = (alex.id.clone(), bob.id.clone());
    lib.write(|tx| tx.upsert_voiceprints(&pid(ANA), vec![alex, bob]))
        .unwrap();
    let strength = |id: &str| {
        lib.load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .into_iter()
            .find(|v| v.id == id)
            .map(|v| v.strength)
    };

    rename_speaker_1(&lib, &m, "Ana Example");
    assert_eq!(strength(&alex_id), Some(Strength::Confirmed));
    assert_eq!(strength(&bob_id), Some(Strength::Weak));

    rename_speaker_1(&lib, &m, "Bia Example");
    assert_eq!(strength(&alex_id), None);
    assert_eq!(strength(&bob_id), Some(Strength::Weak));
}

#[test]
fn correction_attributes_stale_and_legacy_speaker_ids_by_ranges() {
    for namespace in ["previous-analysis", ""] {
        let mut m = interleaved();
        m.speaker_namespace = "current-analysis".into();
        for speaker in &mut m.speakers {
            speaker.assignment = Assignment::Confirmed;
            speaker.person_id = Some(pid(ANA));
        }
        let (_tmp, lib) = setup(
            vec![(Person::new(pid(ANA), "Ana Example"), Vec::new())],
            vec![m.clone()],
        );
        // The stale IDs are the reverse of the published analysis' labels.
        let mut alex = weak_print(&m, MODEL, VoiceprintKind::Captions, &[[0.0, 2.0]]);
        alex.source.speaker_id = Some(sid(2));
        alex.source.speaker_namespace = namespace.into();
        let mut bob = weak_print(&m, MODEL, VoiceprintKind::Captions, &[[3.0, 5.0]]);
        bob.source.speaker_id = Some(sid(1));
        bob.source.speaker_namespace = namespace.into();
        let keep = bob.id.clone();
        // Even a coincidentally equal auto-sample ID must not bypass stale
        // provenance: its actual contributing ranges belong to Bob.
        let mut auto = weak_sample(&m, sid(1), &pid(ANA)).unwrap().unwrap();
        auto.source.speaker_id = Some(sid(2));
        auto.source.speaker_namespace = namespace.into();
        auto.source.ranges = vec![[3.0, 5.0]];
        let keep_auto = auto.id.clone();
        lib.write(|tx| tx.upsert_voiceprints(&pid(ANA), vec![alex, bob, auto]))
            .unwrap();

        rename_speaker_1(&lib, &m, "Bia Example");
        let left: BTreeSet<_> = lib
            .load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .into_iter()
            .map(|vp| vp.id)
            .collect();
        assert_eq!(
            left,
            BTreeSet::from([keep, keep_auto]),
            "sample namespace {namespace:?}"
        );
    }
}

#[test]
fn remapping_moves_every_persons_meeting_samples_and_clears_unmapped_speakers() {
    let m = interleaved();
    let other = meeting(MB, 11, vec![spk(1, Some(basis(0)))]);
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), Vec::new()),
            (Person::new(pid(BIA), "Bia Example"), Vec::new()),
        ],
        vec![m.clone(), other.clone()],
    );
    let tagged = |mut vp: Voiceprint, person: &str, speaker: Option<u32>| {
        vp.source.speaker_id = speaker.map(sid);
        vp.id = format!("{person}-{}-{speaker:?}", vp.id);
        vp
    };
    let caption = tagged(
        weak_print(&m, MODEL, VoiceprintKind::Captions, &[[0.0, 2.0]]),
        ANA,
        Some(1),
    );
    let mut confirmation = tagged(
        weak_print(&m, "old-model", VoiceprintKind::Confirmation, &[[3.0, 5.0]]),
        ANA,
        Some(2),
    );
    confirmation.strength = Strength::Confirmed;
    // Not an auto sample: those go with their earlier auto naming (see
    // `moved_auto_samples_follow_the_new_resolution`).
    let vanished = tagged(
        weak_print(&m, MODEL, VoiceprintKind::Captions, &[[7.0, 9.0]]),
        ANA,
        Some(3),
    );
    let legacy = tagged(
        weak_print(&m, MODEL, VoiceprintKind::Auto, &[[7.0, 8.0]]),
        ANA,
        None,
    );
    let elsewhere = tagged(
        weak_print(&other, MODEL, VoiceprintKind::Auto, &[[0.0, 2.0]]),
        ANA,
        Some(1),
    );
    let bia = tagged(
        weak_print(&m, MODEL, VoiceprintKind::Captions, &[[3.0, 5.0]]),
        BIA,
        Some(2),
    );
    lib.write(|tx| {
        tx.upsert_voiceprints(
            &pid(ANA),
            vec![caption, confirmation, vanished, legacy, elsewhere],
        )?;
        tx.upsert_voiceprints(&pid(BIA), vec![bia])?;
        Ok(())
    })
    .unwrap();

    let mut migrated = m.clone();
    let published = [sid(1), sid(2), sid(3)];
    let changed = lib
        .write(|tx| {
            migrate_sample_speakers(tx, &mut migrated, &published, |s| match s.number() {
                1 => Some(sid(2)),
                2 => Some(sid(1)),
                _ => None,
            })
        })
        .unwrap();
    assert_eq!(changed, 4);
    assert!(!migrated.speaker_namespace.is_empty());
    let migrated_namespace = migrated.speaker_namespace.clone();
    // Publication did not happen. Retrying from the still-published meeting
    // must not reverse the swap or move the already-cleared sample again.
    let mut retry = lib.load_meeting(&m.id).unwrap();
    assert!(retry.speaker_namespace.is_empty());
    let changed_again = lib
        .write(|tx| {
            migrate_sample_speakers(tx, &mut retry, &[sid(3), sid(2), sid(1)], |s| {
                match s.number() {
                    1 => Some(sid(2)),
                    2 => Some(sid(1)),
                    _ => None,
                }
            })
        })
        .unwrap();
    assert_eq!(changed_again, 0);
    assert_eq!(retry.speaker_namespace, migrated_namespace);
    for person in [ANA, BIA] {
        for vp in lib.load_voiceprints(&pid(person)).unwrap().embeddings {
            if vp.source.meeting_id.as_ref() == Some(&m.id)
                && (vp.source.speaker_id.is_some() || vp.source.ranges == [[7.0, 9.0]])
            {
                assert_eq!(vp.source.speaker_namespace, migrated_namespace);
            } else {
                assert!(vp.source.speaker_namespace.is_empty());
            }
        }
    }
    let speakers = |person: &str| -> Vec<(MeetingId, String, Option<u32>)> {
        lib.load_voiceprints(&pid(person))
            .unwrap()
            .embeddings
            .into_iter()
            .map(|v| {
                (
                    v.source.meeting_id.unwrap(),
                    v.model,
                    v.source.speaker_id.map(SpeakerId::number),
                )
            })
            .collect()
    };
    assert_eq!(
        speakers(ANA),
        vec![
            (mid(MA), MODEL.to_owned(), Some(2)),
            (mid(MA), "old-model".to_owned(), Some(1)),
            (mid(MA), MODEL.to_owned(), None),
            (mid(MA), MODEL.to_owned(), None),
            (mid(MB), MODEL.to_owned(), Some(1)),
        ]
    );
    assert_eq!(speakers(BIA), vec![(mid(MA), MODEL.to_owned(), Some(1))]);
}

#[test]
fn remapping_rewrites_only_the_files_whose_samples_change() {
    let m = interleaved();
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), Vec::new()),
            (Person::new(pid(BIA), "Bia Example"), Vec::new()),
        ],
        vec![m.clone()],
    );
    let of = |speaker: u32, ranges: &[[f32; 2]]| {
        let mut vp = weak_print(&m, MODEL, VoiceprintKind::Captions, ranges);
        vp.source.speaker_id = Some(sid(speaker));
        vp
    };
    lib.write(|tx| {
        tx.upsert_voiceprints(&pid(ANA), vec![of(1, &[[0.0, 2.0]])])?;
        tx.upsert_voiceprints(&pid(BIA), vec![of(2, &[[3.0, 5.0]])])
    })
    .unwrap();
    let path = |person: &str| lib.person_dir(&pid(person)).join(paths::VOICEPRINTS_JSON);
    // Stored compactly, so any rewrite (always pretty-printed) shows.
    let compact = |person: &str| {
        let v: serde_json::Value = fsutil::read_json(&path(person)).unwrap();
        std::fs::write(path(person), serde_json::to_vec(&v).unwrap()).unwrap();
        std::fs::read(path(person)).unwrap()
    };
    let (ana, bia) = (compact(ANA), compact(BIA));
    let published = [sid(1), sid(2)];

    // Every published speaker keeps its ID: nothing changes or is written.
    let mut same = m.clone();
    let changed = lib
        .write(|tx| migrate_sample_speakers(tx, &mut same, &published, Some))
        .unwrap();
    assert_eq!(changed, 0);
    assert_eq!(same.speaker_namespace, m.speaker_namespace);
    assert_eq!(std::fs::read(path(ANA)).unwrap(), ana);
    assert_eq!(std::fs::read(path(BIA)).unwrap(), bia);

    // Speaker 2 vanishes: only Bia's sample changes speaker; Ana's keeps
    // its speaker and takes the new namespace.
    let mut renumbered = m.clone();
    let changed = lib
        .write(|tx| {
            migrate_sample_speakers(tx, &mut renumbered, &published, |s| {
                (s == sid(1)).then_some(s)
            })
        })
        .unwrap();
    assert_eq!(changed, 1);
    assert_ne!(renumbered.speaker_namespace, m.speaker_namespace);
    let sample = |person: &str| lib.load_voiceprints(&pid(person)).unwrap().embeddings[0].clone();
    assert_eq!(sample(ANA).source.speaker_id, Some(sid(1)));
    assert_eq!(
        sample(ANA).source.speaker_namespace,
        renumbered.speaker_namespace
    );
    assert_eq!(sample(BIA).source.speaker_id, None);
    assert_ne!(std::fs::read(path(ANA)).unwrap(), ana);
}

#[test]
fn moved_auto_samples_follow_the_new_resolution() {
    let mut m = interleaved();
    for (speaker, person) in m.speakers.iter_mut().zip([ANA, BIA]) {
        speaker.assignment = Assignment::Auto;
        speaker.person_id = Some(pid(person));
    }
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), Vec::new()),
            (Person::new(pid(BIA), "Bia Example"), Vec::new()),
        ],
        vec![m.clone()],
    );
    // S1 auto-named Ana, S2 Bia, each with its weak auto sample.
    lib.write(|tx| sync_weak_samples(tx, &m, &[])).unwrap();
    let auto_speakers = |person: &str| -> Vec<Option<SpeakerId>> {
        lib.load_voiceprints(&pid(person))
            .unwrap()
            .embeddings
            .into_iter()
            .filter(|v| v.source.kind == VoiceprintKind::Auto)
            .map(|v| v.source.speaker_id)
            .collect()
    };
    assert_eq!(auto_speakers(ANA), [Some(sid(1))]);
    assert_eq!(auto_speakers(BIA), [Some(sid(2))]);

    // A new analysis swaps the speakers. Bia's new speaker already carries
    // her naming; Ana's is not named yet.
    let mut renamed = m.clone();
    for speaker in &mut renamed.speakers {
        let (assignment, person) = if speaker.id == sid(1) {
            (Assignment::Auto, Some(pid(BIA)))
        } else {
            (Assignment::None, None)
        };
        speaker.assignment = assignment;
        speaker.person_id = person;
    }
    lib.write(|tx| {
        migrate_sample_speakers(tx, &mut renamed, &[sid(1), sid(2)], |s| {
            Some(sid(3 - s.number()))
        })
    })
    .unwrap();
    // Ana's sample went with her earlier naming of the old S1; Bia's moved.
    assert!(auto_speakers(ANA).is_empty());
    assert_eq!(auto_speakers(BIA), [Some(sid(1))]);

    // Resolution names the new S2 Bia as well: Ana gets nothing back.
    let mut resolved = renamed.clone();
    let s2 = resolved.speaker_mut(sid(2)).unwrap();
    s2.assignment = Assignment::Auto;
    s2.person_id = Some(pid(BIA));
    let change = SpeakerChange {
        meeting_id: resolved.id.clone(),
        speaker_id: sid(2),
        before: SpeakerState::of(renamed.speaker(sid(2)).unwrap()),
        after: SpeakerState::of(resolved.speaker(sid(2)).unwrap()),
    };
    lib.write(|tx| sync_weak_samples(tx, &resolved, std::slice::from_ref(&change)))
        .unwrap();
    assert!(auto_speakers(ANA).is_empty());
    assert_eq!(auto_speakers(BIA).len(), 2);

    // Resolving it as Ana again instead gives her the speaker's sample back.
    let s2 = resolved.speaker_mut(sid(2)).unwrap();
    s2.person_id = Some(pid(ANA));
    let change = SpeakerChange {
        after: SpeakerState::of(resolved.speaker(sid(2)).unwrap()),
        ..change
    };
    lib.write(|tx| sync_weak_samples(tx, &resolved, &[change]))
        .unwrap();
    assert_eq!(auto_speakers(ANA), [Some(sid(2))]);
}

// ---- line corrections ----

const BIA_NAME: &str = "Bia Example";

/// MA: S1 (confirmed Ana) says segments 0 and 2 in a row, S2 (unnamed) says
/// segment 1 afterwards; Ana has stored voiceprints.
fn corrected_setup() -> (TempDir, Library) {
    let mut rng = Rng::new(11);
    let ana = Person::new(pid(ANA), "Ana Example");
    let mut s1 = spk(1, Some(rng.voice(0, 0.03)));
    s1.assignment = Assignment::Confirmed;
    s1.person_id = Some(pid(ANA));
    let mut m = meeting(MA, 10, vec![s1, spk(2, None)]);
    m.segments.push(seg(2, 1, 2.0, "and another thought"));
    setup(vec![(ana, rng.voices(0, 2, 0.03))], vec![m])
}

fn fingerprints(lib: &Library, ids: &[u32]) -> Vec<SegmentFingerprint> {
    let m = lib.load_meeting(&mid(MA)).unwrap();
    ids.iter()
        .map(|id| segment_fingerprint(m.segments.iter().find(|s| s.id == *id).unwrap()))
        .collect()
}

fn attribute(lib: &Library, ids: &[u32], person: Option<&str>) -> Result<Meeting> {
    let f = fingerprints(lib, ids);
    with_index(lib, |tx, idx| {
        set_utterance_people(tx, idx, &mid(MA), &f, person)
    })
}

fn person_hits(lib: &Library, person: &str) -> Vec<(String, u32, String)> {
    let idx = Index::open_readonly(lib.root()).unwrap();
    idx.search(&SearchQuery {
        person: Some(person.into()),
        ..SearchQuery::default()
    })
    .unwrap()
    .into_iter()
    .map(|h| (h.meeting_id.to_string(), h.segment_id, h.speaker_name))
    .collect()
}

fn queued_jobs(lib: &Library) -> usize {
    match fs::read_dir(lib.root().join(paths::QUEUE_DIR)) {
        Ok(entries) => entries
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.file_name().to_string_lossy().starts_with("j-"))
            })
            .count(),
        Err(_) => 0,
    }
}

#[test]
fn line_correction_names_one_line_without_touching_identity() {
    let (_tmp, lib) = corrected_setup();
    let vp_path = lib.person_dir(&pid(ANA)).join(paths::VOICEPRINTS_JSON);
    let voiceprints = fs::read(&vp_path).unwrap();
    let before = lib.load_meeting(&mid(MA)).unwrap();
    assert!(md(&lib, MA).contains("Ana Example: words of speaker S1 and another thought"));

    let m = attribute(&lib, &[2], Some(BIA_NAME)).unwrap();
    // The typed name created the person.
    let bia_id = find_person(&lib, BIA_NAME).unwrap().unwrap().id;
    assert_eq!(m.utterance_people.len(), 1);
    assert_eq!(m.utterance_people[0].person_id, bia_id);
    // Speakers, namespace, samples and the queue are untouched.
    assert_eq!(m.speakers, before.speakers);
    assert_eq!(m.speaker_namespace, before.speaker_namespace);
    assert_eq!(fs::read(&vp_path).unwrap(), voiceprints);
    assert_eq!(queued_jobs(&lib), 0);
    assert!(
        !lib.person_dir(&bia_id)
            .join(paths::VOICEPRINTS_JSON)
            .exists()
    );

    let who: Vec<(u32, Option<String>, bool)> = effective_attribution(&m)
        .into_iter()
        .map(|a| (a.segment_id, a.person_id.map(|p| p.to_string()), a.line))
        .collect();
    let bia = m.utterance_people[0].person_id.to_string();
    assert_eq!(
        who,
        [
            (0, Some(ANA.to_owned()), false),
            (1, None, false),
            (2, Some(bia.clone()), true)
        ]
    );
    // meeting.md splits the turn and names the line.
    let text = md(&lib, MA);
    assert!(
        text.contains("[00:00:00] Ana Example: words of speaker S1\n"),
        "{text}"
    );
    assert!(
        text.contains("[00:00:02] Bia Example: and another thought\n"),
        "{text}"
    );

    // The index follows: utterances, windows and meeting filters.
    let ana_hits = person_hits(&lib, "ana example");
    let bia_hits = person_hits(&lib, "bia example");
    assert_eq!(ana_hits, [(MA.to_owned(), 0, "Ana Example".to_owned())]);
    assert_eq!(bia_hits, [(MA.to_owned(), 2, BIA_NAME.to_owned())]);
    let idx = Index::open_readonly(lib.root()).unwrap();
    let windows = crate::index::meeting_windows(&m);
    assert_eq!(windows.len(), 3, "the corrected line is its own window");
    assert_eq!(
        windows[1].person_id.as_ref().map(PersonId::to_string),
        Some(bia.clone())
    );
    assert_eq!(
        idx.window_revision(&mid(MA)).unwrap(),
        Some(crate::index::window_revision(&windows))
    );
    let meetings = idx
        .search_meetings(&SearchQuery {
            person: Some("bia example".into()),
            ..SearchQuery::default()
        })
        .unwrap();
    assert_eq!(meetings.len(), 1);
    drop(idx);

    // A rebuild from the files gives the same rows.
    lib.write(|tx| Index::rebuild(tx)).unwrap();
    assert_eq!(person_hits(&lib, "ana example"), ana_hits);
    assert_eq!(person_hits(&lib, "bia example"), bia_hits);
    assert_eq!(
        Index::open_readonly(lib.root())
            .unwrap()
            .window_revision(&mid(MA))
            .unwrap(),
        Some(crate::index::window_revision(&windows))
    );

    // Clearing restores the speaker's attribution.
    let m = attribute(&lib, &[2], None).unwrap();
    assert!(m.utterance_people.is_empty());
    assert!(md(&lib, MA).contains("Ana Example: words of speaker S1 and another thought"));
    assert!(person_hits(&lib, "bia example").is_empty());
    assert_eq!(fs::read(&vp_path).unwrap(), voiceprints);
    assert_eq!(queued_jobs(&lib), 0);
}

#[test]
fn line_corrections_are_refused_before_done_while_enriching_or_when_stale() {
    let (_tmp, lib) = corrected_setup();
    let set_status = |status: ProcessingStatus| {
        lib.write(|tx| {
            tx.update_meeting(&mid(MA), |m| {
                m.processing.status = status;
                Ok(())
            })
            .map(|_| ())
        })
        .unwrap();
    };
    set_status(ProcessingStatus::Analyzed);
    let err = attribute(&lib, &[2], Some(ANA)).unwrap_err();
    assert_eq!(err.code(), "library_busy");
    set_status(ProcessingStatus::Done);

    // An enrichment of the meeting is queued: refused until it ends.
    let queue = crate::queue::Queue::open(lib.root()).unwrap();
    let job = lib
        .write(|tx| {
            queue.enqueue(
                tx,
                crate::queue::ENRICH_KIND,
                MA,
                serde_json::json!({ "meeting_id": MA }),
            )
        })
        .unwrap();
    let err = attribute(&lib, &[2], Some(ANA)).unwrap_err();
    assert_eq!(err.code(), "library_busy");
    lib.write(|tx| queue.set_state(tx, &job, crate::queue::JobState::Done, None))
        .unwrap();
    attribute(&lib, &[2], Some(ANA)).unwrap();

    // A fingerprint that no longer matches a segment changes nothing.
    let mut stale = fingerprints(&lib, &[1]);
    stale[0].text_sha256 = schema::sha256_hex(b"words said before reprocessing");
    let err = with_index(&lib, |tx, idx| {
        set_utterance_people(tx, idx, &mid(MA), &stale, Some(BIA_NAME))
    })
    .unwrap_err();
    assert_eq!(err.code(), "invalid_input");
    assert!(err.to_string().contains("the transcript changed; reload"));
    let mut moved = fingerprints(&lib, &[1]);
    moved[0].start += 0.001;
    let err = with_index(&lib, |tx, idx| {
        set_utterance_people(tx, idx, &mid(MA), &moved, Some(BIA_NAME))
    })
    .unwrap_err();
    assert_eq!(err.code(), "invalid_input");
    assert!(
        find_person(&lib, BIA_NAME).unwrap().is_none(),
        "no person created"
    );
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(m.utterance_people.len(), 1);
    assert_eq!(m.utterance_people[0].person_id, pid(ANA));
    // An unknown e-mail is not a new person.
    let err = attribute(&lib, &[1], Some("nobody@example.com")).unwrap_err();
    assert_eq!(err.code(), "not_found");
}

#[test]
fn forgetting_a_person_removes_their_line_corrections_and_line_references() {
    let (_tmp, lib) = corrected_setup();
    attribute(&lib, &[2], Some(BIA_NAME)).unwrap();
    attribute(&lib, &[1], Some(ANA)).unwrap();
    let bia = find_person(&lib, BIA_NAME).unwrap().unwrap().id;
    lib.write(|tx| {
        tx.update_meeting(&mid(MA), |m| {
            m.enrichment = Some(schema::Enrichment {
                provider: "test".into(),
                model: None,
                requested_model: None,
                model_reported: false,
                created_at: schema::now(),
                stale: false,
                revision: crate::index::meeting_window_revision(m),
                attribution_revision: attribution_revision(m),
                line_people: [("P1".to_owned(), bia.clone()), ("P2".to_owned(), pid(ANA))]
                    .into_iter()
                    .collect(),
                summary: "{{P1}} agreed with {{P2}}.".into(),
                topics: Vec::new(),
                keywords: Vec::new(),
                tags: Vec::new(),
                decisions: vec![schema::Decision {
                    id: "d1".into(),
                    text: "{{P1}} decides".into(),
                    at_s: 0.0,
                    by_speaker: None,
                    by_name: Some("P1".into()),
                }],
                actions: Vec::new(),
                instructions: None,
            });
            Ok(())
        })
        .map(|_| ())
    })
    .unwrap();
    assert!(md(&lib, MA).contains("Bia Example agreed with Ana Example."));
    assert!(!md(&lib, MA).contains("predates"));
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(meeting_references_person(&m, &bia) && meeting_references_person(&m, &pid(ANA)));
    assert!(!meeting_references_person(&m, &pid(BIA)));

    with_index(&lib, |tx, idx| forget_person(tx, idx, BIA_NAME)).unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(m.utterance_people.len(), 1);
    assert_eq!(m.utterance_people[0].person_id, pid(ANA));
    assert!(!meeting_references_person(&m, &bia));
    let e = m.enrichment.as_ref().unwrap();
    assert_eq!(e.line_people.keys().collect::<Vec<_>>(), ["P2"]);
    // The label itself is gone, so it can never name someone else later.
    assert_eq!(e.summary, "Unknown agreed with {{P2}}.");
    // Markdown, the MCP resource (same renderer) and the index say stale.
    assert!(e.stale && enrichment_is_stale(&m));
    let text = md(&lib, MA);
    assert!(text.contains("Unknown agreed with Ana Example."), "{text}");
    assert!(
        text.contains("predates changes to the transcript"),
        "{text}"
    );
    assert!(!text.contains(BIA_NAME), "{text}");
    assert!(person_hits(&lib, bia.as_str()).is_empty());
    let idx = Index::open_readonly(lib.root()).unwrap();
    let decisions = idx
        .list_decisions(&crate::index::DecisionQuery::default())
        .unwrap();
    assert_eq!(decisions.len(), 1);
    assert!(decisions[0].enrichment_stale);
    assert_eq!(decisions[0].text, "Unknown decides");
    assert_eq!(decisions[0].by.as_deref(), Some("Unknown"));
    assert_eq!(decisions[0].by_person_id, None);
}

#[test]
fn line_corrections_change_the_attribution_revision_and_stale_the_enrichment() {
    // A one-segment meeting: windows cannot split, only the attribution changes.
    let (_tmp, lib) = setup(Vec::new(), vec![meeting(MA, 10, vec![spk(1, None)])]);
    lib.write(|tx| tx.save_person(&Person::new(pid(ANA), "Ana Example")))
        .unwrap();
    lib.write(|tx| tx.save_person(&Person::new(pid(BIA), BIA_NAME)))
        .unwrap();
    let enrich_now = || {
        lib.write(|tx| {
            tx.update_meeting(&mid(MA), |m| {
                m.enrichment = Some(schema::Enrichment {
                    provider: "test".into(),
                    model: None,
                    requested_model: None,
                    model_reported: false,
                    created_at: schema::now(),
                    stale: false,
                    revision: crate::index::meeting_window_revision(m),
                    attribution_revision: attribution_revision(m),
                    line_people: Default::default(),
                    summary: "{{S1}} spoke.".into(),
                    topics: Vec::new(),
                    keywords: Vec::new(),
                    tags: Vec::new(),
                    decisions: Vec::new(),
                    actions: Vec::new(),
                    instructions: None,
                });
                Ok(())
            })
            .map(|_| ())
        })
        .unwrap();
        assert!(!enrichment_is_stale(&lib.load_meeting(&mid(MA)).unwrap()));
    };
    enrich_now();
    let before = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(attribution_revision(&before), "");

    let m = attribute(&lib, &[0], Some(ANA)).unwrap();
    assert_eq!(
        crate::index::meeting_window_revision(&m),
        crate::index::meeting_window_revision(&before)
    );
    assert_ne!(attribution_revision(&m), "");
    assert!(enrichment_is_stale(&m) && m.enrichment.as_ref().unwrap().stale);
    assert!(md(&lib, MA).contains("predates changes to the transcript"));

    // A → B on the same line stales a fresh enrichment again.
    enrich_now();
    let a = attribution_revision(&lib.load_meeting(&mid(MA)).unwrap());
    let m = attribute(&lib, &[0], Some(BIA)).unwrap();
    assert_ne!(attribution_revision(&m), a);
    assert!(enrichment_is_stale(&m));
    // Setting the same person again writes nothing.
    let again = attribute(&lib, &[0], Some(BIA)).unwrap();
    assert_eq!(again.updated_at, m.updated_at);
}

fn entry(seg: &Segment, person: &str) -> schema::UtterancePerson {
    let f = segment_fingerprint(seg);
    schema::UtterancePerson {
        track: f.track,
        start: f.start,
        end: f.end,
        text_sha256: f.text_sha256,
        person_id: pid(person),
        at: schema::now(),
        live_decision: None,
    }
}

#[test]
fn reprocessing_moves_line_corrections_by_overlap_and_text_or_drops_them() {
    let mut m = meeting(MA, 10, vec![spk(1, None), spk(2, None), spk(1, None)]);
    m.segments[2].text = "third line".into();
    m.utterance_people = vec![
        entry(&m.segments[0], ANA),
        entry(&m.segments[1], BIA),
        entry(&m.segments[2], ANA),
    ];
    // New analysis: renumbered IDs; segment 0 unchanged, segment 1 shifted
    // by under half its length, segment 2 with other words.
    let mut new = m.segments.clone();
    for (i, s) in new.iter_mut().enumerate() {
        s.id = 10 + i as u32;
        s.speaker = None;
    }
    new[1].start += 0.8;
    new[1].end += 0.8;
    new[2].text = "third line, reworded".into();
    m.segments = new;
    let notes = reapply_utterance_people(&mut m);
    let kept: Vec<(u32, String)> = effective_attribution(&m)
        .into_iter()
        .filter(|a| a.line)
        .map(|a| (a.segment_id, a.person_id.unwrap().to_string()))
        .collect();
    assert_eq!(kept, [(10, ANA.to_owned()), (11, BIA.to_owned())]);
    assert_eq!(m.utterance_people[1].start, m.segments[1].start);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains(ANA) && notes[0].contains("00:00:10"),
        "{notes:?}"
    );

    // Shifted by more than half of either span: dropped.
    let mut far = m.clone();
    far.segments[1].start += 1.5;
    far.segments[1].end += 1.5;
    let notes = reapply_utterance_people(&mut far);
    assert_eq!(notes.len(), 1);
    assert_eq!(far.utterance_people.len(), 1);
}

/// [`voice_matches`] for `s` as a speaker of `m` (replacing the speaker
/// with its ID, or added to the meeting).
fn matches_of(
    people: &PeopleSamples,
    m: &Meeting,
    s: &Speaker,
    roster: Option<&BTreeSet<PersonId>>,
    t: Option<&Thresholds>,
    model: &str,
    limit: usize,
) -> Result<Vec<VoiceMatch>> {
    let mut m = m.clone();
    match m.speaker_mut(s.id) {
        Some(slot) => *slot = s.clone(),
        None => m.speakers.push(s.clone()),
    }
    voice_matches(people, &m, s.id, roster, t, model, limit)
}

#[test]
fn voice_matches_top_is_resolve_pick_with_and_without_roster() {
    let cid = "p-c3c3c3";
    let mut rng = Rng::new(31);
    let speakers: Vec<Speaker> = (1..=6)
        .map(|n| spk(n, Some(rng.voice((n % 3) as usize, 0.35))))
        .collect();
    let mut m = meeting(MA, 10, speakers);
    m.participants = vec![Participant {
        person_id: Some(pid(BIA)),
        evidence: vec!["calendar".into()],
        ..Default::default()
    }];
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), rng.voices(0, 3, 0.35)),
            (Person::new(pid(BIA), "Bia Example"), rng.voices(1, 3, 0.35)),
            (Person::new(pid(cid), "Cid Example"), rng.voices(2, 3, 0.35)),
        ],
        Vec::new(),
    );
    let people = load_person_samples(&lib, MODEL).unwrap();
    let roster = m.roster_person_ids();
    assert_eq!(roster, BTreeSet::from([pid(BIA)]));
    let t = Thresholds {
        margin: Some(0.06),
        ..thresholds()
    };
    for roster in [None, Some(&roster)] {
        for t in [None, Some(&t)] {
            let mut resolved = m.clone();
            resolve_meeting(&mut resolved, MODEL, &people, t, roster).unwrap();
            for s in &m.speakers {
                let matches = matches_of(&people, &m, s, roster, t, MODEL, 3).unwrap();
                assert_eq!(matches.len(), 3);
                let after = SpeakerState::of(resolved.speaker(s.id).unwrap());
                let res = identity::resolve(
                    &s.centroid.as_ref().unwrap().vector().unwrap(),
                    &people.for_meeting(&m.id),
                    &[],
                    t,
                    roster,
                );
                assert_eq!(Some(&matches[0].person_id), res.person_id.as_ref());
                assert_eq!(Some(matches[0].score), res.score);
                assert_eq!(matches[0].likely, res.assignment == Assignment::Auto);
                if after.assignment != Assignment::None {
                    assert_eq!(after.person_id.as_ref(), Some(&matches[0].person_id));
                }
                assert!(matches[1..].iter().all(|v| !v.likely));
            }
        }
    }
    let mut rejecting = m.speakers[0].clone();
    let top = matches_of(&people, &m, &rejecting, None, None, MODEL, 3).unwrap()[0]
        .person_id
        .clone();
    rejecting.rejected_person_ids.push(top.clone());
    let matches = matches_of(&people, &m, &rejecting, None, None, MODEL, 3).unwrap();
    assert_eq!(matches.len(), 2);
    assert!(matches.iter().all(|v| v.person_id != top));
    let silent = spk(9, None);
    assert!(
        matches_of(&people, &m, &silent, None, None, MODEL, 3)
            .unwrap()
            .is_empty()
    );
    assert!(
        matches_of(&people, &m, &m.speakers[0], None, None, "other-model", 3)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn voice_matches_likely_needs_auto_name_and_margin_over_the_full_pool() {
    let cosine = |x: f32| vec![x, (1.0 - x * x).sqrt()];
    let mut s = spk(1, Some(vec![1.0, 0.0]));
    s.assignment = Assignment::Suggested;
    s.suggestion = Some(Suggestion {
        person_id: pid(BIA),
        score: 0.93,
    });
    let m = meeting(MA, 10, vec![s.clone()]);
    let lib_with = |bia: f32| {
        setup(
            vec![
                (Person::new(pid(ANA), "Ana Example"), vec![cosine(0.95)]),
                (Person::new(pid(BIA), "Bia Example"), vec![cosine(bia)]),
            ],
            Vec::new(),
        )
    };
    let t = thresholds();
    let likely = |people: &PeopleSamples, t: Option<&Thresholds>, limit| {
        matches_of(people, &m, &s, None, t, MODEL, limit)
            .unwrap()
            .iter()
            .map(|v| v.likely)
            .collect::<Vec<_>>()
    };

    // Two near ties above auto_name: no badge, even when only one is shown.
    let (_tmp, lib) = lib_with(0.93);
    let people = load_person_samples(&lib, MODEL).unwrap();
    assert_eq!(likely(&people, Some(&t), 3), [false, false]);
    assert_eq!(likely(&people, Some(&t), 1), [false]);
    let matches = matches_of(&people, &m, &s, None, Some(&t), MODEL, 3).unwrap();
    assert_eq!(
        matches
            .iter()
            .map(|v| (v.person_id.clone(), v.current_suggestion))
            .collect::<Vec<_>>(),
        [(pid(ANA), false), (pid(BIA), true)]
    );
    assert!((matches[0].score - 0.95).abs() < 1e-5);

    // A clear lead: the top row only, also behind --limit 1; never without thresholds.
    let (_tmp, lib) = lib_with(0.5);
    let people = load_person_samples(&lib, MODEL).unwrap();
    assert_eq!(likely(&people, Some(&t), 3), [true, false]);
    assert_eq!(likely(&people, Some(&t), 1), [true]);
    assert_eq!(likely(&people, None, 3), [false, false]);
    let suggest_only = Thresholds {
        auto_name: None,
        ..t
    };
    assert_eq!(likely(&people, Some(&suggest_only), 3), [false, false]);
}

#[test]
fn confirmed_speaker_does_not_match_its_own_samples() {
    let mut s1 = spk(1, Some(basis(0)));
    s1.assignment = Assignment::Confirmed;
    s1.person_id = Some(pid(ANA));
    let m = meeting(MA, 10, vec![s1.clone()]);
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), vec![tilted()]),
            (Person::new(pid(BIA), "Bia Example"), vec![basis(1)]),
        ],
        vec![m.clone()],
    );
    let own = new_voiceprint(
        &pid(ANA),
        MODEL,
        &m,
        &basis(0),
        0.0,
        2.0,
        VoiceprintKind::Confirmation,
        Strength::Confirmed,
    );
    lib.write(|tx| tx.upsert_voiceprints(&pid(ANA), vec![own]))
        .unwrap();
    let people = load_person_samples(&lib, MODEL).unwrap();
    let tilt = identity::cosine(&basis(0), &tilted());

    let matches = matches_of(&people, &m, &s1, None, None, MODEL, 3).unwrap();
    assert_eq!(matches[0].person_id, pid(ANA));
    assert!(
        (matches[0].score - tilt).abs() < 1e-5,
        "only samples from other meetings: {}",
        matches[0].score
    );
    assert!(matches.iter().all(|v| !v.current_suggestion));

    // The same voice unconfirmed is matched against the meeting's pool, own sample included.
    let mut open = s1;
    open.assignment = Assignment::None;
    open.person_id = None;
    let matches = matches_of(&people, &m, &open, None, None, MODEL, 3).unwrap();
    assert!((matches[0].score - (1.0 + tilt) / 2.0).abs() < 1e-5);
}

#[test]
fn forgetting_requests_reevaluation_for_every_affected_model() {
    let mut rng = Rng::new(3);
    let mut s1 = spk(1, Some(rng.voice(0, 0.03)));
    s1.assignment = Assignment::Auto;
    s1.person_id = Some(pid(ANA));
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(ANA), "Ana Example"), rng.voices(0, 2, 0.03))],
        vec![meeting(MA, 10, vec![s1])],
    );
    // A pending pass of another model must not keep the forgotten voice.
    lib.write(|tx| crate::learning::request(tx, "other-embedder", Some(&mid(MB))))
        .unwrap();
    let out = with_index(&lib, |tx, idx| forget_person(tx, idx, ANA)).unwrap();
    let jobs: Vec<_> = out.learning.iter().map(|r| r.job_id.clone()).collect();
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert!(jobs.contains(&crate::learning::job_id(MODEL)));
    assert!(out.learning.iter().all(|r| r.source_meeting.is_none()));
    let mine = crate::learning::status(&lib, MODEL).unwrap().unwrap();
    assert_eq!(mine.requested, 1);
    assert!(mine.source_meetings.is_empty());
    let other = crate::learning::status(&lib, "other-embedder")
        .unwrap()
        .unwrap();
    assert_eq!(other.requested, 2);
    assert_eq!(other.source_meetings, vec![mid(MB)]);
}

#[test]
fn a_line_only_correction_requests_no_reevaluation() {
    let (_tmp, lib) = corrected_setup();
    attribute(&lib, &[2], Some(BIA_NAME)).unwrap();
    assert!(crate::learning::status(&lib, MODEL).unwrap().is_none());
    // Naming the speaker does.
    let out = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), sid(2), BIA_NAME, Vec::new(), MODEL)
    })
    .unwrap();
    assert_eq!(out.learning.generation, 1);
    assert_eq!(out.learning.source_meeting, Some(mid(MA)));
}

/// The picker's "likely" badge is the publication policy: a voice known
/// only from weak samples is no likely match although the ordinary pool
/// would auto-name it, and resolution then suggests rather than names.
#[test]
fn likely_and_resolution_follow_the_confirmed_evidence_gate() {
    let s = spk(1, Some(basis(0)));
    let m = meeting(MA, 10, vec![s.clone()]);
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(ANA), "Ana Example"), Vec::new())],
        Vec::new(),
    );
    let other = meeting(MB, 11, Vec::new());
    let weak: Vec<Voiceprint> = [0.0, 3.0]
        .into_iter()
        .map(|start| {
            new_voiceprint(
                &pid(ANA),
                MODEL,
                &other,
                &basis(0),
                start,
                start + 2.0,
                VoiceprintKind::Captions,
                Strength::Weak,
            )
        })
        .collect();
    lib.write(|tx| tx.upsert_voiceprints(&pid(ANA), weak))
        .unwrap();
    let people = load_person_samples(&lib, MODEL).unwrap();
    let t = thresholds();
    let ordinary = identity::resolve(&basis(0), &people.for_meeting(&m.id), &[], Some(&t), None);
    assert_eq!(ordinary.assignment, Assignment::Auto);
    let matches = matches_of(&people, &m, &s, None, Some(&t), MODEL, 3).unwrap();
    assert_eq!(matches[0].person_id, pid(ANA));
    assert!(!matches[0].likely);
    let mut resolved = m.clone();
    resolve_meeting(&mut resolved, MODEL, &people, Some(&t), None).unwrap();
    let r = resolved.speaker(sid(1)).unwrap();
    assert_eq!(r.assignment, Assignment::Suggested);
    assert_eq!(r.suggestion.as_ref().unwrap().person_id, pid(ANA));
    // Already auto-named Ana: kept, and shown as likely.
    let mut named = s.clone();
    named.assignment = Assignment::Auto;
    named.person_id = Some(pid(ANA));
    let matches = matches_of(&people, &m, &named, None, Some(&t), MODEL, 3).unwrap();
    assert!(matches[0].likely);
}

mod global_identity;
mod grouping;
mod live;
mod person;
mod review;
mod templates;
mod transcript;
