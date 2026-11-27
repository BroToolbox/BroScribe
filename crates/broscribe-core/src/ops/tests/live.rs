//! Live-preview records under the public identity writes: explicit line
//! edits settle every supporting decision, forgetting regenerates the
//! derived notes, user names are validated and unusable evidence is
//! reported and repaired explicitly.

use super::*;
use crate::config::LiveConfig;
use crate::live_identity::{
    LiveAction, LiveAttribution, LiveOutcome, LiveRange, live_review_items, live_reviews,
    refresh_review_notes,
};
use crate::schema::ReviewSource;

const BIA_NAME: &str = "Bia Example";
const ANA_NAME: &str = "Ana Example";

fn live_range(from_s: f64, to_s: f64) -> LiveRange {
    LiveRange {
        track: "system".into(),
        from_sample: (from_s * 16_000.0) as u64,
        to_sample: (to_s * 16_000.0) as u64,
        from: None,
        to: None,
    }
}

fn record(
    decision: u64,
    person: &str,
    span: (f64, f64),
    outcome: LiveOutcome,
    supported: Vec<[f64; 2]>,
) -> LiveAttribution {
    LiveAttribution {
        turn_id: format!("system-{decision}"),
        decision,
        action: LiveAction::Name,
        person_id: Some(pid(person)),
        range: live_range(span.0, span.1),
        from_s: Some(span.0),
        to_s: Some(span.1),
        supported,
        outcome,
        at: schema::now(),
        user_override: false,
        forget_fence: None,
    }
}

/// One final line (system track, 1–3 s) Ana's two adjacent live turns
/// jointly support (1–2 s and 2–3 s join over it); the first decision
/// created the line. Bia's third turn covers only part of it and asks for
/// review, with its derived note.
fn jointly_supported() -> (TempDir, Library, SegmentFingerprint) {
    let mut m = meeting(MA, 10, Vec::new());
    m.segments = vec![Segment {
        speech_overlap: Vec::new(),
        id: 0,
        track: "system".into(),
        speaker: None,
        start: 1.0,
        end: 3.0,
        text: "synthetic joint line".into(),
        words: Vec::new(),
    }];
    let fp = segment_fingerprint(&m.segments[0]);
    m.utterance_people.push(schema::UtterancePerson {
        track: fp.track.clone(),
        start: fp.start,
        end: fp.end,
        text_sha256: fp.text_sha256.clone(),
        person_id: pid(ANA),
        at: schema::now(),
        live_decision: Some(1),
    });
    m.live_attributions = vec![
        record(1, ANA, (1.0, 2.0), LiveOutcome::Applied, vec![[1.0, 3.0]]),
        record(2, ANA, (2.0, 3.0), LiveOutcome::Applied, vec![[1.0, 3.0]]),
        record(3, BIA, (2.5, 4.0), LiveOutcome::Partial, vec![[2.5, 3.0]]),
    ];
    let names = BTreeMap::from([
        (pid(ANA), ANA_NAME.to_owned()),
        (pid(BIA), BIA_NAME.to_owned()),
    ]);
    refresh_review_notes(&mut m, &names);
    let (tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), ANA_NAME), Vec::new()),
            (Person::new(pid(BIA), BIA_NAME), Vec::new()),
        ],
        vec![m],
    );
    (tmp, lib, fp)
}

fn live_notes(m: &Meeting) -> Vec<&str> {
    m.review_notes
        .iter()
        .filter(|n| n.source == ReviewSource::LivePreview)
        .map(|n| n.text.as_str())
        .collect()
}

fn speaker_lines(people: &[&str]) -> Meeting {
    let mut m = meeting(MA, 10, vec![spk(1, Some(basis(0)))]);
    m.source = Source::Recording;
    m.speakers[0].track = "system".into();
    m.segments.clear();
    for (i, person) in people.iter().enumerate() {
        let a = 1.0 + 3.0 * i as f64;
        let mut segment = seg(i as u32, 1, a, "Synthetic final line");
        segment.track = "system".into();
        let fp = segment_fingerprint(&segment);
        let decision = i as u64 + 1;
        m.live_attributions.push(record(
            decision,
            person,
            (fp.start, fp.end),
            LiveOutcome::Applied,
            vec![[fp.start, fp.end]],
        ));
        m.utterance_people.push(schema::UtterancePerson {
            track: fp.track,
            start: fp.start,
            end: fp.end,
            text_sha256: fp.text_sha256,
            person_id: pid(person),
            at: schema::now(),
            live_decision: Some(decision),
        });
        m.segments.push(segment);
    }
    m
}

fn live_people() -> Vec<(Person, Vec<Vec<f32>>)> {
    vec![
        (Person::new(pid(ANA), ANA_NAME), vec![basis(0)]),
        (Person::new(pid(BIA), BIA_NAME), vec![]),
    ]
}

