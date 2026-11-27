use std::path::Path;
use std::sync::Mutex;

use broscribe_core::config::Thresholds;
use broscribe_core::queue::IMPORT_KIND;
use broscribe_core::schema::{Assignment, Person};
use broscribe_core::{Library, SpeakerId};
use broscribe_models::SpeakerEmbedder;
use chrono::DateTime;

use super::*;
use crate::import::{ImportOptions, import_file};
use crate::manifest::{ManifestParticipant, ManifestPerson, SpeakerHint};
use crate::testutil::{Fixture, models};

const MODEL: &str = "nemo-titanet-large";

fn sid(n: u32) -> SpeakerId {
    SpeakerId::new(n).unwrap()
}

fn hint(name: Option<&str>, is_self: bool, ranges: &[(f64, f64)]) -> SpeakerHint {
    SpeakerHint {
        name: name.map(str::to_owned),
        email: None,
        is_self,
        ranges: ranges.to_vec(),
    }
}

fn entry(i: usize, path: &Path, day: u32, hints: Vec<SpeakerHint>) -> ManifestMeeting {
    ManifestMeeting {
        index: i,
        audio_path: path.to_owned(),
        sources: Vec::new(),
        per_participant: false,
        source_id: Some(format!("other-tool-{i}")),
        title: Some(format!("Sync {i}")),
        started_at: Some(
            DateTime::parse_from_rfc3339(&format!("2026-10-{day:02}T10:00:00Z")).unwrap(),
        ),
        ended_at: None,
        duration_s: None,
        language: Some("en".into()),
        participants: Vec::new(),
        hints,
    }
}

fn manifest(people: Vec<ManifestPerson>, meetings: Vec<ManifestMeeting>) -> Manifest {
    Manifest {
        id: "test-batch".into(),
        generated_at: None,
        people,
        meetings,
        issues: Vec::new(),
    }
}

fn calibrated(fx: &mut Fixture, merge: Option<f32>) {
    fx.config.identity.insert(
        MODEL.into(),
        Thresholds {
            auto_name: Some(0.9),
            suggest: Some(0.5),
            margin: Some(0.1),
            merge,
            // Re-finishing a `done` meeting never groups, whatever the threshold.
            auto_merge: merge,
        },
    );
}

/// Voices: 0 = the user, 1 = Ana, 2 = Bea, 3 = someone never hinted.
/// Meeting 0 (earlier): Ana and Bea without hints. Meeting 1 (later): the
/// user, Ana and Bea, all hinted.
fn two_meetings(fx: &Fixture) -> Manifest {
    let early = fx.wav(
        "early.wav",
        &[(Some(1), 4.0), (Some(2), 4.0), (Some(1), 2.0)],
    );
    let later = fx.wav(
        "later.wav",
        &[(Some(0), 4.0), (Some(1), 4.0), (Some(2), 4.0)],
    );
    manifest(
        vec![ManifestPerson {
            name: Some("Ana Example".into()),
            email: Some("ana@example.com".into()),
        }],
        vec![
            entry(0, &early, 1, Vec::new()),
            entry(
                1,
                &later,
                2,
                vec![
                    hint(None, true, &[(0.0, 4.0)]),
                    hint(Some("Ana Example"), false, &[(4.0, 8.0)]),
                    hint(Some("Bea Example"), false, &[(8.0, 12.0)]),
                ],
            ),
        ],
    )
}

/// Meetings, people, voiceprints and named speakers in the library.
fn counts(lib: &Library) -> (usize, usize, usize, Vec<(Assignment, Option<String>)>) {
    let people = lib.list_people().unwrap();
    let prints = people
        .iter()
        .map(|p| lib.load_voiceprints(&p.id).unwrap().embeddings.len())
        .sum();
    let mut named = Vec::new();
    for dir in lib.list_meeting_dirs().unwrap() {
        let m = lib.load_meeting_at(&dir).unwrap();
        assert_eq!(m.processing.status, ProcessingStatus::Done);
        for s in &m.speakers {
            let name = s.named_person().map(|p| lib.load_person(p).unwrap().name);
            named.push((s.assignment, name));
        }
    }
    named.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    (
        lib.list_meeting_dirs().unwrap().len(),
        people.len(),
        prints,
        named,
    )
}

fn meeting_by_source(lib: &Library, source_id: &str) -> Meeting {
    lib.list_meeting_dirs()
        .unwrap()
        .into_iter()
        .map(|d| lib.load_meeting_at(&d).unwrap())
        .find(|m| m.source_id.as_deref() == Some(source_id))
        .unwrap()
}

fn queue_jobs(lib: &Library) -> usize {
    Queue::open(lib.root())
        .unwrap()
        .list(MANIFEST_KIND)
        .unwrap()
        .len()
}

#[test]
fn hint_in_a_later_meeting_names_a_speaker_in_an_earlier_one() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let man = two_meetings(&fx);
    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert_eq!(s.analyzed, 2);
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.people_seeded, 1);
    // Me and Bea are created from hints; Ana was seeded.
    assert_eq!(s.people_created, 2);
    assert_eq!(
        s.people.iter().filter(|p| p.enrolled).count(),
        3,
        "{:?}",
        s.people
    );
    assert!(s.notes.is_empty());

    let ana = fx.lib.find_person_by_name("Ana Example").unwrap().unwrap();
    let bea = fx.lib.find_person_by_name("Bea Example").unwrap().unwrap();
    let me = fx.lib.self_person().unwrap().unwrap();
    assert_eq!(me.name, "Me");
    let prints = fx.lib.load_voiceprints(&ana.id).unwrap();
    let hints: Vec<_> = prints
        .embeddings
        .iter()
        .filter(|e| e.source.kind == VoiceprintKind::Hint)
        .collect();
    assert_eq!(hints.len(), 2);
    assert!(
        hints
            .iter()
            .all(|e| e.strength == Strength::Confirmed && e.model == MODEL)
    );
    // Auto-names add weak samples.
    assert!(
        prints
            .embeddings
            .iter()
            .any(|e| e.source.kind == VoiceprintKind::Auto && e.strength == Strength::Weak)
    );

    let early = meeting_by_source(&fx.lib, "other-tool-0");
    let s1 = early.speaker(sid(1)).unwrap();
    assert_eq!(
        (s1.assignment, s1.person_id.as_ref()),
        (Assignment::Auto, Some(&ana.id))
    );
    let s2 = early.speaker(sid(2)).unwrap();
    assert_eq!(
        (s2.assignment, s2.person_id.as_ref()),
        (Assignment::Auto, Some(&bea.id))
    );
    let md = std::fs::read_to_string(
        fx.lib
            .meeting_dir(&early.id)
            .unwrap()
            .join(broscribe_core::paths::MEETING_MD),
    )
    .unwrap();
    assert!(md.contains("Ana Example:"), "{md}");

    // Completed batch: none of its jobs (nor their hints) remain.
    assert_eq!(queue_jobs(&fx.lib), 0);
    assert!(s.jobs_removed >= 2);

    // Running it again changes nothing.
    let before = counts(&fx.lib);
    let again = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert_eq!(again.analyzed, 0);
    assert_eq!(again.existing, 2);
    assert_eq!(counts(&fx.lib), before);
    assert_eq!(queue_jobs(&fx.lib), 0);
}

#[test]
fn interrupted_batches_resume_with_identical_counts() {
    let mut reference = Fixture::new();
    calibrated(&mut reference, None);
    let man = two_meetings(&reference);
    import_manifest(
        &reference.env(),
        &mut models(),
        &man,
        &BatchOptions::default(),
    )
    .unwrap();
    let expected = counts(&reference.lib);

    for point in [
        FailPoint::AfterHints,
        FailPoint::AfterPhase1,
        FailPoint::AfterPhase2,
    ] {
        let mut fx = Fixture::new();
        calibrated(&mut fx, None);
        let man = two_meetings(&fx);
        FAIL_AT.set(Some(point));
        let err =
            import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
        assert!(is_failpoint(&err), "{point:?}: {err}");
        if point == FailPoint::AfterHints {
            // hints.json is written, the meeting is not published.
            assert!(fx.lib.list_meeting_dirs().unwrap().is_empty());
            let q = Queue::open(fx.lib.root()).unwrap();
            let job = &q.list(MANIFEST_KIND).unwrap()[0];
            assert!(q.job_dir(&job.id).join(HINTS_JSON).is_file());
        } else {
            assert_eq!(queue_jobs(&fx.lib), 2);
        }
        let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
        assert!(s.failed.is_empty(), "{point:?}: {:?}", s.failed);
        if point != FailPoint::AfterHints {
            assert_eq!(s.reused, 2, "{point:?}");
        }
        assert_eq!(counts(&fx.lib), expected, "{point:?}");
        assert_eq!(queue_jobs(&fx.lib), 0);
    }
}

#[test]
fn done_meeting_with_a_confirmation_passes_phase_3_unchanged_after_merge_is_set() {
    let mut fx = Fixture::new();
    let file = fx.wav(
        "call.wav",
        &[(Some(1), 4.0), (Some(2), 4.0), (Some(3), 4.0)],
    );
    let imported = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
    let zed = Person::new(fx.lib.new_person_id(), "Zed Example");
    fx.lib
        .write(|tx| {
            tx.save_person(&zed)?;
            tx.update_meeting(&imported.meeting_id, |m| {
                let s = m.speaker_mut(sid(1)).unwrap();
                s.assignment = Assignment::Confirmed;
                s.person_id = Some(zed.id.clone());
                Ok(())
            })
            .map(|_| ())
        })
        .unwrap();
    let before = fx.lib.load_meeting(&imported.meeting_id).unwrap();
    // A merge threshold that would fold every cluster of an analyzed meeting.
    calibrated(&mut fx, Some(-1.0));
    let man = manifest(
        Vec::new(),
        vec![entry(
            0,
            &file,
            3,
            vec![
                hint(Some("Ana Example"), false, &[(4.0, 8.0)]),
                hint(Some("Bea Example"), false, &[(8.0, 12.0)]),
            ],
        )],
    );
    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert_eq!(s.existing, 1);
    assert_eq!(s.merged_speakers, 0);
    let after = fx.lib.load_meeting(&imported.meeting_id).unwrap();
    let ids = |m: &Meeting| m.speakers.iter().map(|s| s.id).collect::<Vec<_>>();
    assert_eq!(ids(&after), ids(&before));
    let s1 = after.speaker(sid(1)).unwrap();
    assert_eq!(
        (s1.assignment, s1.person_id.as_ref()),
        (Assignment::Confirmed, Some(&zed.id))
    );
    // Manifest metadata merged into the file-derived meeting.
    assert_eq!(after.source_id.as_deref(), Some("other-tool-0"));
    assert_eq!(after.title.as_deref(), Some("Sync 0"));
    assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
}

