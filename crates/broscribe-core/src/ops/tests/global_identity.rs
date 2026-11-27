use super::*;

const MD: &str = "m-0000d4";
const CAI: &str = "p-c3c3c3";
const DAN: &str = "p-d4d4d4";

fn named(speaker: u32, person: &str) -> Speaker {
    let mut s = spk(speaker, None);
    s.assignment = Assignment::Confirmed;
    s.person_id = Some(pid(person));
    s
}

fn groups(lib: &Library) -> Vec<IdentityGroup> {
    identity_groups(lib, &review_queue(lib, None).unwrap(), MODEL, None).unwrap()
}

#[test]
fn batches_rank_by_distinct_meetings_and_include_only_selected_occurrences() {
    let mut a = meeting(
        MA,
        1,
        vec![
            spk(1, Some(basis(0))),
            spk(2, Some(basis(0))),
            spk(9, Some(basis(1))),
        ],
    );
    a.speakers[2].talk_s = 50.0;
    let b = meeting(MB, 2, vec![spk(1, Some(basis(0)))]);
    let c = meeting(MC, 3, vec![spk(1, Some(basis(0)))]);
    let d = meeting(MD, 4, vec![named(1, BIA)]);
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(BIA), "Bia Example"), Vec::new())],
        vec![a, b, c, d],
    );
    let rows = groups(&lib);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].meeting_count, 3);
    assert_eq!(rows[0].occurrences.len(), 4);
    assert_eq!(rows[1].occurrences[0].speaker_id, sid(9));
    let batch = IdentifyBatch {
        person: "Ana Example".into(),
        occurrences: rows[0]
            .occurrences
            .iter()
            .filter(|o| o.meeting_id != mid(MC))
            .cloned()
            .collect(),
    };
    let out = with_index(&lib, |tx, index| {
        identify_batch(tx, index, &batch, MODEL, None)
    })
    .unwrap();
    assert_eq!(out.meeting_count, 2);
    assert_eq!(out.named.len(), 3);
    for o in &batch.occurrences {
        let m = lib.load_meeting(&o.meeting_id).unwrap();
        assert_eq!(
            m.speaker(o.speaker_id).unwrap().named_person(),
            Some(&out.person_id)
        );
    }
    assert_eq!(
        lib.load_meeting(&mid(MC)).unwrap().speakers[0].assignment,
        Assignment::None
    );
    assert_eq!(
        lib.load_meeting(&mid(MD)).unwrap().speakers[0].person_id,
        Some(pid(BIA))
    );
    assert!(
        with_index(&lib, |tx, index| identify_batch(
            tx, index, &batch, MODEL, None
        ))
        .is_err()
    );
}

#[test]
fn batch_checks_current_decisions_and_the_selected_profile_before_writing() {
    let a = meeting(MA, 1, vec![spk(1, Some(basis(0)))]);
    let b = meeting(MB, 2, vec![spk(1, Some(basis(0)))]);
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(ANA), "Ana Example"), Vec::new())],
        vec![a, b],
    );
    let batch = IdentifyBatch {
        person: ANA.into(),
        occurrences: groups(&lib)[0].occurrences.clone(),
    };
    let mut b = lib.load_meeting(&mid(MB)).unwrap();
    b.speakers[0].rejected_person_ids.push(pid(ANA));
    lib.write(|tx| tx.save_meeting(&b)).unwrap();
    for shown in [
        batch,
        IdentifyBatch {
            person: ANA.into(),
            occurrences: groups(&lib)[0].occurrences.clone(),
        },
    ] {
        assert!(
            with_index(&lib, |tx, index| identify_batch(
                tx, index, &shown, MODEL, None
            ))
            .is_err()
        );
        assert_eq!(
            lib.load_meeting(&mid(MA)).unwrap().speakers[0].assignment,
            Assignment::None
        );
    }
}

