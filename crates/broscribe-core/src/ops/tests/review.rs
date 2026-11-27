//! Review samples and "Can't tell" (design §4, §6.4): which lines play as
//! a speaker's voice samples, and dismissing a speaker from review with no
//! identity effect.

use super::*;
use crate::queue::Queue;
use crate::schema::{GroupOrigin, Handling, Recording, SpeakerGroup, UtterancePerson};

fn line(id: u32, speaker: u32, start: f64, end: f64, text: &str) -> Segment {
    Segment {
        speech_overlap: Vec::new(),
        id,
        track: "t0".into(),
        speaker: Some(sid(speaker)),
        start,
        end,
        text: text.into(),
        words: Vec::new(),
    }
}

fn clips(m: &Meeting, n: u32) -> Vec<(f64, f64, String)> {
    sample_clips(m, sid(n))
        .into_iter()
        .map(|c| (c.start, c.end, c.text))
        .collect()
}

fn clip(start: f64, end: f64, text: &str) -> (f64, f64, String) {
    (start, end, text.to_owned())
}

#[test]
fn wordless_review_uses_saved_ranges_only_with_available_audio() {
    let mut m = meeting(MA, 10, vec![spk(1, Some(basis(0))), spk(2, None)]);
    m.segments = vec![line(0, 2, 3.0, 3.25, "yes")];
    m.speakers[0].centroid.as_mut().unwrap().ranges =
        vec![[1., 1.5], [12., 32.], [-1., 1.], [61., 62.], [59., 65.]];
    m.recording = Some(Recording {
        state: RecordingState::Available,
        handling: Handling::Copy,
        path: "audio.m4a".into(),
        original_path: None,
        sha256: "ab".repeat(32),
        bytes: 9,
        source_sha256: None,
        sources: Vec::new(),
        retention_override: None,
        delete_after: None,
    });
    assert_eq!(
        clips(&m, 1),
        vec![clip(12., 27., ""), clip(59., 60., ""), clip(1., 1.5, "")]
    );
    assert_eq!(clips(&m, 2), vec![clip(3., 3.25, "yes")]);

    let (_tmp, lib) = setup(Vec::new(), vec![m.clone()]);
    let recording = lib.meeting_audio_path(&m).unwrap().unwrap();
    let hidden = || {
        review_queue(&lib, None)
            .unwrap()
            .into_iter()
            .find(|row| row.item.speaker_id == sid(1))
            .unwrap()
            .without_transcript
    };
    assert!(hidden());
    assert_eq!(review_naming_count(&lib).unwrap(), 1);
    std::fs::write(&recording, b"synthetic").unwrap();
    assert!(!hidden());
    assert_eq!(review_naming_count(&lib).unwrap(), 2);

    m.recording.as_mut().unwrap().state = RecordingState::Deleted;
    lib.write(|tx| tx.save_meeting(&m)).unwrap();
    assert!(hidden());
    assert_eq!(review_naming_count(&lib).unwrap(), 1);
}

#[test]
fn samples_are_the_longest_lines_capped_and_at_most_five() {
    let mut m = meeting(MA, 10, vec![spk(1, None), spk(2, None)]);
    m.segments = vec![
        line(0, 1, 0.0, 1.0, "one"),
        line(1, 1, 2.0, 40.0, "long"),
        line(2, 1, 41.0, 44.0, "three"),
        line(3, 1, 45.0, 50.0, "five"),
        line(4, 1, 51.0, 53.0, "two a"),
        line(5, 1, 54.0, 56.0, "two b"),
        line(6, 1, 57.0, 57.5, "half"),
        line(7, 2, 60.0, 70.0, "other"),
    ];
    // Longest first (capped to MAX_SAMPLE_S), ties by time, five at most.
    assert_eq!(
        clips(&m, 1),
        vec![
            clip(2.0, 2.0 + MAX_SAMPLE_S, "long"),
            clip(45.0, 50.0, "five"),
            clip(41.0, 44.0, "three"),
            clip(51.0, 53.0, "two a"),
            clip(54.0, 56.0, "two b"),
        ]
    );
    assert_eq!(clips(&m, 2), vec![clip(60.0, 70.0, "other")]);
    // Length only ranks: a speaker with only tiny lines still gets them.
    m.segments = vec![line(0, 1, 0.0, 0.3, "hm"), line(1, 1, 1.0, 1.2, "ok")];
    assert_eq!(
        clips(&m, 1),
        vec![clip(0.0, 0.3, "hm"), clip(1.0, 1.2, "ok")]
    );
    assert!(clips(&m, 2).is_empty());
    assert!(clips(&m, 9).is_empty());
}