#[test]
fn speaker_confirmed_between_phases_2_and_3_keeps_assignment_and_id() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, Some(-1.0));
    let file = fx.wav("w.wav", &[(Some(1), 4.0), (Some(2), 4.0), (Some(3), 4.0)]);
    let man = manifest(
        Vec::new(),
        vec![entry(
            0,
            &file,
            1,
            vec![
                hint(Some("Ana Example"), false, &[(0.0, 4.0)]),
                hint(Some("Bea Example"), false, &[(4.0, 8.0)]),
            ],
        )],
    );
    FAIL_AT.set(Some(FailPoint::AfterPhase2));
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
    let m = meeting_by_source(&fx.lib, "other-tool-0");
    assert_eq!(m.processing.status, ProcessingStatus::Analyzed);
    let zed = Person::new(fx.lib.new_person_id(), "Zed Example");
    fx.lib
        .write(|tx| {
            tx.save_person(&zed)?;
            tx.update_meeting(&m.id, |m| {
                let s = m.speaker_mut(sid(3)).unwrap();
                s.assignment = Assignment::Confirmed;
                s.person_id = Some(zed.id.clone());
                Ok(())
            })
            .map(|_| ())
        })
        .unwrap();
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    let after = fx.lib.load_meeting(&m.id).unwrap();
    assert_eq!(after.processing.status, ProcessingStatus::Done);
    let s3 = after.speaker(sid(3)).unwrap();
    assert_eq!(
        broscribe_core::ops::effective_speaker(&after, sid(3)),
        sid(3),
        "the confirmed speaker is never grouped under another"
    );
    assert_eq!(
        (s3.assignment, s3.person_id.as_ref()),
        (Assignment::Confirmed, Some(&zed.id))
    );
}

#[test]
fn forgetting_a_person_scrubs_an_interrupted_batch() {
    let fx = Fixture::new();
    let man = two_meetings(&fx);
    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
    let ana = fx.lib.find_person_by_name("Ana Example").unwrap().unwrap();
    let q = Queue::open(fx.lib.root()).unwrap();
    let mentions = || {
        q.list(MANIFEST_KIND)
            .unwrap()
            .iter()
            .filter_map(|j| HintsFile::read(&q.job_dir(&j.id).join(HINTS_JSON)).ok())
            .map(|h| {
                h.people
                    .iter()
                    .filter(|p| p.person_key == "ana example")
                    .count()
                    + h.speakers
                        .iter()
                        .filter(|p| p.person_key == "ana example")
                        .count()
            })
            .sum::<usize>()
    };
    assert!(mentions() > 0);
    ops::with_index(&fx.lib, |tx, index| {
        ops::forget_person(tx, index, ana.id.as_str())
    })
    .unwrap();
    assert_eq!(mentions(), 0);
    // The resumed batch does not give Ana her voice back (`people[]` may
    // seed her name again, without voiceprints).
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    if let Some(p) = fx.lib.find_person_by_name("Ana Example").unwrap() {
        assert!(
            fx.lib
                .load_voiceprints(&p.id)
                .unwrap()
                .embeddings
                .is_empty()
        );
    }
}

#[test]
fn self_only_hints_report_insufficient_evidence() {
    let fx = Fixture::new();
    let file = fx.wav("solo.wav", &[(Some(0), 4.0), (Some(3), 4.0)]);
    let man = manifest(
        Vec::new(),
        vec![entry(0, &file, 1, vec![hint(None, true, &[(0.0, 4.0)])])],
    );
    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    let me = &s.people[0];
    assert!(me.is_self && !me.enrolled);
    assert_eq!(me.reason, AgreementReason::InsufficientEvidence);
    assert!(s.notes[0].contains("--enroll-me"), "{:?}", s.notes);
}

#[test]
fn move_handling_is_unsupported() {
    let fx = Fixture::new();
    let man = two_meetings(&fx);
    let err = import_manifest(
        &fx.env(),
        &mut models(),
        &man,
        &BatchOptions {
            handling: Some(Handling::Move),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "unsupported");
}

/// Hint prints as (person name, recording, start ms, end ms, id).
fn hint_prints(lib: &Library) -> BTreeSet<(String, Option<String>, i64, i64, String)> {
    let mut out = BTreeSet::new();
    for p in lib.list_people().unwrap() {
        for e in lib.load_voiceprints(&p.id).unwrap().embeddings {
            if e.source.kind != VoiceprintKind::Hint {
                continue;
            }
            // Keyed by the recording whenever the audio is known.
            let m = lib
                .load_meeting(e.source.meeting_id.as_ref().unwrap())
                .unwrap();
            let sha = m.recording.as_ref().map(|r| r.sha256.clone());
            assert_eq!(e.source.sha256, sha);
            assert_eq!(
                e.id,
                schema::voiceprint_id(
                    &p.id,
                    &e.model,
                    VoiceprintOrigin::of_meeting(&m),
                    e.source.start,
                    e.source.end,
                    e.source.kind,
                )
            );
            out.insert((
                p.name.clone(),
                e.source.sha256.clone(),
                (e.source.start * 1000.0).round() as i64,
                (e.source.end * 1000.0).round() as i64,
                e.id,
            ));
        }
    }
    out
}

#[test]
fn embedding_only_pass_gives_the_phase1_voiceprint_ids() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let man = two_meetings(&fx);
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    let first = hint_prints(&fx.lib);
    assert!(!first.is_empty());

    // The jobs are gone: the second run embeds the hints of the existing
    // meetings again and must find the same IDs.
    let again = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert_eq!(again.existing, 2);
    assert_eq!(again.voiceprints_added, 0);
    assert_eq!(hint_prints(&fx.lib), first);

    // A meeting imported on its own first, then hinted by a batch: the
    // embedding-only pass keys the same hint ranges by the same recording as
    // the batch analysis did (its stored segments may give fewer clean pieces).
    let mut other = Fixture::new();
    calibrated(&mut other, None);
    let man2 = two_meetings(&other);
    for m in &man2.meetings {
        import_file(
            &other.env(),
            &mut models(),
            &m.audio_path,
            &ImportOptions::default(),
        )
        .unwrap();
    }
    let s = import_manifest(&other.env(), &mut models(), &man2, &BatchOptions::default()).unwrap();
    assert_eq!(s.existing, 2);
    let strip = |set: BTreeSet<(String, Option<String>, i64, i64, String)>| {
        set.into_iter()
            .map(|(n, sha, a, b, _)| (n, sha, a, b))
            .collect::<BTreeSet<_>>()
    };
    let embedded_only = strip(hint_prints(&other.lib));
    assert!(!embedded_only.is_empty());
    assert!(embedded_only.is_subset(&strip(first)), "{embedded_only:?}");
}

#[test]
fn setup_identity_names_me_and_matches_the_user() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    fx.config.setup.name = Some("Bruno Sample".into());
    fx.config.setup.emails = vec!["bruno@example.com".into()];
    let mut man = two_meetings(&fx);
    man.people.push(ManifestPerson {
        name: Some("Bruno Sample".into()),
        email: Some("Bruno@Example.com".into()),
    });
    let later = &mut man.meetings[1];
    // The user's hint carries their e-mail instead of `is_self`.
    later.hints[0] = SpeakerHint {
        name: None,
        email: Some("BRUNO@example.com".into()),
        is_self: false,
        ranges: vec![(0.0, 4.0)],
    };
    later.participants.push(ManifestParticipant {
        name: Some("bruno sample".into()),
        email: None,
        is_self: false,
    });
    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    // Only Ana is seeded; the user is not a second, non-self person.
    assert_eq!(s.people_seeded, 1);
    let people = fx.lib.list_people().unwrap();
    assert_eq!(people.len(), 3, "{people:?}");
    let me = fx.lib.self_person().unwrap().unwrap();
    assert_eq!(me.name, "Bruno Sample");
    assert_eq!(me.emails, ["BRUNO@example.com"]);
    assert!(s.people.iter().any(|p| p.is_self && p.enrolled));
    let m = meeting_by_source(&fx.lib, "other-tool-1");
    assert!(m.participants.iter().all(|p| p.is_self));
}

#[test]
fn new_me_falls_back_to_the_hint_name_then_me() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let mut man = two_meetings(&fx);
    man.meetings[1].hints[0].name = Some("Bru".into());
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert_eq!(fx.lib.self_person().unwrap().unwrap().name, "Bru");
}

fn batch_files(lib: &Library) -> usize {
    std::fs::read_dir(Queue::open(lib.root()).unwrap().dir())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(BATCH_PREFIX)
        })
        .count()
}