#[test]
fn profile_merge_preserves_current_live_evidence_and_keeps_stale_evidence_fenced() {
    use crate::live_identity::{
        NEVER_FORGOTTEN, NewDecision, forget_fence, read_sidecar, record_decision,
    };

    let mut keep = Person::new(pid(ANA), ANA_NAME);
    keep.is_self = true;
    let mut source = Person::new(pid(BIA), " ");
    source.aliases = vec!["  ".into(), "B. Example".into()];
    let mut m = speaker_lines(&[BIA, BIA]);
    m.participants.push(Participant {
        person_id: Some(source.id.clone()),
        ..Default::default()
    });
    m.live_attributions
        .push(record(3, BIA, (10.0, 11.0), LiveOutcome::Partial, vec![]));
    let (_tmp, lib) = setup(
        vec![(keep, vec![basis(0)]), (source, vec![basis(0)])],
        vec![m.clone()],
    );
    let decide = |tx: &WriteTx<'_>, turn: &str| {
        record_decision(
            tx,
            "s1",
            NewDecision {
                turn_id: turn.into(),
                epoch: 1,
                turn_revision: 1,
                action: LiveAction::Name,
                person_id: Some(pid(BIA)),
                range: live_range(1.0, 3.0),
            },
        )
    };
    lib.write(|tx| {
        assert_eq!(decide(tx, "stale")?.forget_fence, None);
        write_tombstones(tx.root(), &HintKeys::new(&pid(BIA), None))?;
        let current = decide(tx, "current")?.forget_fence;
        assert!(current.is_some());
        for record in &mut m.live_attributions[..2] {
            record.forget_fence.clone_from(&current);
        }
        tx.save_meeting(&m)
    })
    .unwrap();
    let row = person_duplicates(&lib, MODEL, None).unwrap().remove(0);
    let request = MergePeople {
        keep: pid(ANA),
        source: pid(BIA),
        revision: row.revision,
    };
    with_index(&lib, |tx, index| {
        merge_people(tx, index, &request, MODEL, None)
    })
    .unwrap();
    assert_eq!(
        lib.load_person(&request.keep).unwrap().aliases,
        ["B. Example"]
    );
    let expected_fence = forget_fence(&lib, ANA).unwrap();
    assert_eq!(expected_fence, NEVER_FORGOTTEN);
    let sidecar = read_sidecar(lib.root(), "s1").unwrap().unwrap();
    let current = sidecar.current("current").unwrap();
    assert_eq!(current.person(), Some(&request.keep));
    assert_eq!(current.forget_fence, None);
    let stale = sidecar.current("stale").unwrap();
    assert_eq!(stale.person(), Some(&request.keep));
    assert_ne!(
        stale.forget_fence.as_deref().unwrap_or(NEVER_FORGOTTEN),
        expected_fence
    );
    let mut m = lib.load_meeting(&m.id).unwrap();
    assert_eq!(m.participants[0].person_id.as_ref(), Some(&request.keep));
    assert!(m.participants[0].is_self);
    assert_ne!(
        m.live_attributions[2]
            .forget_fence
            .as_deref()
            .unwrap_or(NEVER_FORGOTTEN),
        expected_fence
    );
    reconcile_live_speakers(&lib, &mut m, &LiveConfig::default()).unwrap();
    assert_eq!(
        m.speakers[0].live_promotion.as_ref().unwrap().person_id,
        request.keep
    );
}

#[test]
fn two_lines_confirm_once_and_one_line_stays_a_suggestion_even_with_a_voice_match() {
    let (_tmp, lib) = setup(live_people(), vec![]);
    let people = load_person_samples(&lib, MODEL).unwrap();
    let mut one = speaker_lines(&[ANA]);
    let mut voice_only = one.clone();
    voice_only.utterance_people.clear();
    resolve_meeting(&mut voice_only, MODEL, &people, Some(&thresholds()), None).unwrap();
    assert_eq!(voice_only.speakers[0].assignment, Assignment::Auto);
    resolve_meeting(&mut one, MODEL, &people, Some(&thresholds()), None).unwrap();
    reconcile_live_speakers(&lib, &mut one, &LiveConfig::default()).unwrap();
    assert_eq!(one.speakers[0].assignment, Assignment::Suggested);
    assert_eq!(one.speakers[0].live_suggestion_lines, Some(1));
    assert_eq!(
        one.speakers[0].suggestion.as_ref().unwrap().person_id,
        pid(ANA)
    );
    let mut one_without_match = speaker_lines(&[BIA]);
    resolve_meeting(
        &mut one_without_match,
        MODEL,
        &people,
        Some(&thresholds()),
        None,
    )
    .unwrap();
    reconcile_live_speakers(&lib, &mut one_without_match, &LiveConfig::default()).unwrap();
    assert_eq!(
        one_without_match.speakers[0].assignment,
        Assignment::Suggested
    );
    assert_eq!(
        one_without_match.speakers[0]
            .suggestion
            .as_ref()
            .unwrap()
            .person_id,
        pid(BIA)
    );

    let mut two = speaker_lines(&[ANA, ANA]);
    let higher = LiveConfig {
        promotion_min_lines: std::num::NonZeroUsize::new(3),
        ..Default::default()
    };
    reconcile_live_speakers(&lib, &mut two, &higher).unwrap();
    assert_eq!(two.speakers[0].assignment, Assignment::Suggested);
    reconcile_live_speakers(&lib, &mut two, &LiveConfig::default()).unwrap();
    assert_eq!(two.speakers[0].assignment, Assignment::Confirmed);
    assert_eq!(
        two.speakers[0]
            .live_promotion
            .as_ref()
            .unwrap()
            .decision_ids,
        [1, 2]
    );
    let first = two.clone();
    reconcile_live_speakers(&lib, &mut two, &LiveConfig::default()).unwrap();
    assert_eq!(two, first);
    lib.write(|tx| {
        sync_live_promotions(tx, None, &two, MODEL)?;
        sync_live_promotions(tx, Some(&first), &two, MODEL)
    })
    .unwrap();
    let queue = crate::queue::Queue::open(lib.root()).unwrap();
    assert_eq!(queue.list(crate::queue::CONFIRM_KIND).unwrap().len(), 1);
}