#[test]
fn samples_skip_lines_others_talk_over_within_the_played_clip() {
    let mut m = meeting(MA, 10, vec![spk(1, None), spk(2, None)]);
    m.speakers[1].track = "t1".into();
    m.segments = vec![
        // 30 s line: the interruption is 20 % of it, but 40 % of the
        // 15 s actually played.
        line(0, 1, 0.0, 30.0, "long, talked over early"),
        line(1, 2, 0.0, 6.0, "interruption"),
        line(2, 1, 40.0, 42.0, "clear"),
        line(3, 2, 41.5, 42.0, "short overlap"),
    ];
    // Another track counts too: the tracks play mixed.
    m.segments[1].track = "t1".into();
    m.segments[3].track = "t1".into();
    assert_eq!(clips(&m, 1), vec![clip(40.0, 42.0, "clear")]);
    // Every line talked over: all of them, rather than none.
    m.segments[3].start = 40.0;
    assert_eq!(
        clips(&m, 1),
        vec![
            clip(0.0, MAX_SAMPLE_S, "long, talked over early"),
            clip(40.0, 42.0, "clear")
        ]
    );
}

#[test]
fn samples_follow_groups_and_leave_out_lines_corrected_to_someone_else() {
    let mut m = meeting(MA, 10, vec![spk(1, None), spk(2, None), spk(3, None)]);
    m.speakers[0].assignment = Assignment::Confirmed;
    m.speakers[0].person_id = Some(pid(ANA));
    m.segments = vec![
        line(0, 1, 0.0, 3.0, "kept"),
        line(1, 2, 5.0, 9.0, "member"),
        line(2, 1, 10.0, 16.0, "corrected to Bia"),
        line(3, 1, 20.0, 21.0, "corrected to Ana"),
        line(4, 3, 30.0, 31.0, "other"),
    ];
    m.speaker_groups = vec![SpeakerGroup::new(sid(1), [sid(2)], GroupOrigin::User)];
    let correction = |s: &Segment, person: &str| UtterancePerson {
        track: s.track.clone(),
        start: s.start,
        end: s.end,
        text_sha256: segment_fingerprint(s).text_sha256,
        person_id: pid(person),
        at: schema::now(),
        live_decision: None,
    };
    m.utterance_people = vec![
        correction(&m.segments[2], BIA),
        correction(&m.segments[3], ANA),
    ];
    let shown = clips(&m, 1);
    assert_eq!(
        shown,
        vec![
            clip(5.0, 9.0, "member"),
            clip(0.0, 3.0, "kept"),
            clip(20.0, 21.0, "corrected to Ana"),
        ]
    );
    // A member ID means its group.
    assert_eq!(clips(&m, 2), shown);
}

// ---- "Can't tell" ----

fn fence(lib: &Library) -> SpeakerFence {
    SpeakerFence::of(&lib.load_meeting(&mid(MA)).unwrap())
}

fn dismiss(lib: &Library, n: u32) -> Result<DismissOutcome> {
    let fence = fence(lib);
    with_index(lib, |tx, idx| {
        dismiss_review(tx, idx, &mid(MA), sid(n), &fence)
    })
}

fn restore(lib: &Library, n: u32) -> Result<DismissOutcome> {
    let fence = fence(lib);
    with_index(lib, |tx, idx| {
        restore_review(tx, idx, &mid(MA), sid(n), &fence)
    })
}

/// Shown speakers of MA in the review queue (both groups).
fn pending(lib: &Library) -> Vec<SpeakerId> {
    let mut ids: Vec<SpeakerId> = review_queue(lib, None)
        .unwrap()
        .into_iter()
        .filter(|r| r.item.meeting_id == mid(MA))
        .map(|r| r.item.speaker_id)
        .collect();
    ids.sort();
    ids
}

