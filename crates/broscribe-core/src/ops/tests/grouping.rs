//! Speaker grouping (design §6.4): the overlay, its invariants, the writes
//! and how identity writes and automatic producers treat grouped speakers.

use super::*;
use crate::index::{ActionQuery, DecisionQuery};
use crate::schema::{GroupOrigin, SpeakerGroup};

const CAI: &str = "p-c3c3c3";

fn people() -> Vec<(Person, Vec<Vec<f32>>)> {
    vec![
        (Person::new(pid(ANA), "Ana Example"), Vec::new()),
        (Person::new(pid(BIA), "Bia Example"), Vec::new()),
        (Person::new(pid(CAI), "Cai Example"), Vec::new()),
    ]
}

fn named(mut s: Speaker, assignment: Assignment, person: &str) -> Speaker {
    s.assignment = assignment;
    match assignment {
        Assignment::Suggested => {
            s.suggestion = Some(Suggestion {
                person_id: pid(person),
                score: 0.6,
            });
        }
        _ => s.person_id = Some(pid(person)),
    }
    s
}

fn fence(lib: &Library) -> SpeakerFence {
    SpeakerFence::of(&lib.load_meeting(&mid(MA)).unwrap())
}

fn merge(lib: &Library, kept: u32, members: &[u32]) -> Result<GroupOutcome> {
    let fence = fence(lib);
    let members: Vec<SpeakerId> = members.iter().map(|&n| sid(n)).collect();
    with_index(lib, |tx, idx| {
        group_speakers(
            tx,
            idx,
            &mid(MA),
            sid(kept),
            &members,
            GroupOrigin::User,
            &fence,
            MODEL,
            Some(&thresholds()),
        )
    })
}

fn unmerge(lib: &Library, group: &str) -> Result<UngroupOutcome> {
    let fence = fence(lib);
    with_index(lib, |tx, idx| {
        ungroup_speakers(tx, idx, &mid(MA), group, &fence)
    })
}

fn auto_thresholds() -> Thresholds {
    Thresholds {
        merge: Some(0.5),
        auto_merge: Some(0.9),
        ..thresholds()
    }
}

/// A caption sample of `person` learned from `speaker` of `m`.
fn caption_print(m: &Meeting, person: &str, speaker: u32) -> Voiceprint {
    let s = m.speaker(sid(speaker)).unwrap();
    let mut vp = new_voiceprint(
        &pid(person),
        MODEL,
        m,
        &s.centroid.as_ref().unwrap().vector().unwrap(),
        f64::from(speaker) * 5.0,
        f64::from(speaker) * 5.0 + 2.0,
        VoiceprintKind::Captions,
        Strength::Weak,
    );
    vp.source.set_speaker(m, sid(speaker));
    vp
}

fn voiceprint_bytes(lib: &Library, person: &str) -> Vec<u8> {
    fs::read(lib.person_dir(&pid(person)).join(paths::VOICEPRINTS_JSON)).unwrap_or_default()
}

fn index(lib: &Library) -> Index {
    Index::open_readonly(lib.root()).unwrap()
}

fn talk_of(lib: &Library, person: &str) -> (u32, f64) {
    let p = index(lib)
        .list_people()
        .unwrap()
        .into_iter()
        .find(|p| p.id == pid(person))
        .unwrap();
    (p.meetings, p.talk_s)
}

fn meetings_with(lib: &Library, person: &str) -> usize {
    index(lib)
        .search_meetings(&SearchQuery {
            person: Some(person.into()),
            ..SearchQuery::default()
        })
        .unwrap()
        .len()
}

/// S1 confirmed Ana, S2 auto-named Bia, S3 suggested Bia, S4 unnamed; S1–S3
/// share a voice. An enrichment action is owned by S2.
fn shown_meeting() -> Meeting {
    let mut m = meeting(
        MA,
        10,
        vec![
            named(spk(1, Some(basis(0))), Assignment::Confirmed, ANA),
            named(spk(2, Some(basis(0))), Assignment::Auto, BIA),
            named(spk(3, Some(basis(0))), Assignment::Suggested, BIA),
            spk(4, Some(basis(1))),
        ],
    );
    m.segments
        .push(seg(4, 2, 2.5, "member speaking right after"));
    m.segments.sort_by(|a, b| a.start.total_cmp(&b.start));
    m.enrichment = Some(schema::Enrichment {
        provider: "test".into(),
        model: None,
        requested_model: None,
        model_reported: false,
        created_at: schema::now(),
        stale: false,
        revision: crate::index::meeting_window_revision(&m),
        attribution_revision: String::new(),
        line_people: BTreeMap::new(),
        summary: "{{S2}} reports.".into(),
        topics: Vec::new(),
        keywords: Vec::new(),
        tags: Vec::new(),
        decisions: vec![schema::Decision {
            id: "d1".into(),
            text: "{{S2}} decides".into(),
            at_s: 0.0,
            by_speaker: Some(sid(2)),
            by_name: None,
        }],
        actions: vec![schema::Action {
            id: "a1".into(),
            text: "{{S2}} sends the notes".into(),
            owner_speaker: Some(sid(2)),
            owner_name: None,
            due: None,
            at_s: 1.0,
            status: schema::ActionStatus::Open,
            status_source: schema::StatusSource::Llm,
            status_updated_at: None,
            original_owner: None,
            due_user_set: false,
            stale: false,
        }],
        instructions: None,
    });
    m.template_results.push(schema::TemplateResult {
        template_id: "meeting-notes".into(),
        template_name: "Meeting notes".into(),
        instructions_sha256: String::new(),
        provider: "test".into(),
        model: None,
        requested_model: None,
        model_reported: false,
        created_at: schema::now(),
        revision: String::new(),
        attribution_revision: String::new(),
        line_people: Default::default(),
        stale: false,
        markdown: "{{S2}} presented the numbers.".into(),
    });
    m
}