#[test]
fn batch_skips_the_occurrence_with_a_different_saved_line_name() {
    let a = meeting(MA, 1, vec![spk(1, Some(basis(0)))]);
    let mut b = meeting(MB, 2, vec![spk(1, Some(basis(0)))]);
    let line = segment_fingerprint(&b.segments[0]);
    b.utterance_people.push(schema::UtterancePerson {
        track: line.track,
        start: line.start,
        end: line.end,
        text_sha256: line.text_sha256,
        person_id: pid(BIA),
        at: schema::now(),
        live_decision: None,
    });
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), Vec::new()),
            (Person::new(pid(BIA), "Bia Example"), Vec::new()),
        ],
        vec![a, b],
    );
    let batch = IdentifyBatch {
        person: ANA.into(),
        occurrences: groups(&lib)[0].occurrences.clone(),
    };
    assert_eq!(batch.occurrences.len(), 2);
    let out = with_index(&lib, |tx, index| {
        identify_batch(tx, index, &batch, MODEL, None)
    })
    .unwrap();
    assert_eq!(out.meeting_count, 1);
    assert_eq!(out.skipped_conflicts.len(), 1);
    assert_eq!(out.skipped_conflicts[0].meeting_id, mid(MB));
    let b = lib.load_meeting(&mid(MB)).unwrap();
    assert_eq!(b.speakers[0].assignment, Assignment::None);
    assert!(
        b.review_notes
            .iter()
            .any(|note| note.text.contains("Ana Example") && note.text.contains("Bia Example"))
    );
    assert!(
        !lib.load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .iter()
            .any(|p| p.source.meeting_id.as_ref() == Some(&mid(MB)))
    );
}

#[test]
fn duplicates_rank_by_score_then_distinct_meetings_and_exclusions_survive_rebuild() {
    let voice = |x: f32| vec![x, (1.0 - x * x).sqrt()];
    let a = meeting(MA, 1, vec![named(1, ANA), named(2, BIA)]);
    let b = meeting(MB, 2, vec![named(1, ANA)]);
    let c = meeting(MC, 3, vec![named(1, CAI)]);
    let d = meeting(MD, 4, vec![named(1, DAN)]);
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), vec![voice(1.0)]),
            (Person::new(pid(BIA), "Bia Example"), vec![voice(1.0)]),
            (Person::new(pid(CAI), "Cai Example"), vec![voice(1.0)]),
            (Person::new(pid(DAN), "Dan Example"), vec![voice(0.95)]),
        ],
        vec![a, b, c, d],
    );
    let pairs = person_duplicates(&lib, MODEL, None).unwrap();
    assert_eq!(pairs[0].pair, PersonPair::new(pid(ANA), pid(CAI)));
    assert_eq!(pairs[0].meeting_count, 3);
    assert_eq!(pairs[1].pair, PersonPair::new(pid(ANA), pid(BIA)));
    assert_eq!(pairs[1].meeting_count, 2);
    assert_eq!(pairs[2].score, 1.0);
    let refused = PersonPair(pid(CAI), pid(ANA));
    lib.write(|tx| dismiss_person_duplicate(tx, &refused))
        .unwrap();
    lib.write(|tx| Index::rebuild(tx).map(|_| ())).unwrap();
    assert_eq!(
        person_duplicates(&lib, MODEL, None).unwrap()[0].pair,
        pairs[1].pair
    );
}