#[test]
fn conflicting_live_or_saved_line_names_block_automatic_voice_naming() {
    let (_tmp, lib) = setup(live_people(), vec![]);
    let people = load_person_samples(&lib, MODEL).unwrap();
    let mut mixed = speaker_lines(&[ANA, ANA, BIA]);
    resolve_meeting(&mut mixed, MODEL, &people, Some(&thresholds()), None).unwrap();
    reconcile_live_speakers(&lib, &mut mixed, &LiveConfig::default()).unwrap();
    assert_eq!(mixed.speakers[0].assignment, Assignment::None);
    let note = mixed
        .review_notes
        .iter()
        .find(|note| note.source == ReviewSource::SpeakerIdentity)
        .unwrap();
    for word in [ANA_NAME, BIA_NAME, "00:00:01", "00:00:07"] {
        assert!(note.text.contains(word), "{}", note.text);
    }
    assert_eq!(mixed.utterance_people.len(), 3);

    let mut saved = speaker_lines(&[BIA]);
    saved.live_attributions.clear();
    saved.utterance_people[0].live_decision = None;
    resolve_meeting(&mut saved, MODEL, &people, Some(&thresholds()), None).unwrap();
    assert_eq!(saved.speakers[0].assignment, Assignment::None);
    assert_eq!(
        saved.speakers[0]
            .identity_conflict
            .as_ref()
            .unwrap()
            .candidate,
        Some(pid(ANA))
    );
    let names = UserSpeakerEvidence::of(&saved);
    assert!(!names.allows_sample(sid(1), &pid(ANA), 0.5, 1.5));
    assert!(names.allows_sample(sid(1), &pid(ANA), 4.0, 6.0));

    let auto = &mut saved.speakers[0];
    auto.assignment = Assignment::Auto;
    auto.person_id = Some(pid(BIA));
    resolve_meeting(&mut saved, MODEL, &people, Some(&thresholds()), None).unwrap();
    assert_eq!(saved.speakers[0].assignment, Assignment::None);
    assert_eq!(
        saved.speakers[0]
            .identity_conflict
            .as_ref()
            .unwrap()
            .candidate,
        Some(pid(ANA))
    );
}

#[test]
fn undo_fence_follows_live_lines_through_a_speaker_split() {
    let mut old = speaker_lines(&[ANA, ANA, ANA, ANA]);
    old.speakers[0].live_speaker_override = true;
    let (_tmp, lib) = setup(live_people(), vec![]);
    let mut split = old.clone();
    split.speakers = vec![
        Speaker::new(sid(3), "system"),
        Speaker::new(sid(4), "system"),
    ];
    for (i, segment) in split.segments.iter_mut().enumerate() {
        segment.speaker = Some(if i < 2 { sid(3) } else { sid(4) });
    }
    carry_live_speaker_overrides(&old, &mut split);
    reconcile_live_speakers(&lib, &mut split, &LiveConfig::default()).unwrap();
    assert!(
        split
            .speakers
            .iter()
            .all(|speaker| speaker.live_speaker_override && speaker.live_promotion.is_none())
    );
}