#[test]
fn interrupted_single_import_is_finished_on_rerun() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let file = fx.wav("one.wav", &[(Some(1), 4.0), (Some(2), 4.0)]);
    FAIL_AT.set(Some(FailPoint::ImportAfterSave));
    let err = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap_err();
    assert!(is_failpoint(&err), "{err}");
    let dirs = fx.lib.list_meeting_dirs().unwrap();
    assert_eq!(dirs.len(), 1);
    let saved = fx.lib.load_meeting_at(&dirs[0]).unwrap();
    assert_eq!(saved.processing.status, ProcessingStatus::Analyzed);

    let again = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
    assert!(!again.already_exists);
    assert_eq!(again.meeting_id, saved.id);
    let done = fx.lib.load_meeting(&saved.id).unwrap();
    assert_eq!(done.processing.status, ProcessingStatus::Done);
    assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
    let q = Queue::open(fx.lib.root()).unwrap();
    assert!(q.list(IMPORT_KIND).unwrap().is_empty());

    let third = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
    assert!(third.already_exists);
    assert_eq!(fx.lib.load_meeting(&saved.id).unwrap(), done);
}

#[test]
fn batches_only_enroll_resolve_and_clean_their_own_jobs() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let mut a = two_meetings(&fx);
    a.id = "batch-a".into();
    // Only batch A hints Caio (voice 2 of the earlier meeting).
    a.meetings[0].hints = vec![hint(Some("Caio Example"), false, &[(4.0, 8.0)])];
    // Batch B: a meeting of its own plus A's later meeting (a shared job).
    let own = fx.wav("own.wav", &[(Some(1), 4.0), (Some(3), 4.0)]);
    let mut b = manifest(
        Vec::new(),
        vec![
            entry(
                5,
                &own,
                3,
                vec![hint(Some("Ana Example"), false, &[(0.0, 4.0)])],
            ),
            a.meetings[1].clone(),
        ],
    );
    b.id = "batch-b".into();

    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &a, &BatchOptions::default()).unwrap_err();
    assert_eq!(queue_jobs(&fx.lib), 2);

    let s = import_manifest(&fx.env(), &mut models(), &b, &BatchOptions::default()).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!((s.analyzed, s.reused), (1, 1));
    assert_eq!(s.resolved, 2);
    // B never enrolled from A's hints nor resolved A's own meeting.
    assert!(
        fx.lib
            .find_person_by_name("Caio Example")
            .unwrap()
            .is_none()
    );
    let early = meeting_by_source(&fx.lib, "other-tool-0");
    assert_eq!(early.processing.status, ProcessingStatus::Analyzed);
    let later = meeting_by_source(&fx.lib, "other-tool-1");
    assert_eq!(later.processing.status, ProcessingStatus::Done);
    // B's own job is gone; the shared one stays for A, with its hints.
    assert_eq!(queue_jobs(&fx.lib), 2);
    let q = Queue::open(fx.lib.root()).unwrap();
    assert!(
        q.list(MANIFEST_KIND)
            .unwrap()
            .iter()
            .all(|j| q.job_dir(&j.id).join(HINTS_JSON).is_file())
    );
    assert_eq!(batch_files(&fx.lib), 1);

    // A, run again, finishes with its own hints and cleans up everything.
    let s = import_manifest(&fx.env(), &mut models(), &a, &BatchOptions::default()).unwrap();
    assert_eq!(s.reused, 2);
    assert_eq!(s.resolved, 2);
    assert!(
        fx.lib
            .find_person_by_name("Caio Example")
            .unwrap()
            .is_some()
    );
    let early = meeting_by_source(&fx.lib, "other-tool-0");
    assert_eq!(early.processing.status, ProcessingStatus::Done);
    assert_eq!(queue_jobs(&fx.lib), 0);
    assert_eq!(batch_files(&fx.lib), 0);
}

#[test]
fn enrollment_waits_while_another_process_analyzes_an_entry() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let man = two_meetings(&fx);
    // Another live process holds the later meeting's job.
    let q = Queue::open(fx.lib.root()).unwrap();
    let job = fx
        .lib
        .write(|tx| q.enqueue(tx, MANIFEST_KIND, "other-tool-1", serde_json::json!({})))
        .unwrap();
    let other = broscribe_platform::fakes::FakeProcessProbe::new(4242);
    fx.probe.set_alive(4242, true);
    let lease = q.claim(&fx.lib, &job, &other).unwrap().unwrap();

    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert_eq!(s.busy, 1);
    assert!(
        s.notes.iter().any(|n| n.contains("still being analyzed")),
        "{:?}",
        s.notes
    );
    assert!(s.people.is_empty());
    assert_eq!(s.resolved, 0);
    let early = meeting_by_source(&fx.lib, "other-tool-0");
    assert_eq!(early.processing.status, ProcessingStatus::Analyzed);
    assert!(fx.lib.find_person_by_name("Bea Example").unwrap().is_none());

    // Once the other process lets go, running the batch again finishes it.
    lease.release(&fx.lib).unwrap();
    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert_eq!(s.resolved, 2);
    assert_eq!(queue_jobs(&fx.lib), 0);
    assert_eq!(batch_files(&fx.lib), 0);
}

/// Runs `hook` once, at the first embedding: a pause before publication in
/// which another process acts.
struct HookEmbedder {
    inner: Box<dyn SpeakerEmbedder + Send>,
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl SpeakerEmbedder for HookEmbedder {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn embed(&self, samples_16k: &[f32]) -> Result<Option<Vec<f32>>> {
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        self.inner.embed(samples_16k)
    }
}

#[test]
fn people_forgotten_during_analysis_are_not_recreated_from_stale_work() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let later = fx.wav(
        "later.wav",
        &[(Some(0), 4.0), (Some(1), 4.0), (Some(2), 4.0)],
    );
    let man = manifest(
        vec![ManifestPerson {
            name: Some("Ana Example".into()),
            email: None,
        }],
        vec![entry(
            0,
            &later,
            2,
            vec![
                hint(None, true, &[(0.0, 4.0)]),
                hint(Some("Ana Example"), false, &[(4.0, 8.0)]),
                hint(Some("Bea Example"), false, &[(8.0, 12.0)]),
            ],
        )],
    );
    let mut m = models();
    let other = Library::open(fx.lib.root()).unwrap();
    m.embedder = Box::new(HookEmbedder {
        inner: m.embedder,
        hook: Mutex::new(Some(Box::new(move || {
            ops::with_index(&other, |tx, index| {
                ops::forget_person(tx, index, "Ana Example")
            })
            .unwrap();
        }))),
    });
    let s = import_manifest(&fx.env(), &mut m, &man, &BatchOptions::default()).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.forgotten, 1);
    assert!(
        s.notes.iter().any(|n| n.contains("forgotten")),
        "{:?}",
        s.notes
    );
    // The stale analysis did not bring Ana back, nor give anyone her voice.
    assert!(fx.lib.find_person_by_name("Ana Example").unwrap().is_none());
    assert_eq!(fx.lib.list_people().unwrap().len(), 2);
    assert!(s.people.iter().all(|p| p.name != "Ana Example"));

    // A new run after the forget uses her hints again (the embedding-only
    // pass of this short meeting gives too few pieces to enroll anyone but
    // the user; what matters is that her hints are not dropped).
    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert_eq!(s.forgotten, 0);
    let ana = fx.lib.find_person_by_name("Ana Example").unwrap().unwrap();
    assert!(
        s.people
            .iter()
            .any(|p| p.person_id == ana.id && p.samples > 0),
        "{:?}",
        s.people
    );
}

/// A single-file entry whose file changes after the batch hashed it (here
/// during its analysis) is never published with audio other than the hash
/// it records: the entry fails and the original stays as it is. A new run
/// imports the file as it is now, its library copy holding those bytes.
#[test]
fn a_file_changed_after_hashing_is_never_published_as_its_copy() {
    let fx = Fixture::new();
    let path = fx.wav("changing.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
    let other = fx.wav("other.wav", &[(Some(2), 4.0), (Some(1), 4.0)]);
    let man = manifest(Vec::new(), vec![entry(0, &path, 1, Vec::new())]);
    let mut m = models();
    let changed = path.clone();
    m.embedder = Box::new(HookEmbedder {
        inner: m.embedder,
        hook: Mutex::new(Some(Box::new(move || {
            std::fs::copy(&other, &changed).unwrap();
        }))),
    });
    let s = import_manifest(&fx.env(), &mut m, &man, &BatchOptions::default()).unwrap();
    assert_eq!(s.failed.len(), 1, "{:?}", s.failed);
    assert_eq!(s.failed[0].source_id.as_deref(), Some("other-tool-0"));
    assert!(fx.lib.list_meeting_dirs().unwrap().is_empty());
    let current = std::fs::read(&path).unwrap();
    assert_eq!(current, std::fs::read(fx.input.join("other.wav")).unwrap());

    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    let meeting = meeting_by_source(&fx.lib, "other-tool-0");
    let recording = meeting.recording.unwrap();
    let dir = fx.lib.meeting_dir(&meeting.id).unwrap();
    assert_eq!(std::fs::read(dir.join(&recording.path)).unwrap(), current);
    assert_eq!(recording.sha256, file_sha256(&path).unwrap().0);
}

/// Files left in the queue's job folders that look like staged audio.
fn staged_audio(lib: &Library) -> Vec<std::path::PathBuf> {
    let queue = Queue::open(lib.root()).unwrap();
    let Ok(jobs) = std::fs::read_dir(queue.dir()) else {
        return Vec::new();
    };
    jobs.flatten()
        .filter_map(|job| std::fs::read_dir(job.path()).ok())
        .flat_map(|files| files.flatten().map(|f| f.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("audio."))
        })
        .collect()
}