#[test]
fn merge_preflights_enrollment_and_preserves_attributions_notes_voice_and_exclusions() {
    let a = meeting(MA, 1, vec![named(1, ANA)]);
    let mut b = meeting(MB, 2, vec![named(1, BIA)]);
    let line = segment_fingerprint(&b.segments[0]);
    b.utterance_people.push(schema::UtterancePerson {
        track: line.track,
        start: line.start,
        end: line.end,
        text_sha256: line.text_sha256,
        person_id: pid(BIA),
        at: schema::now(),
        live_decision: None,
    });
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), vec![basis(0)]),
            (Person::new(pid(BIA), "Bia Example"), vec![basis(0)]),
            (Person::new(pid(CAI), "Cai Example"), vec![basis(0)]),
        ],
        vec![a, b],
    );
    lib.write(|tx| {
        let mut source = lib.load_person(&pid(BIA))?;
        source.emails.push("bia@example.test".into());
        tx.save_person_with_notes(&source, "Source notes")?;
        tx.save_person_with_notes(&lib.load_person(&pid(ANA))?, "Target notes")?;
        dismiss_person_duplicate(tx, &PersonPair(pid(BIA), pid(CAI)))
    })
    .unwrap();
    let from = lib.person_dir(&pid(BIA)).join("enrollment");
    let to = lib.person_dir(&pid(ANA)).join("enrollment");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(&to).unwrap();
    fs::write(from.join("sample.wav"), b"synthetic").unwrap();
    fs::write(from.join("another.wav"), b"another synthetic sample").unwrap();
    fs::write(to.join("sample.wav"), b"conflict").unwrap();
    let row = person_duplicates(&lib, MODEL, None).unwrap().remove(0);
    assert_eq!(row.pair, PersonPair(pid(ANA), pid(BIA)));
    let request = MergePeople {
        keep: pid(ANA),
        source: pid(BIA),
        revision: row.revision,
    };
    assert!(
        with_index(&lib, |tx, index| merge_people(
            tx, index, &request, MODEL, None
        ))
        .is_err()
    );
    assert!(pending_person_merge(&lib).unwrap().is_none());
    assert!(!to.join("another.wav").exists());
    assert_eq!(fs::read(to.join("sample.wav")).unwrap(), b"conflict");
    assert!(
        crate::queue::Queue::open(lib.root())
            .unwrap()
            .snapshot()
            .unwrap()
            .jobs
            .is_empty()
    );
    assert_eq!(
        lib.load_meeting(&mid(MB)).unwrap().speakers[0].person_id,
        Some(pid(BIA))
    );
    fs::remove_file(to.join("sample.wav")).unwrap();
    let out = with_index(&lib, |tx, index| {
        merge_people(tx, index, &request, MODEL, None)
    })
    .unwrap();
    assert_eq!(out.meetings_changed, 1);
    assert!(pending_person_merge(&lib).unwrap().is_none());
    assert!(lib.load_person(&pid(BIA)).is_err());
    let (keep, notes) = lib.load_person_with_notes(&pid(ANA)).unwrap();
    assert_eq!(keep.aliases, ["Bia Example"]);
    assert_eq!(keep.emails, ["bia@example.test"]);
    assert!(notes.contains("Target notes") && notes.contains("Source notes"));
    assert_eq!(lib.load_voiceprints(&pid(ANA)).unwrap().embeddings.len(), 2);
    assert_eq!(fs::read(to.join("sample.wav")).unwrap(), b"synthetic");
    let b = lib.load_meeting(&mid(MB)).unwrap();
    assert_eq!(b.speakers[0].person_id, Some(pid(ANA)));
    assert_eq!(b.utterance_people[0].person_id, pid(ANA));
    assert!(md(&lib, MB).contains("Ana Example"));
    assert!(person_duplicates(&lib, MODEL, None).unwrap().is_empty());
}