#[test]
fn live_naming_respects_rejections_explicit_names_and_the_paired_microphone() {
    let (_tmp, lib) = setup(live_people(), vec![]);
    let mut named = speaker_lines(&[ANA, ANA]);
    named.speakers[0].rejected_person_ids = vec![pid(ANA)];
    reconcile_live_speakers(&lib, &mut named, &LiveConfig::default()).unwrap();
    assert!(named.speakers[0].live_promotion.is_none());
    assert!(
        named.speakers[0]
            .identity_conflict
            .as_ref()
            .unwrap()
            .rejected
    );

    named.speakers[0].person_id = Some(pid(BIA));
    named.speakers[0].assignment = Assignment::Confirmed;
    reconcile_live_speakers(&lib, &mut named, &LiveConfig::default()).unwrap();
    assert_eq!(named.speakers[0].named_person(), Some(&pid(BIA)));
    assert!(named.speakers[0].live_promotion.is_none());

    let mut mic = speaker_lines(&[ANA, ANA]);
    mic.tracks = vec![
        schema::Track {
            id: "system".into(),
            role: schema::TrackRole::System,
        },
        schema::Track {
            id: "mic".into(),
            role: schema::TrackRole::Mic,
        },
    ];
    mic.speakers[0].track = "mic".into();
    for line in &mut mic.segments {
        line.track = "mic".into();
    }
    for line in &mut mic.utterance_people {
        line.track = "mic".into();
    }
    for r in &mut mic.live_attributions {
        r.range.track = "mic".into();
    }
    reconcile_live_speakers(&lib, &mut mic, &LiveConfig::default()).unwrap();
    assert!(mic.speakers[0].live_promotion.is_none());
    lib.write(|tx| {
        let mut person = lib.load_person(&pid(ANA))?;
        person.is_self = true;
        tx.save_person(&person)
    })
    .unwrap();
    reconcile_live_speakers(&lib, &mut mic, &LiveConfig::default()).unwrap();
    assert_eq!(mic.speakers[0].named_person(), Some(&pid(ANA)));
}

#[test]
fn clear_forget_and_undo_remove_only_the_samples_the_live_name_taught() {
    for action in ["clear", "forget", "undo"] {
        let mut m = speaker_lines(&[ANA, ANA]);
        let (_tmp, lib) = setup(live_people(), vec![m.clone()]);
        reconcile_live_speakers(&lib, &mut m, &LiveConfig::default()).unwrap();
        let mut print = new_voiceprint(
            &pid(ANA),
            MODEL,
            &m,
            &basis(0),
            1.0,
            3.0,
            VoiceprintKind::LiveConfirmation,
            Strength::Confirmed,
        );
        print.source.set_speaker(&m, sid(1));
        with_index(&lib, |tx, index| {
            tx.upsert_voiceprints(&pid(ANA), vec![print])?;
            save_and_index(tx, index, &mut m)
        })
        .unwrap();
        with_index(&lib, |tx, index| {
            match action {
                "clear" => {
                    let mut cleared = m.clone();
                    cleared.live_attributions[1].action = LiveAction::Clear;
                    cleared.live_attributions[1].outcome = LiveOutcome::Cleared;
                    cleared.utterance_people.pop();
                    reconcile_live_speakers(&lib, &mut cleared, &LiveConfig::default())?;
                    sync_live_promotions(tx, Some(&m), &cleared, MODEL)?;
                    save_and_index(tx, index, &mut cleared)?;
                }
                "forget" => {
                    assert_eq!(forget_voice(tx, index, ANA)?.speakers_changed, 1);
                }
                "undo" => {
                    undo_live_promotion(
                        tx,
                        index,
                        &m.id,
                        &crate::live_identity::speaker_namings(&m)[0],
                    )?;
                }
                _ => unreachable!(),
            }
            Ok(())
        })
        .unwrap();
        let mut saved = lib.load_meeting(&m.id).unwrap();
        assert!(saved.speakers[0].named_person().is_none(), "{action}");
        assert!(
            lib.load_voiceprints(&pid(ANA))
                .unwrap()
                .embeddings
                .iter()
                .all(|print| print.source.kind != VoiceprintKind::LiveConfirmation)
        );
        reconcile_live_speakers(&lib, &mut saved, &LiveConfig::default()).unwrap();
        assert!(saved.speakers[0].live_promotion.is_none(), "{action}");
        if action == "undo" {
            assert!(saved.speakers[0].live_speaker_override);
            assert_eq!(saved.utterance_people.len(), 2);
            assert_eq!(lib.load_voiceprints(&pid(ANA)).unwrap().embeddings.len(), 1);
            let people = load_person_samples(&lib, MODEL).unwrap();
            resolve_meeting(&mut saved, MODEL, &people, Some(&thresholds()), None).unwrap();
            reconcile_live_speakers(&lib, &mut saved, &LiveConfig::default()).unwrap();
            assert!(saved.speakers[0].named_person().is_none());
        }
    }
}