/// A multi-source entry whose source changes after the batch hashed it
/// (here during its analysis, after decoding) publishes nothing, copied or
/// referenced: no meeting, no staged container left, every source as it
/// is now. A new run imports what the sources hold now, recording their
/// current fingerprint (and, referenced, the per-source hashes of that
/// same pass; owned, the hash of the stored container).
#[test]
fn changed_sources_are_never_published_under_their_old_fingerprint() {
    use broscribe_platform::encoder::{FakeEncodedDecoder, FakeEncoder};
    for handling in [Handling::Copy, Handling::Reference] {
        let mut fx = Fixture::new();
        fx.chain.0.push(Box::new(FakeEncodedDecoder));
        let options = BatchOptions {
            handling: Some(handling),
            encoder: std::sync::Arc::new(FakeEncoder::default()),
        };
        let e = sources_entry(&fx, None);
        let first = std::fs::read(&e.sources[0].path).unwrap();
        let other = fx.wav("replacement.wav", &[(Some(3), 4.0)]);
        let replacement = std::fs::read(&other).unwrap();
        let changed = e.sources[1].path.clone();
        let mut m = models();
        m.embedder = Box::new(HookEmbedder {
            inner: m.embedder,
            hook: Mutex::new(Some(Box::new(move || {
                std::fs::copy(&other, &changed).unwrap();
            }))),
        });
        let man = manifest(Vec::new(), vec![e.clone()]);
        let s = import_manifest(&fx.env(), &mut m, &man, &options).unwrap();
        assert_eq!(s.failed.len(), 1, "{handling:?}: {:?}", s.failed);
        assert!(
            fx.lib.list_meeting_dirs().unwrap().is_empty(),
            "{handling:?}"
        );
        assert!(staged_audio(&fx.lib).is_empty(), "{handling:?}");
        assert_eq!(std::fs::read(&e.sources[0].path).unwrap(), first);
        assert_eq!(std::fs::read(&e.sources[1].path).unwrap(), replacement);

        let s = import_manifest(&fx.env(), &mut models(), &man, &options).unwrap();
        assert!(s.failed.is_empty(), "{handling:?}: {:?}", s.failed);
        let dir = fx.lib.list_meeting_dirs().unwrap().remove(0);
        let rec = fx.lib.load_meeting_at(&dir).unwrap().recording.unwrap();
        let current = entry_hash(&e).unwrap();
        assert_eq!(rec.source_sha256.as_deref(), Some(current.sha256.as_str()));
        if handling == Handling::Reference {
            let recorded: Vec<_> = rec.sources.iter().map(|s| s.sha256.clone()).collect();
            let files: Vec<_> = e
                .sources
                .iter()
                .map(|s| file_sha256(&s.path).unwrap().0)
                .collect();
            assert_eq!(recorded, files);
        } else {
            assert_eq!(rec.sha256, file_sha256(&dir.join(&rec.path)).unwrap().0);
        }
    }
}

/// Names a manifest would give new people that no person may carry: a
/// bidirectional override, a control character, one character too many.
fn invalid_names() -> [String; 3] {
    [
        "Ana \u{202E}Example".to_owned(),
        "Ana\u{7}Example".to_owned(),
        "x".repeat(broscribe_core::names::NAME_MAX_CHARS + 1),
    ]
}

/// CO15: a `people[]` name no person may carry refuses the whole manifest,
/// parsed or given directly, before anything is written.
#[test]
fn an_invalid_people_name_refuses_the_whole_manifest() {
    let fx = Fixture::new();
    let audio = fx.wav("good.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
    for bad in invalid_names() {
        let text = serde_json::json!({
            "format": crate::manifest::FORMAT,
            "version": 2,
            "people": [{"name": "Bea Example"}, {"name": bad}],
            "meetings": [{"audio_path": audio, "source_id": "good"}],
        })
        .to_string();
        let err = crate::manifest::parse(&text).unwrap_err();
        assert_eq!(err.code(), "invalid_manifest", "{err}");

        let man = manifest(
            vec![
                ManifestPerson {
                    name: Some("Bea Example".into()),
                    email: None,
                },
                ManifestPerson {
                    name: Some(bad.clone()),
                    email: None,
                },
            ],
            vec![entry(0, &audio, 1, Vec::new())],
        );
        let err =
            import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
        assert_eq!(err.code(), "invalid_manifest", "{err}");
        assert!(fx.lib.list_people().unwrap().is_empty());
        assert!(fx.lib.list_meeting_dirs().unwrap().is_empty());
        assert_eq!(queue_jobs(&fx.lib), 0);
        assert_eq!(batch_files(&fx.lib), 0);
    }
}

/// CO15: a speaker hint or source `person_hint` that would name a new
/// person in a way no person may carry fails its own meeting only, parsed
/// or given directly. The other meetings import, and an e-mail-only
/// `people[]` entry is still named by its e-mail.
#[test]
fn an_invalid_hint_name_fails_only_its_meeting() {
    let fx = Fixture::new();
    let bad_audio = fx.wav("bad.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
    let good_audio = fx.wav("good.wav", &[(Some(2), 4.0), (Some(1), 4.0)]);
    for bad in invalid_names() {
        let text = serde_json::json!({
            "format": crate::manifest::FORMAT,
            "version": 2,
            "meetings": [
                {"audio_path": bad_audio, "speaker_hints": [{"name": bad, "ranges": [[0, 4]]}]},
                {"sources": [{"path": bad_audio, "role": "participant", "offset_s": 0.0,
                              "person_hint": {"name": bad}}]},
                {"audio_path": good_audio,
                 "speaker_hints": [{"name": "Ana Example", "ranges": [[0, 4]]}]},
            ],
        })
        .to_string();
        let m = crate::manifest::parse(&text).unwrap();
        let issues: Vec<_> = m.issues.iter().map(|i| (i.meeting, i.code)).collect();
        assert_eq!(
            issues,
            [(Some(0), "invalid_manifest"), (Some(1), "invalid_manifest")]
        );
        let kept: Vec<_> = m.meetings.iter().map(|e| e.index).collect();
        assert_eq!(kept, [2]);
    }

    let bad = invalid_names()[0].clone();
    let man = manifest(
        vec![ManifestPerson {
            name: None,
            email: Some("cara@example.com".into()),
        }],
        vec![
            entry(
                0,
                &bad_audio,
                1,
                vec![hint(Some(bad.as_str()), false, &[(0.0, 4.0)])],
            ),
            entry(
                1,
                &good_audio,
                2,
                vec![hint(Some("Ana Example"), false, &[(0.0, 4.0)])],
            ),
        ],
    );
    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    let failed: Vec<_> = s.failed.iter().map(|f| (f.index, f.code)).collect();
    assert_eq!(failed, [(0, "invalid_manifest")]);
    assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
    assert_eq!(
        meeting_by_source(&fx.lib, "other-tool-1").processing.status,
        ProcessingStatus::Done
    );
    let names: Vec<String> = fx
        .lib
        .list_people()
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert!(names.iter().any(|n| n == "cara@example.com"), "{names:?}");
    assert!(!names.contains(&bad), "{names:?}");
    assert!(
        names
            .iter()
            .all(|n| broscribe_core::names::validate_name(n).is_ok_and(|v| v == n)),
        "{names:?}"
    );
}

#[test]
fn source_id_and_recording_must_agree_with_the_library() {
    let fx = Fixture::new();
    let a = fx.wav("a.wav", &[(Some(1), 4.0), (Some(2), 4.0)]);
    let b = fx.wav("b.wav", &[(Some(2), 4.0), (Some(1), 4.0)]);
    let s = import_manifest(
        &fx.env(),
        &mut models(),
        &manifest(Vec::new(), vec![entry(0, &a, 1, Vec::new())]),
        &BatchOptions::default(),
    )
    .unwrap();
    assert_eq!(s.analyzed, 1);

    // The same source_id with other audio, and the same audio under another
    // source_id: both conflicts, nothing imported.
    let mut other = manifest(
        Vec::new(),
        vec![entry(0, &b, 1, Vec::new()), entry(7, &a, 1, Vec::new())],
    );
    other.id = "other".into();
    let s = import_manifest(&fx.env(), &mut models(), &other, &BatchOptions::default()).unwrap();
    assert_eq!(s.failed.len(), 2, "{:?}", s.failed);
    assert!(s.failed.iter().all(|f| f.code == "already_exists"));
    assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
    assert_eq!(
        meeting_by_source(&fx.lib, "other-tool-0")
            .recording
            .unwrap()
            .original_path
            .unwrap(),
        a
    );
    assert_eq!(queue_jobs(&fx.lib), 0);
}

#[test]
fn self_hint_promotes_a_matching_seeded_person_without_setup() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let mut man = two_meetings(&fx);
    man.people.push(ManifestPerson {
        name: Some("Bruno Sample".into()),
        email: None,
    });
    man.meetings[1].hints[0].name = Some("Bruno Sample".into());
    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert_eq!(s.people_seeded, 2);
    // Only Bea is new; the seeded Bruno became Me.
    assert_eq!(s.people_created, 1);
    let me = fx.lib.self_person().unwrap().unwrap();
    assert_eq!(me.name, "Bruno Sample");
    assert_eq!(fx.lib.list_people().unwrap().len(), 3);
    assert!(s.people.iter().any(|p| p.is_self && p.person_id == me.id));
}

#[test]
fn resume_never_enrolls_a_batch_paused_between_entries() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let mut man = two_meetings(&fx);
    man.meetings[0].hints = vec![hint(Some("Ana Example"), false, &[(0.0, 4.0)])];
    FAIL_AT.set(Some(FailPoint::AfterEntry));
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
    let q = Queue::open(fx.lib.root()).unwrap();
    let batch = load_batch(&q, &man.id).unwrap().unwrap();
    assert!(batch.sealed);
    assert_eq!(batch.jobs.len(), 2);
    assert_eq!(batch.entries.len(), 2);
    assert!(!batch.phase1_complete);
    let resumed = resume_batches(&fx.env(), &models()).unwrap();
    assert_eq!(resumed.resolved, 0);
    assert!(resumed.people.is_empty());
    assert_eq!(batch_files(&fx.lib), 1);
    // Legacy/partially written membership is never resumable either, even
    // if a phase-complete flag is present.
    fx.lib
        .write(|_| {
            let mut unsealed = batch.clone();
            unsealed.sealed = false;
            unsealed.phase1_complete = true;
            fsutil::write_json_atomic(&batch_path(&q, &man.id), &unsealed)
        })
        .unwrap();
    let resumed = resume_batches(&fx.env(), &models()).unwrap();
    assert_eq!(resumed.resolved, 0);
    assert!(resumed.people.is_empty());
    let finished =
        import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    let ana = finished
        .people
        .iter()
        .find(|p| p.name == "Ana Example")
        .unwrap();
    assert_eq!(ana.meetings, 2);
    assert!(ana.enrolled);
    assert_eq!(finished.resolved, 2);
    assert_eq!(batch_files(&fx.lib), 0);
}