#[test]
fn interrupted_merge_cleanup_refuses_new_source_edits_and_can_resume() {
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), vec![]),
            (Person::new(pid(BIA), "Bia Example"), vec![]),
        ],
        vec![meeting(MA, 1, vec![spk(1, None)])],
    );
    let spec = PersonEditSpec {
        notes: Some("Copied notes".into()),
        ..Default::default()
    };
    let edit = with_index(&lib, |tx, index| edit_person(tx, index, &pid(BIA), &spec)).unwrap();
    let request = MergePeople {
        keep: pid(ANA),
        source: pid(BIA),
        revision: "already reviewed".into(),
    };
    let marker = lib
        .root()
        .join(paths::PEOPLE_DIR)
        .join(".merge-pending.json");
    lib.write(|tx| {
        tx.save_person_with_notes(&lib.load_person(&request.keep)?, "Copied notes")?;
        fsutil::write_json_atomic(
            &marker,
            &serde_json::json!({ "request": request, "affected": [], "cleanup": true }),
        )
    })
    .unwrap();
    let late = PersonEditSpec {
        notes: Some("Later notes".into()),
        ..Default::default()
    };
    let undo_path = lib.person_dir(&request.source).join(PERSON_UNDO_FILE);
    let undo_before = fs::read(&undo_path).unwrap();
    let refused = with_index(&lib, |tx, index| {
        edit_person(tx, index, &request.source, &late)
    });
    assert_eq!(refused.unwrap_err().code(), "library_busy");
    let undo = with_index(&lib, |tx, index| {
        undo_person_edit(tx, index, &request.source, edit.undo_id.as_deref().unwrap())
    });
    assert_eq!(undo.unwrap_err().code(), "library_busy");
    let voice_write =
        lib.write(|tx| tx.save_voiceprints(&Voiceprints::empty(request.source.clone())));
    assert_eq!(voice_write.unwrap_err().code(), "library_busy");
    let name = with_index(&lib, |tx, index| {
        name_speaker(tx, index, &mid(MA), sid(1), BIA, Vec::new(), MODEL)
    });
    assert_eq!(name.unwrap_err().code(), "library_busy");
    assert_eq!(
        attribute(&lib, &[0], Some(BIA)).unwrap_err().code(),
        "library_busy"
    );

    let participant = lib.write(|tx| {
        tx.update_meeting(&mid(MA), |meeting| {
            meeting.participants.push(schema::Participant {
                person_id: Some(pid(BIA)),
                ..Default::default()
            });
            Ok(())
        })
    });
    assert_eq!(participant.unwrap_err().code(), "library_busy");
    let meeting = lib.load_meeting(&mid(MA)).unwrap();
    assert!(!meeting_references_person(&meeting, &pid(BIA)));

    assert_eq!(fs::read(&undo_path).unwrap(), undo_before);
    assert_eq!(
        person_state(&lib, &request.source).unwrap().notes,
        "Copied notes"
    );
    with_index(&lib, |tx, index| {
        merge_people(tx, index, &request, MODEL, None)
    })
    .unwrap();
    assert!(pending_person_merge(&lib).unwrap().is_none());
    assert!(!lib.person_dir(&request.source).exists());
    assert_eq!(
        person_state(&lib, &request.keep).unwrap().notes,
        "Copied notes"
    );
}

#[test]
fn manually_added_and_edited_people_return_their_persisted_hue_and_revision() {
    let (_tmp, lib) = setup(vec![], vec![]);
    let person = with_index(&lib, |tx, index| {
        add_person(
            tx,
            index,
            &NewPersonSpec {
                name: "Ana Example".into(),
                emails: vec![],
                notes: "Seed notes".into(),
            },
        )
    })
    .unwrap();
    assert!(person.hue.is_some());
    assert_eq!(person, lib.load_person(&person.id).unwrap());

    let mut legacy = person;
    legacy.hue = None;
    fs::write(
        lib.person_dir(&legacy.id).join(paths::PERSON_MD),
        crate::render::render_person_md(&legacy, "Seed notes").unwrap(),
    )
    .unwrap();
    let spec = PersonEditSpec {
        notes: Some("Edited notes".into()),
        ..Default::default()
    };
    let edited = with_index(&lib, |tx, index| edit_person(tx, index, &legacy.id, &spec)).unwrap();
    let persisted = person_state(&lib, &legacy.id).unwrap();
    assert_eq!(persisted.person.hue, Some(legacy.hue()));
    assert_eq!(edited.person, persisted.person);
    assert_eq!(edited.revision, persisted.revision);
}

#[test]
fn invalid_person_md_hues_fall_back_to_the_derived_hue() {
    let person = Person::new(pid(ANA), "Ana Example");
    let md = crate::render::render_person_md(&person, "Saved notes").unwrap();
    for (yaml, expected) in [
        ("300", None),
        ("-1", None),
        ("0", None),
        ("8", None),
        ("\"red\"", None),
        ("\"3\"", None),
        ("[1, 2]", None),
        ("{red: 1}", None),
        ("null", None),
        ("3", Some(3)),
    ] {
        let md = md.replacen("---\n", &format!("---\nhue: {yaml}\n"), 1);
        let (decoded, notes) = crate::render::parse_person_md(&md).unwrap();
        assert_eq!(decoded.hue, expected, "hue: {yaml}");
        assert_eq!(decoded.hue(), expected.unwrap_or_else(|| person.hue()));
        assert_eq!(decoded.name, person.name);
        assert_eq!(notes, "Saved notes");
    }
}