#[test]
fn grouping_and_ungrouping_preserve_a_members_live_promotion_and_voiceprint() {
    let mut m = speaker_lines(&[ANA, ANA]);
    let mut confirmed = spk(2, Some(basis(0)));
    confirmed.track = "system".into();
    m.speakers[0].id = sid(2);
    for segment in &mut m.segments {
        segment.speaker = Some(sid(2));
    }
    confirmed.id = sid(1);
    confirmed.assignment = Assignment::Confirmed;
    confirmed.person_id = Some(pid(ANA));
    m.speakers.insert(0, confirmed);

    let (_tmp, lib) = setup(live_people(), vec![m.clone()]);
    reconcile_live_speakers(&lib, &mut m, &LiveConfig::default()).unwrap();
    let before = m.speaker(sid(2)).unwrap().clone();
    assert!(before.live_promotion.is_some());
    let mut print = new_voiceprint(
        &pid(ANA),
        MODEL,
        &m,
        &basis(0),
        1.0,
        3.0,
        VoiceprintKind::LiveConfirmation,
        Strength::Confirmed,
    );
    print.source.set_speaker(&m, sid(2));
    with_index(&lib, |tx, index| {
        tx.upsert_voiceprints(&pid(ANA), vec![print.clone()])?;
        save_and_index(tx, index, &mut m)
    })
    .unwrap();
    let group = with_index(&lib, |tx, index| {
        group_speakers(
            tx,
            index,
            &m.id,
            sid(1),
            &[sid(2)],
            schema::GroupOrigin::User,
            &SpeakerFence::of(&m),
            MODEL,
            None,
        )
    })
    .unwrap()
    .group;
    assert_eq!(group.kept, sid(1));
    let grouped = lib.load_meeting(&m.id).unwrap();
    assert_eq!(grouped.speaker(sid(2)), Some(&before));
    with_index(&lib, |tx, index| {
        ungroup_speakers(tx, index, &m.id, &group.id, &SpeakerFence::of(&grouped))
    })
    .unwrap();
    let ungrouped = lib.load_meeting(&m.id).unwrap();
    assert_eq!(ungrouped.speaker(sid(2)), Some(&before));
    assert!(
        lib.load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .contains(&print)
    );
}

#[test]
fn grouped_promotion_still_requires_enough_lines_on_one_raw_speaker() {
    let mut m = speaker_lines(&[ANA, ANA, ANA]);
    let mut member = spk(2, None);
    member.track = "system".into();
    m.speakers[0].talk_s = 30.0;
    m.speakers.push(member);
    m.segments[2].speaker = Some(sid(2));
    let (_tmp, lib) = setup(live_people(), vec![m.clone()]);
    reconcile_live_speakers(&lib, &mut m, &LiveConfig::default()).unwrap();
    assert!(m.speakers[0].live_promotion.is_some());
    with_index(&lib, |tx, index| save_and_index(tx, index, &mut m)).unwrap();
    let out = with_index(&lib, |tx, index| {
        group_speakers(
            tx,
            index,
            &m.id,
            sid(1),
            &[sid(2)],
            schema::GroupOrigin::User,
            &SpeakerFence::of(&m),
            MODEL,
            None,
        )
    })
    .unwrap();
    assert_eq!(out.group.kept, sid(1));
    let before = lib.load_meeting(&m.id).unwrap();
    assert!(before.speakers[0].live_promotion.is_some());
    let line = segment_fingerprint(&before.segments[1]);
    with_index(&lib, |tx, index| {
        set_utterance_people(tx, index, &m.id, &[line], None)
    })
    .unwrap();
    let after = lib.load_meeting(&m.id).unwrap();
    assert_eq!(
        effective_attribution(&after)
            .iter()
            .filter(|line| line.live_decision.is_some())
            .count(),
        2
    );
    assert!(after.speakers[0].live_promotion.is_none());
}

#[test]
fn the_library_review_list_is_every_meetings_open_live_decisions() {
    let (_tmp, lib, fp) = jointly_supported();
    let quiet = meeting(MB, 11, vec![spk(1, None)]);
    lib.write(|tx| {
        tx.create_meeting_dir(&quiet)?;
        tx.save_meeting(&quiet)
    })
    .unwrap();
    let before = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(effective_attribution(&before)[0].live_decision, Some(1));

    let reviews = live_reviews(&lib).unwrap();
    assert_eq!(
        reviews.len(),
        1,
        "a meeting without open decisions is not listed"
    );
    assert_eq!(reviews[0].meeting_id, mid(MA));
    assert_eq!(reviews[0].recording_sha256, None, "no audio to play");
    assert_eq!(reviews[0].items, live_review_items(&before));
    assert_eq!(reviews[0].items[0].lines, vec![fp.clone()]);

    with_index(&lib, |tx, idx| {
        set_utterance_people(tx, idx, &mid(MA), std::slice::from_ref(&fp), None)
    })
    .unwrap();
    assert!(
        live_reviews(&lib).unwrap().is_empty(),
        "settled by the line edit"
    );
}

/// An unmapped decision (its audio was cut: no final line to correct) next
/// to a line the user corrected on the meeting.
fn unmapped() -> (TempDir, Library) {
    let mut m = meeting(MA, 10, Vec::new());
    m.segments = vec![Segment {
        speech_overlap: Vec::new(),
        id: 0,
        track: "system".into(),
        speaker: None,
        start: 1.0,
        end: 3.0,
        text: "synthetic user line".into(),
        words: Vec::new(),
    }];
    let fp = segment_fingerprint(&m.segments[0]);
    m.utterance_people.push(schema::UtterancePerson {
        track: fp.track,
        start: fp.start,
        end: fp.end,
        text_sha256: fp.text_sha256,
        person_id: pid(ANA),
        at: schema::now(),
        live_decision: None,
    });
    let mut cut = record(4, BIA, (40.0, 42.0), LiveOutcome::Unmapped, Vec::new());
    cut.from_s = None;
    cut.to_s = None;
    m.live_attributions = vec![cut];
    let names = BTreeMap::from([(pid(BIA), BIA_NAME.to_owned())]);
    refresh_review_notes(&mut m, &names);
    setup(
        vec![
            (Person::new(pid(ANA), ANA_NAME), Vec::new()),
            (Person::new(pid(BIA), BIA_NAME), vec![basis(1)]),
        ],
        vec![m],
    )
}