#[test]
fn every_consumer_shows_members_as_their_group_and_unmerge_is_exact() {
    let m = shown_meeting();
    let (_tmp, lib) = setup(people(), vec![m.clone()]);
    // The member's own auto sample, as finishing stored it.
    let bia_sample = weak_sample(&m, sid(2), &pid(BIA)).unwrap().unwrap();
    lib.write(|tx| tx.upsert_voiceprints(&pid(BIA), vec![bia_sample]))
        .unwrap();
    let before = lib.load_meeting(&mid(MA)).unwrap();
    let prints = (voiceprint_bytes(&lib, ANA), voiceprint_bytes(&lib, BIA));
    assert_eq!(meetings_with(&lib, BIA), 1);
    let review_before: Vec<SpeakerId> = review_queue(&lib, None)
        .unwrap()
        .into_iter()
        .map(|r| r.item.speaker_id)
        .collect();
    assert!(review_before.contains(&sid(3)), "{review_before:?}");

    // The user asks to keep S2; the confirmed S1 is kept instead.
    let out = merge(&lib, 2, &[1, 3]).unwrap();
    assert!(out.changed && out.kept_swapped);
    assert_eq!(out.group.kept, sid(1));
    assert_eq!(out.group.members, vec![sid(2), sid(3)]);
    assert_eq!(out.group.origin, GroupOrigin::User);

    let after = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(effective_speaker(&after, sid(2)), sid(1));
    assert_eq!(after.grouping_revision(), out.grouping_revision);
    // meeting.md: one turn of Ana where S1 and S2 alternate; no Bia.
    let text = md(&lib, MA);
    assert!(!text.contains("Bia Example"), "{text}");
    assert!(text.contains("Ana Example reports."), "{text}");
    assert!(
        !text.contains("Speaker 2") && !text.contains("Speaker 3"),
        "{text}"
    );
    assert!(
        text.contains("Ana Example: words of speaker S1 member speaking right after"),
        "{text}"
    );
    assert!(
        after.enrichment.as_ref().unwrap().stale && after.template_results[0].stale,
        "grouping stales the enrichment and template results"
    );
    // Template prose follows the shown speaker everywhere.
    let r = &after.template_results[0];
    let file = lib
        .meeting_dir(&mid(MA))
        .unwrap()
        .join(paths::MEETING_TEMPLATES_DIR)
        .join("meeting-notes.md");
    let file = fs::read_to_string(file).unwrap();
    assert!(
        file.contains("Ana Example presented the numbers."),
        "{file}"
    );
    assert!(file.contains("predates changes"), "{file}");
    let people = lib.people_lookup().unwrap();
    assert_eq!(
        crate::render::template_result_md(&after, r, &people),
        "Ana Example presented the numbers."
    );
    let hits = index(&lib).search_templates("presented", None).unwrap();
    assert!(hits[0].snippet.contains("Ana Example"), "{:?}", hits);
    // Index: utterances, windows and speakers rows per shown speaker.
    let hits = person_hits(&lib, ANA);
    assert_eq!(hits.len(), 4, "{hits:?}");
    assert!(hits.iter().all(|(_, _, name)| name == "Ana Example"));
    assert!(person_hits(&lib, BIA).is_empty());
    assert_eq!(meetings_with(&lib, BIA), 0);
    assert_eq!(talk_of(&lib, BIA), (0, 0.0));
    assert_eq!(talk_of(&lib, ANA), (1, 60.0));
    let windows = crate::index::meeting_windows(&after);
    assert!(windows.iter().all(|w| w.speaker != Some(sid(2))));
    let idx = index(&lib);
    let actions = idx
        .list_actions(&ActionQuery {
            owner: Some(pid(ANA)),
            ..ActionQuery::default()
        })
        .unwrap();
    assert_eq!(
        actions.len(),
        1,
        "an action owned by a member is the group's"
    );
    assert_eq!(actions[0].owner.as_deref(), Some("Ana Example"));
    assert_eq!(actions[0].text, "Ana Example sends the notes");
    let decisions = idx.list_decisions(&DecisionQuery::default()).unwrap();
    assert_eq!(decisions[0].by_person_id, Some(pid(ANA)));
    // Members are hidden from review.
    let review: Vec<SpeakerId> = review_queue(&lib, None)
        .unwrap()
        .into_iter()
        .map(|r| r.item.speaker_id)
        .collect();
    assert!(
        !review.contains(&sid(2)) && !review.contains(&sid(3)),
        "{review:?}"
    );
    assert!(review.contains(&sid(4)));

    // Unmerge: nothing about the speakers, segments or samples changed.
    let undone = unmerge(&lib, "g1").unwrap();
    assert_eq!(undone.group.id, "G1");
    assert_eq!(
        undone.not_same_added,
        vec![[sid(1), sid(2)], [sid(1), sid(3)], [sid(2), sid(3)]]
    );
    let restored = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(restored.speakers, before.speakers);
    assert_eq!(restored.segments, before.segments);
    assert!(restored.speaker_groups.is_empty());
    assert_eq!(
        (voiceprint_bytes(&lib, ANA), voiceprint_bytes(&lib, BIA)),
        prints
    );
    assert_eq!(meetings_with(&lib, BIA), 1);
    assert!(md(&lib, MA).contains("Bia Example"));
}