fn dismissed(lib: &Library) -> Vec<SpeakerId> {
    dismissed_speakers(lib)
        .unwrap()
        .into_iter()
        .map(|r| r.speaker_id)
        .collect()
}

fn meeting_json(lib: &Library) -> Vec<u8> {
    let dir = lib.meeting_dir(&mid(MA)).unwrap();
    fs::read(dir.join(paths::MEETING_JSON)).unwrap()
}

#[test]
fn dismissing_leaves_both_queue_groups_with_no_identity_effect() {
    let mut m = meeting(
        MA,
        10,
        vec![
            spk(1, Some(tilted())),
            spk(2, Some(basis(2))),
            spk(3, Some(basis(3))),
        ],
    );
    // S2 has no transcript: an unresolved quality row.
    m.segments.retain(|s| s.speaker != Some(sid(2)));
    m.speakers[0].assignment = Assignment::Suggested;
    m.speakers[0].suggestion = Some(Suggestion {
        person_id: pid(ANA),
        score: 0.6,
    });
    m.speakers[0].rejected_person_ids = vec![pid(BIA)];
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), vec![basis(0)]),
            (Person::new(pid(BIA), "Bia Example"), Vec::new()),
        ],
        vec![m],
    );
    assert_eq!(pending(&lib), vec![sid(1), sid(2), sid(3)]);
    assert_eq!(review_naming_count(&lib).unwrap(), 2);
    assert_eq!(
        review_naming_counts(&lib).unwrap(),
        BTreeMap::from([(mid(MA), 2)])
    );
    let before = lib.load_meeting(&mid(MA)).unwrap();
    let prints = lib.load_voiceprints(&pid(ANA)).unwrap();

    let out = dismiss(&lib, 1).unwrap();
    assert!(out.changed && out.dismissed);
    assert_eq!((out.speaker_id, out.redirected_from), (sid(1), None));
    assert_eq!(out.speakers, vec![sid(1)]);
    assert!(dismiss(&lib, 2).unwrap().changed);
    assert_eq!(pending(&lib), vec![sid(3)]);
    assert_eq!(review_naming_count(&lib).unwrap(), 1);
    assert_eq!(
        review_naming_counts(&lib).unwrap(),
        BTreeMap::from([(mid(MA), 1)])
    );
    assert_eq!(dismissed(&lib), vec![sid(1), sid(2)]);
    assert_eq!(review_dismissed_count(&lib).unwrap(), 2);
    let row = &dismissed_speakers(&lib).unwrap()[0];
    assert_eq!(row.assignment, Assignment::Suggested);
    assert_eq!(row.person_id.as_ref(), Some(&pid(ANA)));
    assert_eq!(row.sample_text, vec!["words of speaker S1"]);

    // Nothing but the flag changed: no rejection, sample, assignment or
    // suggestion.
    let mut after = lib.load_meeting(&mid(MA)).unwrap();
    assert!(after.speakers[0].review_dismissed_at.is_some());
    for s in &mut after.speakers {
        s.review_dismissed_at = None;
    }
    after.updated_at = before.updated_at;
    assert_eq!(after, before);
    assert_eq!(lib.load_voiceprints(&pid(ANA)).unwrap(), prints);

    // Idempotent.
    let json = meeting_json(&lib);
    let again = dismiss(&lib, 1).unwrap();
    assert!(!again.changed && again.dismissed);
    assert_eq!(meeting_json(&lib), json);

    // Restore (Undo) returns it to the queue.
    let back = restore(&lib, 1).unwrap();
    assert!(back.changed && !back.dismissed);
    assert!(!restore(&lib, 1).unwrap().changed);
    assert_eq!(pending(&lib), vec![sid(1), sid(3)]);
    assert_eq!(dismissed(&lib), vec![sid(2)]);

    // Naming a dismissed speaker clears its flag.
    with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), sid(2), "Bia Example", Vec::new(), MODEL)
    })
    .unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(m.speaker(sid(2)).unwrap().review_dismissed_at.is_none());
    assert!(dismissed(&lib).is_empty());
}