fn dismiss(lib: &Library, decision: u64, key: &str) -> Result<LiveDismissOutcome> {
    with_index(lib, |tx, idx| {
        dismiss_live_review(tx, idx, &mid(MA), decision, key)
    })
}

#[test]
fn an_unmapped_decision_is_left_as_transcribed_and_stays_settled() {
    let (_tmp, lib) = unmapped();
    let before = lib.load_meeting(&mid(MA)).unwrap();
    let prints = lib.load_voiceprints(&pid(BIA)).unwrap();
    let items = live_review_items(&before);
    assert_eq!(items.len(), 1);
    assert!(
        !items[0].mapped && items[0].lines.is_empty(),
        "nothing to correct"
    );
    assert_eq!(live_notes(&before).len(), 1);

    assert_eq!(
        dismiss(&lib, 99, &items[0].review_key).unwrap_err().code(),
        "not_found"
    );
    let done = dismiss(&lib, 4, &items[0].review_key).unwrap();
    assert!(done.changed);
    assert_eq!(done.remaining, 0);

    let after = lib.load_meeting(&mid(MA)).unwrap();
    assert!(live_review_items(&after).is_empty());
    assert!(live_notes(&after).is_empty());
    assert_eq!(
        after.utterance_people, before.utterance_people,
        "lines untouched"
    );
    assert_eq!(after.speakers, before.speakers);
    assert_eq!(after.live_attributions[0].outcome, LiveOutcome::Unmapped);
    assert_eq!(
        lib.load_voiceprints(&pid(BIA)).unwrap(),
        prints,
        "nothing learned"
    );
    assert!(live_reviews(&lib).unwrap().is_empty());

    // Durable and idempotent: a rebuild brings nothing back, a repeat
    // changes nothing.
    lib.write(|tx| Index::rebuild(tx)).unwrap();
    let again = dismiss(&lib, 4, &items[0].review_key).unwrap();
    assert!(!again.changed);
    assert_eq!(lib.load_meeting(&mid(MA)).unwrap(), after);
}

#[test]
fn a_changed_item_or_a_reused_number_is_refused_as_stale() {
    let (_tmp, lib, _fp) = jointly_supported();
    let shown = live_review_items(&lib.load_meeting(&mid(MA)).unwrap());
    let key = shown[0].review_key.clone();
    assert_eq!(
        dismiss(&lib, 3, "not-the-key").unwrap_err().code(),
        STALE_REVIEW
    );

    // A reprocess rewrote the line the decision overlaps.
    lib.write(|tx| {
        tx.update_meeting(&mid(MA), |m| {
            m.segments[0].text = "synthetic rewritten line".into();
            Ok(())
        })
        .map(|_| ())
    })
    .unwrap();
    assert_eq!(dismiss(&lib, 3, &key).unwrap_err().code(), STALE_REVIEW);
    let fresh = live_review_items(&lib.load_meeting(&mid(MA)).unwrap());
    assert_eq!(fresh.len(), 1, "still awaiting review");

    // The same decision number now stands for another turn and range.
    let fresh_key = fresh[0].review_key.clone();
    lib.write(|tx| {
        tx.update_meeting(&mid(MA), |m| {
            let r = m
                .live_attributions
                .iter_mut()
                .find(|a| a.decision == 3)
                .unwrap();
            r.turn_id = "system-reused".into();
            r.range = live_range(2.6, 4.0);
            Ok(())
        })
        .map(|_| ())
    })
    .unwrap();
    assert_eq!(
        dismiss(&lib, 3, &fresh_key).unwrap_err().code(),
        STALE_REVIEW
    );
    let current = live_review_items(&lib.load_meeting(&mid(MA)).unwrap());
    assert!(dismiss(&lib, 3, &current[0].review_key).unwrap().changed);
}

#[test]
fn a_forgotten_decision_is_not_settled_and_nobody_comes_back() {
    let (_tmp, lib, _fp) = jointly_supported();
    let key = live_review_items(&lib.load_meeting(&mid(MA)).unwrap())[0]
        .review_key
        .clone();
    with_index(&lib, |tx, idx| forget_person(tx, idx, BIA)).unwrap();
    let forgotten = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(dismiss(&lib, 3, &key).unwrap_err().code(), STALE_REVIEW);
    assert_eq!(
        lib.load_meeting(&mid(MA)).unwrap(),
        forgotten,
        "nothing written"
    );
    assert!(!lib.person_dir(&pid(BIA)).exists());
    let r = forgotten
        .live_attributions
        .iter()
        .find(|a| a.decision == 3)
        .unwrap();
    assert_eq!(
        (r.outcome, r.person_id.as_ref()),
        (LiveOutcome::Forgotten, None)
    );
}