#[test]
fn forget_between_entries_fences_the_whole_interrupted_batch() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let mut man = two_meetings(&fx);
    man.meetings[0].hints = vec![hint(Some("Ana Example"), false, &[(0.0, 4.0)])];
    FAIL_AT.set(Some(FailPoint::AfterEntry));
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
    let q = Queue::open(fx.lib.root()).unwrap();
    let started_at = load_batch(&q, &man.id).unwrap().unwrap().created_at;
    ops::with_index(&fx.lib, |tx, index| {
        ops::forget_person(tx, index, "Ana Example")
    })
    .unwrap();
    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
    assert_eq!(
        load_batch(&q, &man.id).unwrap().unwrap().created_at,
        started_at
    );
    for job in q.list(MANIFEST_KIND).unwrap() {
        let hints = HintsFile::read(&q.job_dir(&job.id).join(HINTS_JSON)).unwrap();
        assert_eq!(hints.analysis_started_at, Some(started_at));
        assert!(
            hints
                .people
                .iter()
                .all(|p| p.name.as_deref() != Some("Ana Example"))
        );
    }
    let s = resume_batches(&fx.env(), &models()).unwrap();
    assert_eq!(s.resolved, 2);
    assert!(fx.lib.find_person_by_name("Ana Example").unwrap().is_none());
}

#[test]
fn import_repairs_interruption_before_indexing_and_legacy_done_candidates() {
    for legacy_done in [false, true] {
        let mut fx = Fixture::new();
        calibrated(&mut fx, None);
        let file = fx.wav("candidate.wav", &[(Some(1), 4.0), (Some(2), 4.0)]);
        FAIL_AT.set(Some(FailPoint::FinishBeforeIndex));
        import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap_err();
        let saved = fx
            .lib
            .load_meeting_at(&fx.lib.list_meeting_dirs().unwrap()[0])
            .unwrap();
        assert_eq!(saved.processing.status, ProcessingStatus::Analyzed);
        let ana = Person::new(fx.lib.new_person_id(), "Ana Example");
        let mut enrolled = ops::weak_sample(&saved, sid(1), &ana.id).unwrap().unwrap();
        enrolled.strength = Strength::Confirmed;
        enrolled.source.meeting_id = None;
        enrolled.id = "synthetic-enrollment".into();
        fx.lib
            .write(|tx| {
                tx.save_person(&ana)?;
                tx.upsert_voiceprints(&ana.id, vec![enrolled])
            })
            .unwrap();
        if legacy_done {
            // Reproduce the old save-before-index ordering with a pending job.
            fx.lib
                .write(|tx| {
                    tx.update_meeting(&saved.id, |m| {
                        m.processing.status = ProcessingStatus::Done;
                        let s = m.speaker_mut(sid(1)).unwrap();
                        s.assignment = Assignment::Auto;
                        s.person_id = Some(ana.id.clone());
                        Ok(())
                    })
                    .map(|_| ())
                })
                .unwrap();
        }
        let repaired =
            import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
        assert!(!repaired.already_exists);
        assert_eq!(repaired.meeting_id, saved.id);
        assert_eq!(
            fx.lib.load_meeting(&saved.id).unwrap().processing.status,
            ProcessingStatus::Done
        );
        let done = fx.lib.load_meeting(&saved.id).unwrap();
        let weak_id = ops::weak_sample(&done, sid(1), &ana.id)
            .unwrap()
            .unwrap()
            .id;
        assert!(
            fx.lib
                .load_voiceprints(&ana.id)
                .unwrap()
                .embeddings
                .iter()
                .any(|v| v.id == weak_id && v.strength == Strength::Weak)
        );
        let index = Index::open(fx.lib.root()).unwrap();
        let hits = index
            .search_meetings(&broscribe_core::index::SearchQuery::default())
            .unwrap();
        assert!(hits.iter().any(|hit| hit.meeting_id == saved.id));
        assert!(
            import_file(&fx.env(), &mut models(), &file, &ImportOptions::default())
                .unwrap()
                .already_exists
        );
    }
}

#[test]
fn changed_entry_regenerates_checkpoint_and_merges_metadata() {
    let fx = Fixture::new();
    let mut man = two_meetings(&fx);
    man.meetings[0].title = None;
    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
    man.id = "changed-entry".into();
    man.meetings[0].title = Some("Revised metadata".into());
    man.meetings[0].hints = vec![hint(Some("New Example"), false, &[(0.0, 4.0)])];
    let result = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert!(result.failed.is_empty());
    assert_eq!(result.existing, 1);
    assert!(result.people.iter().any(|p| p.name == "New Example"));
    let meeting = meeting_by_source(&fx.lib, "other-tool-0");
    assert_eq!(meeting.title.as_deref(), Some("Revised metadata"));
    assert_eq!(meeting.title_source, Some(schema::TitleSource::User));
}

#[test]
fn missing_batch_job_does_not_block_finishing_remaining_meetings() {
    let fx = Fixture::new();
    let man = two_meetings(&fx);
    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
    let queue = Queue::open(fx.lib.root()).unwrap();
    let job = queue.list(MANIFEST_KIND).unwrap().remove(0);
    std::fs::remove_dir_all(queue.job_dir(&job.id)).unwrap();
    let result = resume_batches(&fx.env(), &models()).unwrap();
    assert_eq!(result.resolved, 1);
    assert_eq!(result.busy, 0);
    assert_eq!(batch_files(&fx.lib), 0);
    assert_eq!(queue_jobs(&fx.lib), 0);
}

#[test]
fn partial_cleanup_prunes_deleted_job_ids_from_batch_membership() {
    let fx = Fixture::new();
    let man = two_meetings(&fx);
    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
    let mut meeting = meeting_by_source(&fx.lib, "other-tool-1");
    meeting.speakers[0].centroid.as_mut().unwrap().model = "foreign".into();
    fx.lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
    let result = resume_batches(&fx.env(), &models()).unwrap();
    assert_eq!(result.resolved, 1);
    let queue = Queue::open(fx.lib.root()).unwrap();
    let batch = load_batch(&queue, &man.id).unwrap().unwrap();
    assert_eq!(batch.jobs.len(), 1);
    assert!(batch.jobs.iter().all(|id| queue.get(id).is_ok()));
}

#[test]
fn failed_manifest_analysis_folders_are_removed_on_batch_completion() {
    let fx = Fixture::new();
    let good = fx.wav("good.wav", &[(Some(0), 3.0)]);
    let bad = fx.input.join("bad.wav");
    std::fs::write(&bad, b"not audio").unwrap();
    let man = manifest(
        Vec::new(),
        vec![
            entry(0, &bad, 1, Vec::new()),
            entry(1, &good, 1, Vec::new()),
        ],
    );
    let result = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert_eq!(result.failed.len(), 1);
    assert_eq!(result.resolved, 1);
    assert_eq!(queue_jobs(&fx.lib), 0);
    assert_eq!(batch_files(&fx.lib), 0);
}

#[test]
fn phase_three_failure_is_reported_and_other_meetings_finish() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let man = two_meetings(&fx);
    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
    let mut broken = meeting_by_source(&fx.lib, "other-tool-0");
    broken.speakers[0].centroid.as_mut().unwrap().vector_b64 = "invalid".into();
    fx.lib.write(|tx| tx.save_meeting(&broken)).unwrap();
    let result = resume_batches(&fx.env(), &models()).unwrap();
    assert_eq!(result.failed.len(), 1);
    assert_eq!(result.failed[0].source_id.as_deref(), Some("other-tool-0"));
    assert_eq!(result.resolved, 1);
    assert_eq!(
        meeting_by_source(&fx.lib, "other-tool-1").processing.status,
        ProcessingStatus::Done
    );
    assert_eq!(queue_jobs(&fx.lib), 0);
}

#[test]
fn resuming_batch_never_enrolls_another_manifest_entries_hints() {
    let fx = Fixture::new();
    let first = two_meetings(&fx);
    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &first, &BatchOptions::default()).unwrap_err();
    let mut second = first.clone();
    second.id = "replacement-hints".into();
    second.meetings[0].hints = vec![hint(Some("Other Example"), false, &[(0.0, 4.0)])];
    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &second, &BatchOptions::default()).unwrap_err();
    // Leave the newer batch paused, so resume only considers the older one's
    // phase-complete membership with the mismatched checkpoint.
    let queue = Queue::open(fx.lib.root()).unwrap();
    let mut batch = load_batch(&queue, &second.id).unwrap().unwrap();
    batch.phase1_complete = false;
    fsutil::write_json_atomic(&batch_path(&queue, &second.id), &batch).unwrap();
    let result = resume_batches(&fx.env(), &models()).unwrap();
    assert_eq!(result.resolved, 0);
    assert!(
        fx.lib
            .find_person_by_name("Other Example")
            .unwrap()
            .is_none()
    );
    assert!(
        result
            .notes
            .iter()
            .any(|note| note.contains("hints changed"))
    );
    // A replaced checkpoint is no other process's analysis.
    assert_eq!(result.busy, 0);
    assert!(
        !result
            .notes
            .iter()
            .any(|note| note.contains("still being analyzed"))
    );
}