#[test]
fn merges_refuse_what_the_invariants_forbid() {
    let mut m = meeting(
        MA,
        10,
        vec![
            named(spk(1, Some(basis(0))), Assignment::Confirmed, ANA),
            named(spk(2, Some(basis(0))), Assignment::Confirmed, BIA),
            spk(3, Some(basis(0))),
            spk(4, Some(basis(0))),
            named(spk(5, Some(basis(0))), Assignment::Auto, BIA),
            spk(6, Some(basis(0))),
        ],
    );
    m.speaker_mut(sid(3)).unwrap().rejected_person_ids = vec![pid(ANA)];
    m.speaker_mut(sid(4)).unwrap().track = "t1".into();
    m.segments[3].track = "t1".into();
    let caption = caption_print(&m, BIA, 5);
    let (_tmp, lib) = setup(people(), vec![m.clone()]);
    lib.write(|tx| tx.upsert_voiceprints(&pid(BIA), vec![caption]))
        .unwrap();
    let refused = |kept: u32, member: u32, why: &str| {
        let err = merge(&lib, kept, &[member]).unwrap_err();
        assert_eq!(err.code(), "invalid_input", "{why}: {err}");
        assert!(
            lib.load_meeting(&mid(MA))
                .unwrap()
                .speaker_groups
                .is_empty()
        );
    };
    refused(1, 2, "two different confirmed people");
    refused(1, 3, "a member rejected the confirmed person");
    refused(1, 4, "across tracks");
    refused(1, 5, "a caption-proven other person counts as confirmed");
    // The same-person caption-proven speaker may join Bia; with the most
    // talk among the proven speakers, it is kept.
    assert_eq!(merge(&lib, 2, &[5]).unwrap().group.kept, sid(5));
    // Adding to a group extends it; a speaker is never in two groups.
    merge(&lib, 6, &[2]).unwrap();
    let g = lib.load_meeting(&mid(MA)).unwrap().speaker_groups;
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].kept, sid(5));
    assert_eq!(g[0].members, vec![sid(2), sid(6)]);

    // A stale fence (regrouped, or reanalysed) changes nothing.
    let mut stale = fence(&lib);
    stale.grouping_revision = String::new();
    let err = with_index(&lib, |tx, idx| {
        ungroup_speakers(tx, idx, &mid(MA), "G5", &stale)
    })
    .unwrap_err();
    assert_eq!(err.code(), "speakers_regrouped");
    let mut stale = fence(&lib);
    stale.speaker_namespace = "older".into();
    let err = with_index(&lib, |tx, idx| {
        mark_not_same(tx, idx, &mid(MA), sid(1), sid(3), &stale)
    })
    .unwrap_err();
    assert_eq!(err.code(), "speakers_reanalysed");
    assert_eq!(lib.load_meeting(&mid(MA)).unwrap().speaker_groups, g);

    // A job working on the meeting refuses every grouping write.
    lib.write(|tx| {
        crate::queue::Queue::open(tx.root())?
            .enqueue(tx, "reprocess", MA, serde_json::json!({ "meeting_id": MA }))
            .map(|_| ())
    })
    .unwrap();
    assert_eq!(unmerge(&lib, "G5").unwrap_err().code(), "library_busy");
    let skipped = with_index(&lib, |tx, idx| {
        auto_group_meeting(tx, idx, &mid(MA), MODEL, Some(&auto_thresholds()))
    })
    .unwrap();
    assert!(skipped.skipped.is_some() && skipped.groups_added == 0);
}