#[test]
fn a_person_with_only_an_open_live_decision_shows_it_until_settled() {
    let (_tmp, lib, _fp) = jointly_supported();
    let view = |lib: &Library| {
        let index = Index::open_readonly(lib.root()).unwrap();
        person_view(lib, &index, &pid(BIA)).unwrap()
    };
    let shown = view(&lib);
    assert_eq!(
        shown.meetings.len(),
        1,
        "no speaker, no line: the decision only"
    );
    let m = &shown.meetings[0];
    assert_eq!(m.meeting_id, mid(MA));
    assert!(m.speakers.is_empty() && m.lines == 0);
    assert_eq!(m.live_review.len(), 1);
    assert_eq!(m.live_review[0].decision, 3);
    // No speaker: no talk time, serialized as 0.0, never -0.0.
    assert!(m.talk_s == 0.0 && m.talk_s.is_sign_positive());
    let json = serde_json::to_string(&shown).unwrap();
    assert!(json.contains(r#""talk_s":0.0"#), "{json}");

    dismiss(&lib, 3, &m.live_review[0].review_key).unwrap();
    assert!(view(&lib).meetings.is_empty());
}

#[test]
fn clearing_a_jointly_supported_line_overrides_every_supporting_decision() {
    let (_tmp, lib, fp) = jointly_supported();
    let before = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(live_notes(&before).len(), 1);
    assert!(live_notes(&before)[0].contains(BIA_NAME));
    let items = live_review_items(&before);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].decision, 3);
    assert_eq!(items[0].lines, vec![fp.clone()]);

    let cleared = with_index(&lib, |tx, idx| {
        set_utterance_people(tx, idx, &mid(MA), std::slice::from_ref(&fp), None)
    })
    .unwrap();
    assert!(cleared.utterance_people.is_empty());
    // Not only the decision the line recorded: every one claiming it.
    assert!(cleared.live_attributions.iter().all(|a| a.user_override));
    // The partial decision the user settled leaves review and its note.
    assert!(live_review_items(&cleared).is_empty());
    assert!(live_notes(&cleared).is_empty());

    // Durable: reloading and rebuilding the index bring nothing back.
    lib.write(|tx| Index::rebuild(tx)).unwrap();
    let after = lib.load_meeting(&mid(MA)).unwrap();
    assert!(after.utterance_people.is_empty());
    assert!(after.live_attributions.iter().all(|a| a.user_override));
    assert!(effective_attribution(&after)[0].person_id.is_none());
    // A repeated clear changes nothing.
    let again = with_index(&lib, |tx, idx| {
        set_utterance_people(tx, idx, &mid(MA), std::slice::from_ref(&fp), None)
    })
    .unwrap();
    assert_eq!(again, after);
}

#[test]
fn forgetting_a_person_regenerates_live_notes_without_their_name() {
    let mut m = meeting(MA, 10, Vec::new());
    m.segments = vec![Segment {
        speech_overlap: Vec::new(),
        id: 0,
        track: "system".into(),
        speaker: None,
        start: 1.0,
        end: 3.0,
        text: "synthetic disputed line".into(),
        words: Vec::new(),
    }];
    m.live_attributions = vec![
        record(
            1,
            ANA,
            (1.0, 3.0),
            LiveOutcome::Conflicting,
            vec![[1.0, 3.0]],
        ),
        record(
            2,
            BIA,
            (1.0, 3.0),
            LiveOutcome::Conflicting,
            vec![[1.0, 3.0]],
        ),
    ];
    let names = BTreeMap::from([
        (pid(ANA), ANA_NAME.to_owned()),
        (pid(BIA), BIA_NAME.to_owned()),
    ]);
    refresh_review_notes(&mut m, &names);
    assert_eq!(live_notes(&m).len(), 2);
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), ANA_NAME), Vec::new()),
            (Person::new(pid(BIA), BIA_NAME), Vec::new()),
        ],
        vec![m],
    );

    with_index(&lib, |tx, idx| forget_person(tx, idx, ANA)).unwrap();
    let after = lib.load_meeting(&mid(MA)).unwrap();
    let notes = live_notes(&after);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains(BIA_NAME));
    assert!(
        after
            .review_notes
            .iter()
            .all(|n| !n.text.contains(ANA_NAME))
    );
    let ana = &after.live_attributions[0];
    assert_eq!(ana.outcome, LiveOutcome::Forgotten);
    assert!(ana.person_id.is_none());
    let items = live_review_items(&after);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].person_id, Some(pid(BIA)));
    // Nothing on disk names the forgotten person.
    assert!(!md(&lib, MA).contains(ANA_NAME));
}