#[test]
fn applied_review_edit_succeeds_when_only_the_undo_receipt_fails() {
    let (_tmp, lib) = setup(vec![(Person::new(pid(ANA), "Ana Example"), vec![])], vec![]);
    let (result, edit) = with_index(&lib, |tx, index| {
        record_review_edit(tx, index, MODEL, |tx, index| {
            let mut person = lib.load_person(&pid(ANA))?;
            person.aliases.push("A. Example".into());
            tx.save_person(&person)?;
            index.upsert_person(tx, &person)?;
            // A directory at the fence path makes the receipt's final read fail.
            fs::create_dir_all(tombstone_path(lib.root(), ANA)).unwrap();
            Ok(person)
        })
    })
    .unwrap();
    assert_eq!(result.aliases, ["A. Example"]);
    assert_eq!(lib.load_person(&pid(ANA)).unwrap(), result);
    assert!(edit.is_none());
}

#[test]
fn identification_history_restores_assignments_and_learning_as_one_edit() {
    let a = meeting(MA, 1, vec![spk(1, Some(basis(0)))]);
    let b = meeting(MB, 2, vec![spk(1, Some(basis(0)))]);
    let (_tmp, lib) = setup(
        vec![(Person::new(pid(ANA), "Ana Example"), Vec::new())],
        vec![a, b],
    );
    let batch = IdentifyBatch {
        person: ANA.into(),
        occurrences: groups(&lib)[0].occurrences.clone(),
    };
    let (_, edit) = with_index(&lib, |tx, index| {
        record_review_edit(tx, index, MODEL, |tx, index| {
            identify_batch(tx, index, &batch, MODEL, None)
        })
    })
    .unwrap();
    let mut edit = edit.unwrap();
    let mut a = lib.load_meeting(&mid(MA)).unwrap();
    let mut later = new_voiceprint(
        &pid(ANA),
        MODEL,
        &a,
        &basis(0),
        0.2,
        1.5,
        VoiceprintKind::Confirmation,
        Strength::Confirmed,
    );
    later.source.set_speaker(&a, sid(1));
    a.tags.push("reviewed".into());
    lib.write(|tx| {
        tx.save_meeting(&a)?;
        tx.upsert_voiceprints(&pid(ANA), vec![later]).map(|_| ())
    })
    .unwrap();
    let count = lib.load_voiceprints(&pid(ANA)).unwrap().embeddings.len();
    assert_eq!(count, 3);
    edit.replay(&lib, false).unwrap();
    for id in [MA, MB] {
        assert_eq!(
            lib.load_meeting(&mid(id)).unwrap().speakers[0].assignment,
            Assignment::None
        );
    }
    assert_eq!(lib.load_meeting(&mid(MA)).unwrap().tags, ["reviewed"]);
    assert!(
        lib.load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .is_empty()
    );
    edit.replay(&lib, true).unwrap();
    for id in [MA, MB] {
        assert_eq!(
            lib.load_meeting(&mid(id)).unwrap().speakers[0].named_person(),
            Some(&pid(ANA))
        );
    }
    assert_eq!(
        lib.load_voiceprints(&pid(ANA)).unwrap().embeddings.len(),
        count
    );

    with_index(&lib, |tx, index| forget_voice(tx, index, ANA)).unwrap();
    assert_eq!(
        edit.replay(&lib, false).unwrap_err().code(),
        "identity_edit_changed"
    );
    assert!(
        lib.load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .is_empty()
    );
}