#[test]
fn the_paired_microphone_track_is_never_grouped() {
    let mut m = meeting(MA, 10, vec![spk(1, Some(basis(0))), spk(2, Some(basis(0)))]);
    m.tracks = vec![
        schema::Track {
            id: "t0".into(),
            role: schema::TrackRole::Mic,
        },
        schema::Track {
            id: "t9".into(),
            role: schema::TrackRole::System,
        },
    ];
    let (_tmp, lib) = setup(people(), vec![m]);
    assert_eq!(merge(&lib, 1, &[2]).unwrap_err().code(), "invalid_input");
    let out = with_index(&lib, |tx, idx| {
        auto_group_meeting(tx, idx, &mid(MA), MODEL, Some(&auto_thresholds()))
    })
    .unwrap();
    assert_eq!(out.groups_added, 0);
}

#[test]
fn a_hand_edited_invalid_group_is_ignored_with_a_note_and_never_saved() {
    let mut m = meeting(
        MA,
        10,
        vec![
            named(spk(1, None), Assignment::Confirmed, ANA),
            named(spk(2, None), Assignment::Confirmed, BIA),
            spk(3, None),
        ],
    );
    let (_tmp, lib) = setup(people(), vec![m.clone()]);
    m.speaker_groups = vec![
        SpeakerGroup::new(sid(1), [sid(2)], GroupOrigin::User),
        SpeakerGroup::new(sid(3), [sid(9)], GroupOrigin::User),
    ];
    m.not_same = vec![[sid(3), sid(1)]];
    let path = lib.meeting_dir(&mid(MA)).unwrap().join(paths::MEETING_JSON);
    fs::write(&path, serde_json::to_vec(&m).unwrap()).unwrap();
    let loaded = lib.load_meeting(&mid(MA)).unwrap();
    assert!(loaded.speaker_groups.is_empty());
    assert_eq!(
        loaded.not_same,
        vec![[sid(1), sid(3)]],
        "pairs are normalised"
    );
    assert_eq!(
        loaded
            .review_notes
            .iter()
            .filter(|n| n.source == schema::ReviewSource::Grouping)
            .count(),
        2
    );
    // Writing an invalid overlay is refused outright.
    let err = lib.write(|tx| tx.save_meeting(&m)).unwrap_err();
    assert_eq!(err.code(), "internal");
}

#[test]
fn naming_through_a_member_names_the_group_and_reconciles_its_members() {
    let m = meeting(
        MA,
        10,
        vec![
            named(spk(1, Some(basis(0))), Assignment::Confirmed, ANA),
            named(spk(2, Some(basis(0))), Assignment::Confirmed, ANA),
            spk(3, Some(basis(0))),
            spk(4, Some(basis(0))),
        ],
    );
    let (_tmp, lib) = setup(people(), vec![m]);
    lib.write(|tx| {
        tx.update_meeting(&mid(MA), |m| {
            m.speaker_mut(sid(3)).unwrap().rejected_person_ids = vec![pid(CAI)];
            Ok(())
        })
        .map(|_| ())
    })
    .unwrap();
    // Two speakers confirmed as the same person may be grouped.
    assert_eq!(merge(&lib, 1, &[2, 3, 4]).unwrap().group.kept, sid(2));
    // Naming through member S4 renames the group (kept S2) and its other
    // confirmed member; S3, which rejected Cai, is separated untouched.
    let out = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), sid(4), CAI, Vec::new(), MODEL)
    })
    .unwrap();
    assert_eq!(out.speaker_id, sid(2));
    assert_eq!(out.redirected_from, Some(sid(4)));
    assert_eq!(out.members_renamed, vec![sid(1)]);
    assert_eq!(out.separated.len(), 1);
    assert_eq!(out.separated[0].speaker_id, sid(3));
    assert_eq!(out.separated[0].reason, SeparationReason::Rejected);
    let m = lib.load_meeting(&mid(MA)).unwrap();
    for n in [1, 2] {
        let s = m.speaker(sid(n)).unwrap();
        assert_eq!(
            (s.assignment, s.person_id.clone()),
            (Assignment::Confirmed, Some(pid(CAI)))
        );
    }
    let s3 = m.speaker(sid(3)).unwrap();
    assert_eq!(s3.assignment, Assignment::None);
    assert_eq!(s3.rejected_person_ids, vec![pid(CAI)]);
    let s4 = m.speaker(sid(4)).unwrap();
    assert_eq!(s4.assignment, Assignment::None, "the member was not named");
    assert_eq!(m.speaker_groups.len(), 1);
    assert_eq!(m.speaker_groups[0].members, vec![sid(1), sid(4)]);
    assert!(m.not_same.contains(&[sid(2), sid(3)]));
    assert!(check_overlay(&m, None).is_ok());

    // Rejecting, on the group, the person a member is confirmed as is refused.
    let err = with_index(&lib, |tx, idx| {
        reject_suggestion(tx, idx, &mid(MA), sid(4), Some(CAI), MODEL, None)
    })
    .unwrap_err();
    assert_eq!(err.code(), "invalid_input");
    // A rejection through a member applies to the kept speaker.
    let out = with_index(&lib, |tx, idx| {
        reject_suggestion(tx, idx, &mid(MA), sid(4), Some(ANA), MODEL, None)
    })
    .unwrap();
    assert_eq!(
        (out.speaker_id, out.redirected_from),
        (sid(2), Some(sid(4)))
    );
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(
        m.speaker(sid(2)).unwrap().rejected_person_ids,
        vec![pid(ANA)]
    );
    assert!(m.speaker(sid(4)).unwrap().rejected_person_ids.is_empty());
}