/// R2.1: the dismissed list is read from the meeting files, so a dismissed
/// speaker that is auto-named later (no longer pending) still shows there,
/// and is never brought back to the queue.
#[test]
fn a_dismissed_speaker_stays_dismissed_after_a_later_auto_name() {
    let m = meeting(MA, 10, vec![spk(1, Some(basis(0)))]);
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(ANA), "Ana Example"), Vec::new())],
        vec![m],
    );
    assert_eq!(pending(&lib), vec![sid(1)]);
    dismiss(&lib, 1).unwrap();
    assert!(pending(&lib).is_empty());
    assert_eq!(review_naming_count(&lib).unwrap(), 0);

    // Ana's voice becomes known; re-resolution auto-names the speaker.
    let changes = with_index(&lib, |tx, idx| {
        tx.upsert_voiceprints(&pid(ANA), vec![confirmed_print(&pid(ANA), &basis(0), 0.0)])?;
        re_resolve_all(tx, idx, MODEL, Some(&thresholds()))
    })
    .unwrap();
    assert_eq!(changes.len(), 1);
    let m = lib.load_meeting(&mid(MA)).unwrap();
    let s1 = m.speaker(sid(1)).unwrap();
    assert_eq!(s1.assignment, Assignment::Auto);
    assert!(review_dismissed(&m, sid(1)));
    assert!(pending(&lib).is_empty());
    assert_eq!(review_naming_count(&lib).unwrap(), 0);
    let rows = dismissed_speakers(&lib).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].assignment, Assignment::Auto);
    assert_eq!(rows[0].person_id.as_ref(), Some(&pid(ANA)));
    // Restore still works on it.
    restore(&lib, 1).unwrap();
    assert!(dismissed(&lib).is_empty());
}

#[test]
fn dismissal_is_fenced_and_needs_a_done_idle_meeting() {
    let m = meeting(MA, 10, vec![spk(1, None), spk(2, None)]);
    let (_tmp, lib) = setup(Vec::new(), vec![m.clone()]);
    let json = meeting_json(&lib);
    let call = |f: SpeakerFence, n: u32| {
        with_index(&lib, |tx, idx| {
            dismiss_review(tx, idx, &mid(MA), sid(n), &f)
        })
    };
    let mut stale = fence(&lib);
    stale.grouping_revision = "older".into();
    assert_eq!(call(stale, 1).unwrap_err().code(), "speakers_regrouped");
    let mut stale = fence(&lib);
    stale.speaker_namespace = "older".into();
    assert_eq!(call(stale, 1).unwrap_err().code(), "speakers_reanalysed");
    assert_eq!(call(fence(&lib), 9).unwrap_err().code(), "not_found");
    assert_eq!(meeting_json(&lib), json);

    // A provisional pass may still be replaced: refused.
    for (status, code) in [
        (ProcessingStatus::Provisional, "library_busy"),
        (ProcessingStatus::Analyzed, "library_busy"),
        (ProcessingStatus::Failed, "invalid_input"),
    ] {
        let mut m = m.clone();
        m.processing.status = status;
        lib.write(|tx| tx.save_meeting(&m)).unwrap();
        assert_eq!(dismiss(&lib, 1).unwrap_err().code(), code, "{status:?}");
        assert_eq!(restore(&lib, 1).unwrap_err().code(), code, "{status:?}");
    }
    lib.write(|tx| tx.save_meeting(&m)).unwrap();
    // A job working on the meeting.
    lib.write(|tx| {
        Queue::open(tx.root())?
            .enqueue(tx, "reprocess", MA, serde_json::json!({ "meeting_id": MA }))
            .map(|_| ())
    })
    .unwrap();
    assert_eq!(dismiss(&lib, 1).unwrap_err().code(), "library_busy");
    assert!(
        lib.load_meeting(&mid(MA))
            .unwrap()
            .speakers
            .iter()
            .all(|s| s.review_dismissed_at.is_none())
    );
}