#[test]
fn profile_merge_history_restores_the_original_profile_and_enrollment() {
    let a = meeting(MA, 1, vec![named(1, ANA)]);
    let b = meeting(MB, 2, vec![named(1, BIA)]);
    let (_tmp, lib) = setup(
        vec![
            (Person::new(pid(ANA), "Ana Example"), vec![basis(0)]),
            (Person::new(pid(BIA), "Bia Example"), vec![basis(0)]),
        ],
        vec![a, b],
    );
    let source = lib.person_dir(&pid(BIA)).join("enrollment");
    let keep = lib.person_dir(&pid(ANA)).join("enrollment");
    for id in [ANA, BIA] {
        let mut profile = lib.load_person(&pid(id)).unwrap();
        profile.hue = None;
        fs::write(
            lib.person_dir(&profile.id).join(paths::PERSON_MD),
            crate::render::render_person_md(&profile, "").unwrap(),
        )
        .unwrap();
    }
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("sample.wav"), b"synthetic").unwrap();
    let before = lib.load_voiceprints(&pid(BIA)).unwrap();
    let request = MergePeople {
        keep: pid(ANA),
        source: pid(BIA),
        revision: person_duplicates(&lib, MODEL, None)
            .unwrap()
            .remove(0)
            .revision,
    };
    let (_, edit) = with_index(&lib, |tx, index| {
        record_review_edit(tx, index, MODEL, |tx, index| {
            merge_people(tx, index, &request, MODEL, None)
        })
    })
    .unwrap();
    let mut edit = edit.unwrap();
    assert!(lib.load_person(&pid(BIA)).is_err());
    let merged = lib.load_voiceprints(&pid(ANA)).unwrap();

    assert_eq!(
        resolve_merged_person(&lib, &pid(BIA)).unwrap().unwrap().id,
        pid(ANA)
    );
    edit.replay(&lib, false).unwrap();
    assert!(!merge_redirect_path(lib.root(), &pid(BIA)).exists());
    let restored = lib.load_person(&pid(BIA)).unwrap();
    assert_eq!(restored.name, "Bia Example");
    assert_eq!(restored.hue, Some(schema::derived_hue(&restored.id)));
    assert_eq!(
        lib.load_meeting(&mid(MB)).unwrap().speakers[0].named_person(),
        Some(&pid(BIA))
    );
    assert_eq!(lib.load_voiceprints(&pid(BIA)).unwrap(), before);
    assert!(lib.load_person(&pid(ANA)).unwrap().aliases.is_empty());
    assert_eq!(fs::read(source.join("sample.wav")).unwrap(), b"synthetic");
    assert!(!keep.join("sample.wav").exists());

    let mut later = meeting(MC, 3, vec![spk(1, None)]);
    lib.write(|tx| tx.create_meeting_dir(&later).map(|_| ()))
        .unwrap();
    later.speakers[0].identity_conflict = Some(IdentityConflict {
        candidate: Some(pid(BIA)),
        lines: vec![],
        rejected: true,
        unlinked_caption: None,
    });
    lib.write(|tx| tx.save_meeting(&later)).unwrap();
    assert_eq!(
        edit.replay(&lib, true).unwrap_err().code(),
        "identity_edit_changed"
    );
    assert!(lib.load_person(&pid(BIA)).is_ok());

    later.speakers[0].identity_conflict = None;
    later.speakers[0].live_promotion = Some(crate::live_identity::LivePromotion {
        person_id: pid(BIA),
        decision_ids: vec![1, 2],
        line_count: 2,
        required_lines: 2,
        at: schema::now(),
    });
    lib.write(|tx| tx.save_meeting(&later)).unwrap();
    assert_eq!(
        edit.replay(&lib, true).unwrap_err().code(),
        "identity_edit_changed"
    );
    assert!(lib.load_person(&pid(BIA)).is_ok());

    later.speakers[0].live_promotion = None;
    lib.write(|tx| tx.save_meeting(&later)).unwrap();
    edit.replay(&lib, true).unwrap();
    assert!(lib.load_person(&pid(BIA)).is_err());
    assert_eq!(lib.load_voiceprints(&pid(ANA)).unwrap(), merged);
    assert_eq!(fs::read(keep.join("sample.wav")).unwrap(), b"synthetic");
    assert_eq!(
        lib.load_meeting(&mid(MB)).unwrap().speakers[0].named_person(),
        Some(&pid(ANA))
    );
    assert_eq!(
        resolve_merged_person(&lib, &pid(BIA)).unwrap().unwrap().id,
        pid(ANA)
    );

    edit.replay(&lib, false).unwrap();
    with_index(&lib, |tx, index| forget_person(tx, index, BIA)).unwrap();
    assert!(resolve_merged_person(&lib, &pid(BIA)).unwrap().is_none());
}