#[test]
fn renaming_a_group_separates_caption_proven_members_of_another_person() {
    // S1 and S2 named Ana by the captions; S3 confirmed Ana.
    let m = meeting(
        MA,
        10,
        vec![
            named(spk(1, Some(basis(0))), Assignment::Auto, ANA),
            named(spk(2, Some(basis(0))), Assignment::Auto, ANA),
            named(spk(3, Some(basis(0))), Assignment::Confirmed, ANA),
        ],
    );
    let prints = vec![caption_print(&m, ANA, 1), caption_print(&m, ANA, 2)];
    let (_tmp, lib) = setup(people(), vec![m]);
    lib.write(|tx| tx.upsert_voiceprints(&pid(ANA), prints.clone()))
        .unwrap();
    let caption_samples = |lib: &Library| -> Vec<Voiceprint> {
        lib.load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .into_iter()
            .filter(|v| v.source.kind == VoiceprintKind::Captions)
            .collect()
    };

    // Two caption-proven Ana speakers renamed Bia: the kept one is
    // confirmed Bia, the other separated with its caption name intact.
    merge(&lib, 1, &[2]).unwrap();
    let kept = lib.load_meeting(&mid(MA)).unwrap().speaker_groups[0].kept;
    let other = if kept == sid(1) { sid(2) } else { sid(1) };
    let out = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), other, BIA, Vec::new(), MODEL)
    })
    .unwrap();
    assert_eq!(out.speaker_id, kept);
    assert_eq!(out.separated.len(), 1);
    assert_eq!(out.separated[0].reason, SeparationReason::Captions);
    assert_eq!(out.separated[0].person_id, pid(ANA));
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(m.speaker_groups.is_empty());
    let s = m.speaker(other).unwrap();
    assert_eq!(
        (s.assignment, s.person_id.clone()),
        (Assignment::Auto, Some(pid(ANA)))
    );
    let k = m.speaker(kept).unwrap();
    assert_eq!(
        (k.assignment, k.person_id.clone()),
        (Assignment::Confirmed, Some(pid(BIA)))
    );
    // The separated member's caption sample is untouched and still its own.
    let left = caption_samples(&lib);
    assert!(
        left.contains(
            prints
                .iter()
                .find(|p| p.source.speaker_id == Some(other))
                .unwrap()
        )
    );
    assert!(left.iter().all(|p| p.source.speaker_id != Some(kept)));
    assert!(check_overlay(&m, Some(&caption_proven(&lib, &m).unwrap())).is_ok());

    // Mixed group (confirmed Ana kept + caption-proven Ana member) renamed
    // Cai: the confirmed member follows, the caption-proven one is separated.
    with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), kept, ANA, Vec::new(), MODEL)
    })
    .unwrap();
    let fence = fence(&lib);
    with_index(&lib, |tx, idx| {
        group_speakers(
            tx,
            idx,
            &mid(MA),
            sid(3),
            &[kept, other],
            GroupOrigin::User,
            &fence,
            MODEL,
            Some(&thresholds()),
        )
    })
    .unwrap();
    let out = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), sid(3), CAI, Vec::new(), MODEL)
    })
    .unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    let g = &m.speaker_groups[0];
    let confirmed_member = if g.kept == sid(3) { kept } else { sid(3) };
    assert!(g.contains(sid(3)) && g.contains(kept));
    assert!(!g.contains(other));
    assert_eq!(out.separated[0].speaker_id, other);
    assert!(out.members_renamed.contains(&confirmed_member) || out.speaker_id == confirmed_member);
    for n in [sid(3), kept] {
        assert_eq!(m.speaker(n).unwrap().person_id, Some(pid(CAI)));
    }
    assert_eq!(m.speaker(other).unwrap().person_id, Some(pid(ANA)));
}