#[test]
fn reordering_a_manifest_keeps_entry_fingerprints() {
    let fx = Fixture::new();
    let m = two_meetings(&fx);
    let mut moved = m.meetings[0].clone();
    moved.index = 7;
    assert_eq!(entry_fingerprint(&m.meetings[0]), entry_fingerprint(&moved));
    moved.title = Some("Another title".into());
    assert_ne!(entry_fingerprint(&m.meetings[0]), entry_fingerprint(&moved));
}

#[test]
fn hints_replaced_after_the_readiness_check_are_not_enrolled() {
    let fx = Fixture::new();
    let first = two_meetings(&fx);
    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &first, &BatchOptions::default()).unwrap_err();
    let queue = Queue::open(fx.lib.root()).unwrap();
    let jobs = load_batch(&queue, &first.id).unwrap().unwrap().jobs;
    let paths: Vec<_> = jobs
        .iter()
        .map(|id| queue.job_dir(id).join(HINTS_JSON))
        .collect();
    let read_all = || -> Vec<Vec<u8>> { paths.iter().map(|p| std::fs::read(p).unwrap()).collect() };
    let mine = read_all();
    let mut second = first.clone();
    second.id = "replacement-hints".into();
    second.meetings[0].hints = vec![hint(Some("Other Example"), false, &[(0.0, 4.0)])];
    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &second, &BatchOptions::default()).unwrap_err();
    let theirs = read_all();
    assert_ne!(mine, theirs);
    let mut batch = load_batch(&queue, &second.id).unwrap().unwrap();
    batch.phase1_complete = false;
    fsutil::write_json_atomic(&batch_path(&queue, &second.id), &batch).unwrap();
    // The first batch's checkpoints pass its readiness check; the second
    // batch then replaces them before the first one enrolls.
    for (path, bytes) in paths.iter().zip(&mine) {
        std::fs::write(path, bytes).unwrap();
    }
    let replace = paths.clone();
    BEFORE_PHASE2.set(Some(Box::new(move || {
        for (path, bytes) in replace.iter().zip(&theirs) {
            std::fs::write(path, bytes).unwrap();
        }
    })));
    let result = resume_batches(&fx.env(), &models()).unwrap();
    assert!(BEFORE_PHASE2.take().is_none(), "the hook ran");
    assert_eq!(result.resolved, 0);
    assert_eq!(result.voiceprints_added, 0);
    assert!(
        fx.lib
            .find_person_by_name("Other Example")
            .unwrap()
            .is_none()
    );
    assert!(
        result
            .notes
            .iter()
            .any(|note| note.contains("hints changed"))
    );
}

#[test]
fn duplicate_recording_without_source_id_is_reported_per_entry() {
    let fx = Fixture::new();
    let a = fx.wav("a.wav", &[(Some(1), 4.0), (Some(2), 4.0)]);
    let mut first = entry(
        0,
        &a,
        1,
        vec![hint(Some("Ana Example"), false, &[(0.0, 4.0)])],
    );
    let mut second = entry(
        1,
        &a,
        1,
        vec![hint(Some("Bea Example"), false, &[(4.0, 8.0)])],
    );
    first.source_id = None;
    second.source_id = None;
    let s = import_manifest(
        &fx.env(),
        &mut models(),
        &manifest(Vec::new(), vec![first, second]),
        &BatchOptions::default(),
    )
    .unwrap();
    assert_eq!(s.analyzed, 1);
    assert_eq!(s.failed.len(), 1, "{:?}", s.failed);
    assert_eq!(s.failed[0].index, 1);
    assert_eq!(s.failed[0].code, "invalid_manifest");
    assert!(
        s.failed[0].message.contains("entry 0"),
        "{}",
        s.failed[0].message
    );
    assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
}

#[test]
fn a_running_import_owns_its_batch_until_it_ends() {
    use broscribe_platform::fakes::FakeProcessProbe;
    let fx = Fixture::new();
    let m = two_meetings(&fx);
    let queue = Queue::open(fx.lib.root()).unwrap();
    let pid = fx.probe.current;
    let seen = std::rc::Rc::new(std::cell::Cell::new(None));
    let during = seen.clone();
    let (hook_queue, id) = (queue.clone(), m.id.clone());
    BEFORE_PHASE2.set(Some(Box::new(move || {
        let probe = FakeProcessProbe::new(pid);
        during.set(Some(batch_active(&hook_queue, &id, &probe).unwrap()));
    })));
    import_manifest(&fx.env(), &mut models(), &m, &BatchOptions::default()).unwrap();
    assert_eq!(seen.get(), Some(true), "active while the import runs");
    assert!(!batch_active(&queue, &m.id, &fx.probe).unwrap());

    // A simulated crash also hands the batch back in this process; a real
    // crash leaves the dead owner's PID, which counts as interrupted.
    let mut again = m.clone();
    again.id = "crashed-owner".into();
    FAIL_AT.set(Some(FailPoint::AfterEntry));
    import_manifest(&fx.env(), &mut models(), &again, &BatchOptions::default()).unwrap_err();
    assert!(!batch_active(&queue, &again.id, &fx.probe).unwrap());
    let mut batch = load_batch(&queue, &again.id).unwrap().unwrap();
    batch.owner = Some(4242);
    fsutil::write_json_atomic(&batch_path(&queue, &again.id), &batch).unwrap();
    assert!(!batch_active(&queue, &again.id, &fx.probe).unwrap());
    fx.probe.set_alive(4242, true);
    assert!(batch_active(&queue, &again.id, &fx.probe).unwrap());
}

/// A v2 entry of two participant sources, without `source_id` (the audio
/// alone identifies it), whose default track is written as `track`.
fn sources_entry(fx: &Fixture, track: Option<usize>) -> ManifestMeeting {
    let a = fx.wav("track-a.wav", &[(Some(1), 4.0)]);
    let b = fx.wav("track-b.wav", &[(Some(2), 4.0)]);
    let source = |path: &Path, offset_s: f64| crate::manifest::ManifestSource {
        path: path.to_owned(),
        audio_track_index: track,
        role: TrackRole::Participant,
        person_hint: None,
        offset_s,
    };
    let mut e = entry(0, &a, 1, Vec::new());
    e.source_id = None;
    e.sources = vec![source(&a, 0.0), source(&b, 2.0)];
    e
}

#[test]
fn null_and_zero_default_track_are_one_recording_including_legacy_hashes() {
    let fx = Fixture::new();
    let options = BatchOptions {
        handling: Some(Handling::Reference),
        ..Default::default()
    };
    let null = sources_entry(&fx, None);
    let zero = sources_entry(&fx, Some(0));
    assert_eq!(
        entry_hash(&null).unwrap().sha256,
        entry_hash(&zero).unwrap().sha256
    );
    let run = |id: &str, e: &ManifestMeeting| {
        let mut m = manifest(Vec::new(), vec![e.clone()]);
        m.id = id.into();
        let s = import_manifest(&fx.env(), &mut models(), &m, &options).unwrap();
        assert!(s.failed.is_empty(), "{id}: {:?}", s.failed);
        (s.analyzed, s.existing)
    };
    assert_eq!(run("null-track", &null), (1, 0));
    assert_eq!(run("zero-track", &zero), (0, 1));
    assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);

    // A meeting an earlier version imported recorded the mapping exactly as
    // written: `null` for the default track.
    let identities: Vec<_> = null
        .sources
        .iter()
        .map(|s| {
            (
                file_sha256(&s.path).unwrap().0,
                s.audio_track_index,
                s.role,
                s.offset_s,
            )
        })
        .collect();
    let legacy = schema::sha256_hex(&serde_json::to_vec(&identities).unwrap());
    assert_ne!(legacy, entry_hash(&zero).unwrap().sha256);
    let dir = fx.lib.list_meeting_dirs().unwrap().remove(0);
    let mut meeting = fx.lib.load_meeting_at(&dir).unwrap();
    let rec = meeting.recording.as_mut().unwrap();
    rec.sha256 = legacy.clone();
    rec.source_sha256 = Some(legacy);
    fx.lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
    assert_eq!(run("legacy-null-track", &zero), (0, 1));
    assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
}

#[test]
fn interrupted_hinted_entry_fails_with_bounded_attempts_instead_of_looping() {
    use broscribe_core::queue::MAX_ATTEMPTS;
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let man = two_meetings(&fx);
    // Entry 0 wrote its checkpoint, not its meeting; the import died.
    FAIL_AT.set(Some(FailPoint::AfterHints));
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
    let q = Queue::open(fx.lib.root()).unwrap();
    let hinted = q
        .list(MANIFEST_KIND)
        .unwrap()
        .into_iter()
        .find(|j| q.job_dir(&j.id).join(HINTS_JSON).is_file())
        .unwrap();
    // Each resume (one worker run) consumes one attempt and reports it.
    for round in 1..=MAX_ATTEMPTS {
        let s = resume_batches(&fx.env(), &models()).unwrap();
        assert_eq!(s.resolved, 0);
        assert!(
            s.failed
                .iter()
                .any(|f| f.source_id.as_deref() == Some("other-tool-0")),
            "{round}: {:?}",
            s.failed
        );
        let job = q.get(&hinted.id).unwrap();
        assert_eq!((job.state, job.attempts), (JobState::Failed, round));
    }
    // No attempt left: no worker takes it again, and resume leaves it alone.
    assert!(!q.get(&hinted.id).unwrap().is_claimable());
    let s = resume_batches(&fx.env(), &models()).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(q.get(&hinted.id).unwrap().attempts, MAX_ATTEMPTS);
    // Running the manifest again restarts the entry and finishes the batch.
    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.resolved, 2);
    assert_eq!(queue_jobs(&fx.lib), 0);
    assert_eq!(batch_files(&fx.lib), 0);
}