/// A group is dismissed only while every one of its speakers is: a member
/// ID dismisses the whole group, merging a pending speaker in brings it
/// back, and unmerging leaves each speaker its own flag.
#[test]
fn groups_are_dismissed_together() {
    let mut m = meeting(
        MA,
        10,
        vec![
            spk(1, Some(basis(0))),
            spk(2, Some(basis(0))),
            spk(3, Some(basis(1))),
            spk(4, Some(basis(2))),
        ],
    );
    m.speaker_groups = vec![SpeakerGroup::new(sid(2), [sid(1)], GroupOrigin::User)];
    let (_tmp, lib) = setup(Vec::new(), vec![m]);
    assert_eq!(pending(&lib), vec![sid(2), sid(3), sid(4)]);

    let out = dismiss(&lib, 1).unwrap();
    assert_eq!(
        (out.speaker_id, out.redirected_from),
        (sid(2), Some(sid(1)))
    );
    assert_eq!(out.speakers, vec![sid(2), sid(1)]);
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(review_dismissed(&m, sid(1)) && review_dismissed(&m, sid(2)));
    assert_eq!(pending(&lib), vec![sid(3), sid(4)]);
    assert_eq!(dismissed(&lib), vec![sid(2)]);

    // Merging a pending speaker into the dismissed group returns it.
    let merged = with_index(&lib, |tx, idx| {
        group_speakers(
            tx,
            idx,
            &mid(MA),
            sid(2),
            &[sid(3)],
            GroupOrigin::User,
            &fence(&lib),
            MODEL,
            Some(&thresholds()),
        )
    })
    .unwrap();
    let kept = merged.group.kept;
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(!review_dismissed(&m, kept));
    assert_eq!(pending(&lib), vec![kept, sid(4)]);
    assert!(dismissed(&lib).is_empty());

    // Unmerge: each speaker keeps its own flag.
    let group = merged.group.id.clone();
    with_index(&lib, |tx, idx| {
        ungroup_speakers(tx, idx, &mid(MA), &group, &fence(&lib))
    })
    .unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(review_dismissed(&m, sid(1)) && review_dismissed(&m, sid(2)));
    assert!(!review_dismissed(&m, sid(3)));
    assert_eq!(pending(&lib), vec![sid(3), sid(4)]);
    assert_eq!(dismissed(&lib), vec![sid(1), sid(2)]);

    // Restoring through one speaker of a group clears every one of them.
    let mut m = lib.load_meeting(&mid(MA)).unwrap();
    m.speaker_groups = vec![SpeakerGroup::new(sid(2), [sid(1)], GroupOrigin::User)];
    m.not_same.clear();
    lib.write(|tx| tx.save_meeting(&m)).unwrap();
    let out = restore(&lib, 2).unwrap();
    assert_eq!(out.speakers, vec![sid(2), sid(1)]);
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(m.speakers.iter().all(|s| s.review_dismissed_at.is_none()));
}

/// Automatic grouping (`merge --auto`, the post-publication step) never
/// hides a pending speaker behind a dismissed one.
#[test]
fn automatic_grouping_never_hides_a_pending_speaker() {
    let m = meeting(
        MA,
        10,
        vec![
            spk(1, Some(basis(0))),
            spk(2, Some(basis(0))),
            spk(3, Some(basis(1))),
        ],
    );
    let (_tmp, lib) = setup(Vec::new(), vec![m]);
    dismiss(&lib, 1).unwrap();
    assert_eq!(pending(&lib), vec![sid(2), sid(3)]);
    let auto = Thresholds {
        merge: Some(0.5),
        auto_merge: Some(0.9),
        ..thresholds()
    };
    let out = with_index(&lib, |tx, idx| {
        auto_group_meeting(tx, idx, &mid(MA), MODEL, Some(&auto))
    })
    .unwrap();
    assert_eq!(out.groups_added, 1, "{out:?}");
    let m = lib.load_meeting(&mid(MA)).unwrap();
    let kept = effective_speaker(&m, sid(1));
    assert_eq!(kept, effective_speaker(&m, sid(2)));
    assert!(!review_dismissed(&m, kept));
    assert_eq!(pending(&lib), vec![kept, sid(3)]);
    assert!(dismissed(&lib).is_empty());
}