#[test]
fn grouped_members_are_frozen_for_every_automatic_identity_producer() {
    // S1 named Bia by the captions, S2 confirmed Bia, S3 unnamed.
    let m = meeting(
        MA,
        10,
        vec![
            named(spk(1, Some(basis(2))), Assignment::Auto, BIA),
            named(spk(2, Some(basis(0))), Assignment::Confirmed, BIA),
            spk(3, Some(basis(3))),
        ],
    );
    let caption = caption_print(&m, BIA, 1);
    let (_tmp, lib) = setup(people(), vec![m]);
    lib.write(|tx| tx.upsert_voiceprints(&pid(BIA), vec![caption]))
        .unwrap();
    let before = lib.load_meeting(&mid(MA)).unwrap();
    let prints = voiceprint_bytes(&lib, BIA);

    // Group → ungroup a caption-named member: byte-identical identity.
    assert_eq!(merge(&lib, 1, &[2, 3]).unwrap().group.kept, sid(2));
    unmerge(&lib, "G2").unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(m.speakers, before.speakers);
    assert_eq!(voiceprint_bytes(&lib, BIA), prints);

    // Re-resolution (what identity reevaluation runs) changes no grouped
    // member, even one a new confirmed sample now matches.
    merge(&lib, 2, &[1, 3]).unwrap();
    lib.write(|tx| {
        tx.upsert_voiceprints(&pid(CAI), vec![confirmed_print(&pid(CAI), &basis(3), 0.0)])
    })
    .unwrap();
    let changes = with_index(&lib, |tx, idx| {
        re_resolve_all(tx, idx, MODEL, Some(&thresholds()))
    })
    .unwrap();
    assert!(changes.is_empty(), "{changes:?}");
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(m.speakers, before.speakers);
    assert_eq!(voiceprint_bytes(&lib, BIA), prints);
    // Ungrouped, the same re-resolution names S3 Cai at once.
    unmerge(&lib, "G2").unwrap();
    let changes = with_index(&lib, |tx, idx| {
        re_resolve_all(tx, idx, MODEL, Some(&thresholds()))
    })
    .unwrap();
    assert!(
        changes.iter().any(|c| c.speaker_id == sid(3)),
        "{changes:?}"
    );
}

#[test]
fn automatic_grouping_suggestions_and_separations() {
    // S1/S2 one voice (≥ auto_merge); S3 tilted (suggested only); S4 other.
    let m = meeting(
        MA,
        10,
        vec![
            spk(1, Some(basis(0))),
            spk(2, Some(basis(0))),
            spk(3, Some(tilted())),
            spk(4, Some(basis(5))),
        ],
    );
    let (_tmp, lib) = setup(people(), vec![m]);
    let run = |t: &Thresholds| {
        with_index(&lib, |tx, idx| {
            auto_group_meeting(tx, idx, &mid(MA), MODEL, Some(t))
        })
        .unwrap()
    };
    // No auto_merge: nothing is grouped; suggestions need merge.
    let only_merge = Thresholds {
        merge: Some(0.5),
        ..thresholds()
    };
    assert_eq!(run(&only_merge).groups_added, 0);
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(
        duplicate_suggestions(&m, MODEL, Some(&thresholds()), &BTreeMap::new())
            .unwrap()
            .is_empty()
    );

    let out = run(&auto_thresholds());
    assert_eq!(
        (out.groups_added, out.speakers_before, out.speakers_after),
        (1, 4, 3)
    );
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(m.speaker_groups.len(), 1);
    assert!(matches!(
        m.speaker_groups[0].origin,
        GroupOrigin::Auto { .. }
    ));
    // Idempotent.
    let again = run(&auto_thresholds());
    assert_eq!((again.groups_added, again.speakers_after), (0, 3));
    // Suggestions: the group and S3 in [merge, auto_merge); never S4.
    let kept = m.speaker_groups[0].kept;
    let suggestions =
        duplicate_suggestions(&m, MODEL, Some(&auto_thresholds()), &BTreeMap::new()).unwrap();
    assert_eq!(suggestions.len(), 1, "{suggestions:?}");
    assert_eq!(
        (suggestions[0].a, suggestions[0].b),
        (sid(3), kept),
        "S3 talks most"
    );
    assert!(suggestions[0].score >= 0.5 && suggestions[0].score < 0.9);

    // Not the same person: no suggestion, no automatic grouping.
    let fence = fence(&lib);
    with_index(&lib, |tx, idx| {
        mark_not_same(tx, idx, &mid(MA), sid(3), kept, &fence)
    })
    .unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(
        duplicate_suggestions(&m, MODEL, Some(&auto_thresholds()), &BTreeMap::new())
            .unwrap()
            .is_empty()
    );
    let low = Thresholds {
        merge: Some(0.5),
        auto_merge: Some(0.5),
        ..thresholds()
    };
    assert_eq!(
        run(&low).groups_added,
        0,
        "not_same blocks automatic grouping"
    );
    // Unmerging an Auto group is remembered: a new run adds nothing.
    let id = m.speaker_groups[0].id.clone();
    unmerge(&lib, &id).unwrap();
    assert_eq!(run(&auto_thresholds()).groups_added, 0);
    assert!(
        lib.load_meeting(&mid(MA))
            .unwrap()
            .speaker_groups
            .is_empty()
    );
}