#[test]
fn a_checkpoint_without_its_meeting_fails_once_and_the_batch_finishes() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let man = two_meetings(&fx);
    FAIL_AT.set(Some(FailPoint::AfterPhase1));
    import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap_err();
    let early = meeting_by_source(&fx.lib, "other-tool-0");
    std::fs::remove_dir_all(fx.lib.meeting_dir(&early.id).unwrap()).unwrap();
    let s = resume_batches(&fx.env(), &models()).unwrap();
    assert_eq!(s.busy, 0);
    assert!(
        !s.notes.iter().any(|n| n.contains("still being analyzed")),
        "{:?}",
        s.notes
    );
    assert_eq!(s.failed.len(), 1, "{:?}", s.failed);
    assert_eq!(s.failed[0].source_id.as_deref(), Some("other-tool-0"));
    assert_eq!(s.resolved, 1);
    assert_eq!(queue_jobs(&fx.lib), 0);
    assert_eq!(batch_files(&fx.lib), 0);
    let again = resume_batches(&fx.env(), &models()).unwrap();
    assert_eq!(again, BatchSummary::default());
}

#[test]
fn evidence_changed_during_enrollment_is_compared_again_before_commit() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    let man = two_meetings(&fx);
    let later = Queue::open(fx.lib.root())
        .unwrap()
        .job_dir(&JobId::for_key(MANIFEST_KIND, "other-tool-1"))
        .join(HINTS_JSON);
    // Between the agreement computation and its commit, another process
    // rewrites the later meeting's checkpoint without Bea's evidence.
    BEFORE_ENROLL_COMMIT.set(Some(Box::new(move || {
        let mut hints = HintsFile::read(&later).unwrap();
        assert!(hints.people.iter().any(|p| p.person_key == "bea example"));
        hints.people.retain(|p| p.person_key != "bea example");
        hints.speakers.retain(|s| s.person_key != "bea example");
        hints.write(&later).unwrap();
    })));
    let s = import_manifest(&fx.env(), &mut models(), &man, &BatchOptions::default()).unwrap();
    assert!(BEFORE_ENROLL_COMMIT.take().is_none(), "the hook ran");
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert!(
        !s.notes.iter().any(|n| n.contains("kept changing")),
        "{:?}",
        s.notes
    );
    // The recomputed enrollment committed, without the withdrawn evidence.
    assert!(!s.people.is_empty());
    let bea = fx.lib.find_person_by_name("Bea Example").unwrap().unwrap();
    assert!(s.people.iter().all(|p| p.person_id != bea.id));
    assert!(
        fx.lib
            .load_voiceprints(&bea.id)
            .unwrap()
            .embeddings
            .iter()
            .all(|e| e.source.kind != VoiceprintKind::Hint)
    );
}

#[test]
fn me_forgotten_during_pair_analysis_is_never_published_from_the_stale_pass() {
    let mut fx = Fixture::new();
    calibrated(&mut fx, None);
    // Me with a confirmed print of the voice talking on the mic.
    let mut me = Person::new(fx.lib.new_person_id(), "Me Example");
    me.is_self = true;
    let voice = models()
        .embedder
        .embed(&crate::testutil::conversation(&[(Some(0), 4.0)]))
        .unwrap()
        .unwrap();
    let print = hint_voiceprint(
        &me.id,
        MODEL,
        &PooledSample {
            vector: voice,
            meeting: fx.lib.new_meeting_id().unwrap(),
            sha: "synthetic-me".into(),
            start: 0.0,
            end: 4.0,
        },
    );
    fx.lib
        .write(|tx| {
            tx.save_person(&me)?;
            tx.upsert_voiceprints(&me.id, vec![print])
        })
        .unwrap();
    let mic = fx.wav("pair-mic.wav", &[(Some(0), 4.0)]);
    let system = fx.wav("pair-system.wav", &[(Some(1), 4.0)]);
    let source = |path: &Path, role: TrackRole| crate::manifest::ManifestSource {
        path: path.to_owned(),
        audio_track_index: None,
        role,
        person_hint: None,
        offset_s: 0.0,
    };
    let mut e = entry(0, &mic, 1, Vec::new());
    e.sources = vec![
        source(&mic, TrackRole::Mic),
        source(&system, TrackRole::System),
    ];
    let mut m = models();
    let other = Library::open(fx.lib.root()).unwrap();
    m.embedder = Box::new(HookEmbedder {
        inner: m.embedder,
        hook: Mutex::new(Some(Box::new(move || {
            ops::with_index(&other, |tx, index| {
                ops::forget_person(tx, index, "Me Example")
            })
            .unwrap();
        }))),
    });
    let s = import_manifest(
        &fx.env(),
        &mut m,
        &manifest(Vec::new(), vec![e]),
        &BatchOptions {
            handling: Some(Handling::Reference),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.analyzed, 1);
    assert!(fx.lib.self_person().unwrap().is_none());
    let meeting = meeting_by_source(&fx.lib, "other-tool-0");
    assert!(
        meeting
            .speakers
            .iter()
            .all(|s| s.person_id.as_ref() != Some(&me.id)),
        "{:?}",
        meeting.speakers
    );
}

#[test]
fn unreadable_meetings_refuse_only_the_entries_they_may_be() {
    let fx = Fixture::new();
    let a = fx.wav("a.wav", &[(Some(1), 4.0), (Some(2), 4.0)]);
    let b = fx.wav("b.wav", &[(Some(2), 4.0), (Some(1), 4.0)]);
    let c = fx.wav("c.wav", &[(Some(1), 3.0), (Some(3), 4.0)]);
    let d = fx.wav("d.wav", &[(Some(3), 5.0)]);
    // References: no audio in the library to compare.
    let reference = BatchOptions {
        handling: Some(Handling::Reference),
        ..BatchOptions::default()
    };
    let s = import_manifest(
        &fx.env(),
        &mut models(),
        &manifest(
            Vec::new(),
            vec![entry(0, &a, 1, Vec::new()), entry(1, &b, 2, Vec::new())],
        ),
        &reference,
    )
    .unwrap();
    assert_eq!(s.analyzed, 2, "{:?}", s.failed);
    let dir_of = |source_id: &str| {
        fx.lib
            .meeting_dir(&meeting_by_source(&fx.lib, source_id).id)
            .unwrap()
    };
    let (a_dir, b_dir) = (dir_of("other-tool-0"), dir_of("other-tool-1"));
    // Only meeting.md still names a's audio; only the damaged meeting.json
    // still names b's source_id.
    let json = broscribe_core::paths::MEETING_JSON;
    std::fs::write(a_dir.join(json), b"{ not json").unwrap();
    std::fs::write(
        b_dir.join(json),
        b"{\n  \"source_id\" : \"other-tool-1\",\n  \"title\": ",
    )
    .unwrap();

    // a again, other audio under b's source_id, and an unrelated entry.
    let mut again = manifest(
        Vec::new(),
        vec![
            entry(0, &a, 1, Vec::new()),
            entry(1, &d, 3, Vec::new()),
            entry(9, &c, 4, Vec::new()),
        ],
    );
    again.id = "again".into();
    let s = import_manifest(&fx.env(), &mut models(), &again, &reference).unwrap();
    assert_eq!(s.analyzed, 1, "{:?}", s.failed);
    let mut failed: Vec<_> = s.failed.iter().collect();
    failed.sort_by_key(|f| f.index);
    assert_eq!(failed.len(), 2, "{:?}", s.failed);
    for (failure, (index, dir)) in failed.into_iter().zip([(0, &a_dir), (1, &b_dir)]) {
        assert_eq!((failure.index, failure.code), (index, "invalid_input"));
        assert!(
            failure.message.contains(&dir.display().to_string()),
            "{}",
            failure.message
        );
    }
    let published: Vec<_> = fx
        .lib
        .list_meeting_dirs()
        .unwrap()
        .iter()
        .filter_map(|dir| fx.lib.load_meeting_at(dir).ok())
        .collect();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].source_id.as_deref(), Some("other-tool-9"));
    assert_eq!(
        published[0]
            .recording
            .as_ref()
            .unwrap()
            .original_path
            .as_ref(),
        Some(&c)
    );
    assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 3);
    assert_eq!(queue_jobs(&fx.lib), 0);
}

/// Replaces the whole `meeting.json` of `dir` with bytes that name nothing.
fn destroy_json(dir: &Path) {
    std::fs::write(
        dir.join(broscribe_core::paths::MEETING_JSON),
        b"{ unreadable",
    )
    .unwrap();
}

fn index_rows(lib: &Library) -> Vec<MeetingId> {
    let mut ids = Index::open_readonly(lib.root())
        .unwrap()
        .meeting_ids()
        .unwrap();
    ids.sort();
    ids
}