#[test]
fn user_name_writes_refuse_control_characters() {
    let m = meeting(MA, 10, vec![spk(1, None)]);
    let (_tmp, lib) = setup(Vec::new(), vec![m]);
    for bad in ["Ana\u{202E}xample", "Ana\nExample", "Ana\u{0007}"] {
        let err = with_index(&lib, |tx, idx| {
            name_speaker(tx, idx, &mid(MA), sid(1), bad, Vec::new(), MODEL)
        })
        .unwrap_err();
        assert_eq!(err.code(), "invalid_input", "{bad:?}");
        let fp = segment_fingerprint(&lib.load_meeting(&mid(MA)).unwrap().segments[0]);
        let err = with_index(&lib, |tx, idx| {
            set_utterance_people(tx, idx, &mid(MA), &[fp], Some(bad))
        })
        .unwrap_err();
        assert_eq!(err.code(), "invalid_input", "{bad:?}");
    }
    // Nothing was created and no reevaluation was requested.
    assert!(lib.list_people().unwrap().is_empty());
    assert!(crate::learning::status(&lib, MODEL).unwrap().is_none());
    // A plain name still creates the person.
    let out = with_index(&lib, |tx, idx| {
        name_speaker(
            tx,
            idx,
            &mid(MA),
            sid(1),
            "  Ana Example ",
            Vec::new(),
            MODEL,
        )
    })
    .unwrap();
    assert!(out.created);
    assert_eq!(lib.load_person(&out.person_id).unwrap().name, ANA_NAME);
}

/// A stored vector of `dim` raw values (bypassing normalisation).
fn raw_print(person: &str, values: &[f32], start: f64) -> Voiceprint {
    let mut vp = confirmed_print(&pid(person), &basis(0), start);
    vp.dim = values.len();
    vp.vector_b64 = schema::encode_vector(values);
    vp
}

#[test]
fn non_finite_samples_never_rank_and_are_repaired_explicitly() {
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), ANA_NAME), vec![basis(1)]),
            (Person::new(pid(BIA), BIA_NAME), Vec::new()),
        ],
        Vec::new(),
    );
    let mut nan = vec![0.0; DIM];
    nan[0] = f32::NAN;
    lib.write(|tx| {
        tx.upsert_voiceprints(&pid(ANA), vec![raw_print(ANA, &nan, 6.0)])?;
        tx.upsert_voiceprints(&pid(BIA), vec![raw_print(BIA, &[f32::INFINITY; DIM], 3.0)])
    })
    .unwrap();
    // Read: the NaN and infinite samples are skipped; Bia has none left.
    let people = load_person_samples(&lib, MODEL).unwrap();
    assert_eq!(people.all().len(), 1);
    assert_eq!(people.all()[0].person_id, pid(ANA));
    assert_eq!(people.all()[0].confirmed.len(), 1);
    let ranked = identity::rank(&basis(0), people.all(), &[], None, None);
    assert!(ranked.iter().all(|r| r.score.is_finite()));
    assert_eq!(ranked.len(), 1);

    // A non-finite centroid neither ranks nor names nor teaches.
    let mut s = spk(1, None);
    s.centroid = Some(Centroid::new(MODEL, &nan, 3));
    let mut m = meeting(MA, 10, vec![s]);
    let changes = resolve_meeting(&mut m, MODEL, &people, Some(&thresholds()), None).unwrap();
    assert!(changes.is_empty());
    assert_eq!(m.speakers[0].assignment, Assignment::None);
    assert_eq!(weak_sample(&m, sid(1), &pid(ANA)).unwrap(), None);
    assert!(
        voice_matches(&people, &m, sid(1), None, None, MODEL, 3)
            .unwrap()
            .is_empty()
    );

    // The check reports counts and IDs only; the repair is explicit.
    let check = identity_check(&lib).unwrap();
    assert!(!check.is_clean());
    assert_eq!(
        check.unusable,
        vec![
            UnusableSamples {
                person_id: pid(ANA),
                undecodable: 0,
                non_finite: 1,
            },
            UnusableSamples {
                person_id: pid(BIA),
                undecodable: 0,
                non_finite: 1,
            },
        ]
    );
    let json = serde_json::to_string(&check).unwrap();
    assert!(!json.contains(ANA_NAME) && !json.contains(BIA_NAME));
    let before = fs::read(lib.person_dir(&pid(ANA)).join(paths::VOICEPRINTS_JSON)).unwrap();
    let repaired = lib.write(repair_identity).unwrap();
    assert_eq!(repaired.samples_removed, 2);
    assert_eq!(repaired.people, vec![pid(ANA), pid(BIA)]);
    assert!(!repaired.learning.is_empty());
    assert_ne!(
        fs::read(lib.person_dir(&pid(ANA)).join(paths::VOICEPRINTS_JSON)).unwrap(),
        before
    );
    assert!(identity_check(&lib).unwrap().is_clean());
    assert_eq!(lib.load_voiceprints(&pid(ANA)).unwrap().embeddings.len(), 1);
    let again = lib.write(repair_identity).unwrap();
    assert_eq!(again.samples_removed, 0);
}