#[test]
fn forgetting_a_person_leaves_nothing_in_the_overlay_and_unmerge_restores_nothing() {
    let m = meeting(
        MA,
        10,
        vec![
            named(spk(1, Some(basis(0))), Assignment::Confirmed, BIA),
            named(spk(2, Some(basis(0))), Assignment::Auto, BIA),
        ],
    );
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(BIA), "Bia Example"), vec![basis(0)])],
        vec![m],
    );
    merge(&lib, 1, &[2]).unwrap();
    with_index(&lib, |tx, idx| forget_person(tx, idx, BIA)).unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    let overlay = serde_json::to_string(&(&m.speaker_groups, &m.not_same)).unwrap();
    assert!(!overlay.contains(BIA), "{overlay}");
    unmerge(&lib, "G1").unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(m.speakers.iter().all(|s| s.person_id.is_none()));
    assert!(!lib.person_dir(&pid(BIA)).exists());
}

/// S2 auto-named Bia (with its weak sample), S9 rejected Bia: grouped by a
/// user merge or by `merge --auto`, the group is not shown as Bia and Bia
/// keeps no weak sample from it; re-resolution keeps it that way.
#[test]
fn a_kept_speaker_is_never_named_as_someone_a_member_rejected() {
    for automatic in [false, true] {
        let mut kept = named(spk(2, Some(basis(0))), Assignment::Auto, BIA);
        kept.talk_s = 100.0;
        let mut member = spk(9, Some(basis(0)));
        member.rejected_person_ids = vec![pid(BIA)];
        let m = meeting(MA, 10, vec![kept, member]);
        let sample = weak_sample(&m, sid(2), &pid(BIA)).unwrap().unwrap();
        let (_tmp, lib) = setup(
            vec![(Person::new(pid(BIA), "Bia Example"), vec![basis(0)])],
            vec![m],
        );
        lib.write(|tx| tx.upsert_voiceprints(&pid(BIA), vec![sample]))
            .unwrap();
        let weak = |lib: &Library| {
            lib.load_voiceprints(&pid(BIA))
                .unwrap()
                .embeddings
                .iter()
                .filter(|v| v.strength == Strength::Weak)
                .count()
        };
        assert_eq!(weak(&lib), 1);
        if automatic {
            let out = with_index(&lib, |tx, idx| {
                auto_group_meeting(tx, idx, &mid(MA), MODEL, Some(&auto_thresholds()))
            })
            .unwrap();
            assert_eq!(out.groups_added, 1);
        } else {
            assert_eq!(merge(&lib, 2, &[9]).unwrap().group.kept, sid(2));
        }
        let check = |lib: &Library| {
            let m = lib.load_meeting(&mid(MA)).unwrap();
            assert_eq!(m.speaker_groups[0].kept, sid(2));
            let s = m.speaker(sid(2)).unwrap();
            assert_ne!(SpeakerState::of(s).person_id, Some(pid(BIA)), "{s:?}");
            assert!(!md(lib, MA).contains("Bia Example"));
            assert_eq!(weak(lib), 0, "no weak Bia sample from the group");
            assert_eq!(effective_rejections(&m, sid(2)), vec![pid(BIA)]);
            // The member's own rejections stay its own.
            assert!(s.rejected_person_ids.is_empty());
        };
        check(&lib);
        with_index(&lib, |tx, idx| {
            re_resolve_all(tx, idx, MODEL, Some(&auto_thresholds()))
        })
        .unwrap();
        check(&lib);
    }
}

#[test]
fn a_reversed_hand_edited_not_same_pair_still_separates_its_speakers() {
    let mut m = meeting(MA, 10, vec![spk(2, None), spk(9, None)]);
    let (_tmp, lib) = setup(people(), vec![m.clone()]);
    m.speaker_groups = vec![SpeakerGroup::new(sid(2), [sid(9)], GroupOrigin::User)];
    m.not_same = vec![[sid(9), sid(2)]];
    let path = lib.meeting_dir(&mid(MA)).unwrap().join(paths::MEETING_JSON);
    fs::write(&path, serde_json::to_vec(&m).unwrap()).unwrap();
    let loaded = lib.load_meeting(&mid(MA)).unwrap();
    assert!(
        loaded.speaker_groups.is_empty(),
        "{:?}",
        loaded.speaker_groups
    );
    assert_eq!(loaded.not_same, vec![[sid(2), sid(9)]]);
    assert!(
        loaded
            .review_notes
            .iter()
            .any(|n| n.source == schema::ReviewSource::Grouping)
    );
    lib.write(|tx| tx.save_meeting(&loaded)).unwrap();
}

/// S1 (most talk) and S2, one voice; S2 rejected Ana.
fn rejecting_member_meeting() -> Meeting {
    let mut s1 = spk(1, Some(basis(0)));
    s1.talk_s = 100.0;
    let mut s2 = spk(2, Some(basis(0)));
    s2.rejected_person_ids = vec![pid(ANA)];
    meeting(MA, 10, vec![s1, s2])
}