/// Each failure of `s`, as `(index, code)`, after checking that its message
/// names `dir`.
fn refusals(s: &BatchSummary, dir: &Path) -> Vec<(usize, &'static str)> {
    let mut out: Vec<_> = s
        .failed
        .iter()
        .map(|f| {
            assert!(
                f.message.contains(&dir.display().to_string()),
                "{}",
                f.message
            );
            (f.index, f.code)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn a_copied_multi_source_meeting_is_refused_after_its_json_is_destroyed() {
    use broscribe_platform::encoder::{FakeEncodedDecoder, FakeEncoder};
    let mut fx = Fixture::new();
    fx.chain.0.push(Box::new(FakeEncodedDecoder));
    let copy = BatchOptions {
        handling: Some(Handling::Copy),
        encoder: std::sync::Arc::new(FakeEncoder::default()),
    };
    // Two sources and their mapping, no source_id: only the original
    // content/mapping fingerprint identifies the meeting.
    let sources = sources_entry(&fx, None);
    let mut first = manifest(Vec::new(), vec![sources.clone()]);
    first.id = "first".into();
    let s = import_manifest(&fx.env(), &mut models(), &first, &copy).unwrap();
    assert_eq!(s.analyzed, 1, "{:?}", s.failed);
    let dir = fx.lib.list_meeting_dirs().unwrap().remove(0);
    let original = fx.lib.load_meeting_at(&dir).unwrap();
    let rec = original.recording.as_ref().unwrap();
    let fingerprint = entry_hash(&sources).unwrap().sha256;
    // The library keeps the encoded copy: its hash is not the fingerprint.
    assert_eq!(rec.source_sha256.as_deref(), Some(fingerprint.as_str()));
    assert_ne!(rec.sha256, fingerprint);
    destroy_json(&dir);
    let md = std::fs::read(dir.join(broscribe_core::paths::MEETING_MD)).unwrap();
    let rows = index_rows(&fx.lib);
    assert_eq!(rows, vec![original.id.clone()]);

    let siblings = [
        fx.wav("sibling-0.wav", &[(Some(1), 3.0), (Some(3), 4.0)]),
        fx.wav("sibling-1.wav", &[(Some(3), 5.0), (Some(2), 3.0)]),
    ];
    for (round, sibling) in siblings.iter().enumerate() {
        if round == 1 {
            // Also once the library lost its own copy of the audio.
            let mut removed = 0;
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("audio."))
                {
                    std::fs::remove_file(path).unwrap();
                    removed += 1;
                }
            }
            assert_eq!(removed, 1);
        }
        // The same sources and mapping again, beside unrelated content
        // with its own source_id.
        let mut again = manifest(
            Vec::new(),
            vec![sources.clone(), entry(9 + round, sibling, 4, Vec::new())],
        );
        again.id = format!("again-{round}");
        let before = fx.lib.list_meeting_dirs().unwrap().len();
        let s = import_manifest(&fx.env(), &mut models(), &again, &copy).unwrap();
        assert_eq!(
            refusals(&s, &dir),
            vec![(0, "invalid_input")],
            "round {round}"
        );
        assert_eq!(s.analyzed, 1, "round {round}: {:?}", s.failed);
        let dirs = fx.lib.list_meeting_dirs().unwrap();
        assert_eq!(dirs.len(), before + 1, "round {round}");
        let published: Vec<_> = dirs
            .iter()
            .filter_map(|d| fx.lib.load_meeting_at(d).ok())
            .collect();
        assert_eq!(published.len(), round + 1, "round {round}");
        assert!(
            published.iter().all(|m| {
                m.recording.as_ref().unwrap().source_sha256.as_deref() != Some(fingerprint.as_str())
            }),
            "round {round}: the sources were imported a second time"
        );
        // The index has a row only for the original and the published
        // meetings, each once: the refused entry added none.
        let mut known: Vec<_> = published.iter().map(|m| m.id.clone()).collect();
        known.push(original.id.clone());
        let rows = index_rows(&fx.lib);
        assert!(
            rows.windows(2).all(|w| w[0] != w[1]) && rows.iter().all(|id| known.contains(id)),
            "round {round}: {rows:?} beside {known:?}"
        );
        assert!(
            published.iter().all(|m| rows.contains(&m.id)),
            "round {round}"
        );
    }
    // The damaged meeting itself is left exactly as it was.
    assert_eq!(
        std::fs::read(dir.join(broscribe_core::paths::MEETING_JSON)).unwrap(),
        b"{ unreadable"
    );
    assert_eq!(
        std::fs::read(dir.join(broscribe_core::paths::MEETING_MD)).unwrap(),
        md
    );
}

#[test]
fn a_source_id_is_refused_for_other_audio_after_its_json_is_destroyed() {
    let fx = Fixture::new();
    let reference = BatchOptions {
        handling: Some(Handling::Reference),
        ..BatchOptions::default()
    };
    let a = fx.wav("a.wav", &[(Some(1), 4.0), (Some(2), 4.0)]);
    let b = fx.wav("b.wav", &[(Some(2), 4.0), (Some(1), 4.0)]);
    let c = fx.wav("c.wav", &[(Some(1), 3.0), (Some(3), 4.0)]);
    // Reference a, source_id X ("other-tool-0").
    let s = import_manifest(
        &fx.env(),
        &mut models(),
        &manifest(Vec::new(), vec![entry(0, &a, 1, Vec::new())]),
        &reference,
    )
    .unwrap();
    assert_eq!(s.analyzed, 1, "{:?}", s.failed);
    let dir = fx.lib.list_meeting_dirs().unwrap().remove(0);
    destroy_json(&dir);
    let md = std::fs::read(dir.join(broscribe_core::paths::MEETING_MD)).unwrap();
    // Other audio b under X, and unrelated c with its own source_id.
    let mut again = manifest(
        Vec::new(),
        vec![entry(0, &b, 2, Vec::new()), entry(9, &c, 4, Vec::new())],
    );
    again.id = "again".into();
    let s = import_manifest(&fx.env(), &mut models(), &again, &reference).unwrap();
    assert_eq!(refusals(&s, &dir), vec![(0, "invalid_input")]);
    assert_eq!(s.analyzed, 1, "{:?}", s.failed);
    let published: Vec<_> = fx
        .lib
        .list_meeting_dirs()
        .unwrap()
        .iter()
        .filter_map(|d| fx.lib.load_meeting_at(d).ok())
        .collect();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].source_id.as_deref(), Some("other-tool-9"));
    assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 2);
    assert_eq!(index_rows(&fx.lib).len(), 2);
    assert_eq!(
        std::fs::read(dir.join(broscribe_core::paths::MEETING_JSON)).unwrap(),
        b"{ unreadable"
    );
    assert_eq!(
        std::fs::read(dir.join(broscribe_core::paths::MEETING_MD)).unwrap(),
        md
    );
    assert_eq!(queue_jobs(&fx.lib), 0);
}

#[test]
fn a_source_id_a_manifest_adopted_is_refused_for_other_audio_after_its_json_is_destroyed() {
    use broscribe_core::render::{MeetingMdIdentity, read_meeting_md_identity};
    let fx = Fixture::new();
    let a = fx.wav("a.wav", &[(Some(1), 4.0), (Some(2), 4.0)]);
    let b = fx.wav("b.wav", &[(Some(2), 4.0), (Some(1), 4.0)]);
    let c = fx.wav("c.wav", &[(Some(1), 3.0), (Some(3), 4.0)]);
    let reference = BatchOptions {
        handling: Some(Handling::Reference),
        ..BatchOptions::default()
    };
    // A file import without source_id, then a manifest names its audio X.
    let out = import_file(
        &fx.env(),
        &mut models(),
        &a,
        &ImportOptions {
            handling: Some(Handling::Reference),
            ..ImportOptions::default()
        },
    )
    .unwrap();
    let s = import_manifest(
        &fx.env(),
        &mut models(),
        &manifest(Vec::new(), vec![entry(0, &a, 1, Vec::new())]),
        &reference,
    )
    .unwrap();
    assert_eq!((s.analyzed, s.existing), (0, 1), "{:?}", s.failed);
    let adopted = fx.lib.load_meeting(&out.meeting_id).unwrap();
    assert_eq!(adopted.source_id.as_deref(), Some("other-tool-0"));
    let dir = fx.lib.meeting_dir(&adopted.id).unwrap();
    // The adoption reached the derived meeting.md, complete.
    assert_eq!(
        read_meeting_md_identity(&dir.join(broscribe_core::paths::MEETING_MD)).unwrap(),
        Some(MeetingMdIdentity::of(&adopted))
    );
    destroy_json(&dir);
    let mut again = manifest(
        Vec::new(),
        vec![entry(0, &b, 2, Vec::new()), entry(9, &c, 4, Vec::new())],
    );
    again.id = "again".into();
    let s = import_manifest(&fx.env(), &mut models(), &again, &reference).unwrap();
    assert_eq!(refusals(&s, &dir), vec![(0, "invalid_input")]);
    assert_eq!(s.analyzed, 1, "{:?}", s.failed);
    assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 2);
}

#[test]
fn an_unreadable_meeting_without_provenance_refuses_entries_until_repaired() {
    let fx = Fixture::new();
    let reference = BatchOptions {
        handling: Some(Handling::Reference),
        ..BatchOptions::default()
    };
    let a = fx.wav("a.wav", &[(Some(1), 4.0), (Some(2), 4.0)]);
    let c = fx.wav("c.wav", &[(Some(1), 3.0), (Some(3), 4.0)]);
    let s = import_manifest(
        &fx.env(),
        &mut models(),
        &manifest(Vec::new(), vec![entry(0, &a, 1, Vec::new())]),
        &reference,
    )
    .unwrap();
    assert_eq!(s.analyzed, 1, "{:?}", s.failed);
    let dir = fx.lib.list_meeting_dirs().unwrap().remove(0);
    // An earlier version wrote this meeting.md: it records the recording's
    // hash but not the source fingerprint or source_id, so even content
    // and a source_id it never had cannot be ruled out.
    destroy_json(&dir);
    crate::finish::unreadable_lookup_tests::strip_provenance(&dir);
    let mut unrelated = manifest(Vec::new(), vec![entry(9, &c, 4, Vec::new())]);
    unrelated.id = "unrelated".into();
    let s = import_manifest(&fx.env(), &mut models(), &unrelated, &reference).unwrap();
    assert_eq!(refusals(&s, &dir), vec![(9, "invalid_input")]);
    assert!(
        s.failed[0].message.contains("rule out"),
        "{}",
        s.failed[0].message
    );
    assert_eq!(s.analyzed, 0);
    assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
    // Removing the damaged meeting.json, as the refusal says, unblocks it.
    std::fs::remove_file(dir.join(broscribe_core::paths::MEETING_JSON)).unwrap();
    unrelated.id = "unrelated-again".into();
    let s = import_manifest(&fx.env(), &mut models(), &unrelated, &reference).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.analyzed, 1);
}