#[test]
fn shown_voice_ranking_honours_every_members_rejection() {
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), vec![basis(0)]),
            (Person::new(pid(BIA), "Bia Example"), vec![tilted()]),
        ],
        vec![rejecting_member_meeting()],
    );
    let people = load_person_samples(&lib, MODEL).unwrap();
    let alone = lib.load_meeting(&mid(MA)).unwrap();
    let top = voice_matches(&people, &alone, sid(1), None, None, MODEL, 3).unwrap();
    assert_eq!(top[0].person_id, pid(ANA), "ungrouped: S1 rejected no one");

    merge(&lib, 1, &[2]).unwrap();
    let grouped = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(effective_speaker(&grouped, sid(2)), sid(1));
    for asked in [sid(1), sid(2)] {
        let matches = voice_matches(&people, &grouped, asked, None, None, MODEL, 3).unwrap();
        assert!(matches.iter().all(|v| v.person_id != pid(ANA)), "{asked}");
        assert_eq!(matches[0].person_id, pid(BIA));
        // The ranking shown is the one resolution picks from.
        let res = identity::resolve(
            &basis(0),
            &people.for_meeting(&grouped.id),
            &effective_rejections(&grouped, sid(1)),
            None,
            None,
        );
        assert_eq!(res.person_id.as_ref(), Some(&matches[0].person_id));
    }
    assert!(voice_matches(&people, &grouped, sid(9), None, None, MODEL, 3).is_err());
}

#[test]
fn a_separated_member_is_not_same_as_every_remaining_member() {
    // S1 (most talk) sounds a little different; S2 (rejected Ana) and S3
    // are the closest pair.
    let mut s1 = spk(1, Some(tilted()));
    s1.talk_s = 100.0;
    let mut s2 = spk(2, Some(basis(0)));
    s2.rejected_person_ids = vec![pid(ANA)];
    let m = meeting(MA, 10, vec![s1, s2, spk(3, Some(basis(0)))]);
    let (_tmp, lib) = setup(people(), vec![m]);
    let group = merge(&lib, 1, &[2, 3]).unwrap().group;
    assert_eq!(group.kept, sid(1));
    let out = with_index(&lib, |tx, idx| {
        name_speaker(tx, idx, &mid(MA), sid(1), ANA, Vec::new(), MODEL)
    })
    .unwrap();
    assert_eq!(
        out.separated
            .iter()
            .map(|s| s.speaker_id)
            .collect::<Vec<_>>(),
        vec![sid(2)]
    );
    let after = lib.load_meeting(&mid(MA)).unwrap();
    // Remembered apart from the kept speaker and from S3, which stays.
    assert!(after.not_same.contains(&[sid(1), sid(2)]));
    assert!(after.not_same.contains(&[sid(2), sid(3)]));
    assert_eq!(after.speaker_groups.len(), 1);
    assert_eq!(after.speaker_groups[0].members, vec![sid(3)]);
    // Automatic grouping, even with the group gone, never puts S2 back
    // with either of them (S2–S3 is the most similar pair).
    let mut loose = after.clone();
    loose.speaker_groups.clear();
    let regrouped =
        identity::auto_groups(&loose, MODEL, Some(0.5), None, &BTreeMap::new()).unwrap();
    assert!(
        regrouped.iter().all(|g| !g.contains(sid(2))),
        "{regrouped:?}"
    );
}

#[test]
fn the_roster_floor_counts_groups_already_shown_on_other_tracks() {
    let on = |n: u32, track: &str, v: Vec<f32>| {
        let mut s = spk(n, Some(v));
        s.track = track.into();
        s
    };
    let mut m = meeting(
        MA,
        10,
        vec![
            on(1, "t0", basis(0)),
            on(2, "t0", basis(0)),
            on(5, "t1", basis(1)),
            on(6, "t1", basis(2)),
            on(7, "t1", basis(3)),
        ],
    );
    // Three raw speakers on t1, shown as one.
    m.speaker_groups = vec![SpeakerGroup::new(
        sid(5),
        [sid(6), sid(7)],
        GroupOrigin::User,
    )];
    let none = BTreeMap::new();
    // Roster of three: one shown on t1, so t0 keeps two voices.
    let groups = identity::auto_groups(&m, MODEL, Some(0.9), Some(3), &none).unwrap();
    assert_eq!(
        groups, m.speaker_groups,
        "t0 must not merge below the roster"
    );
    // Roster of two: t0 may become one voice.
    let groups = identity::auto_groups(&m, MODEL, Some(0.9), Some(2), &none).unwrap();
    assert_eq!(groups.len(), 2);
    assert!(
        groups
            .iter()
            .any(|g| g.contains(sid(1)) && g.contains(sid(2)))
    );
}
