//! Ordinary person edits and their Undo (UI31), and the read-only person
//! projection of `people show` / MCP `get_person` (CL17, CL20).

use super::*;
use crate::ops::{
    PersonEditSpec, PersonField, edit_person, person_state, person_view, undo_person_edit,
};

fn ana_named(meeting_id: &str, day: u32) -> Meeting {
    let mut s = spk(1, None);
    s.person_id = Some(pid(ANA));
    s.assignment = Assignment::Confirmed;
    meeting(meeting_id, day, vec![s])
}

fn library() -> (TempDir, Library) {
    let mut ana = Person::new(pid(ANA), "Ana Example");
    ana.aliases.push("Aninha".into());
    ana.emails.push("ana@example.com".into());
    let bia = Person::new(pid(BIA), "Bia Example");
    setup(
        vec![(ana, vec![basis(0)]), (bia, vec![])],
        vec![ana_named(MA, 1)],
    )
}

fn edit(lib: &Library, spec: &PersonEditSpec) -> Result<crate::ops::PersonEditOutcome> {
    with_index(lib, |tx, index| edit_person(tx, index, &pid(ANA), spec))
}

fn undo(lib: &Library, undo_id: &str) -> Result<crate::ops::PersonEditOutcome> {
    with_index(lib, |tx, index| {
        undo_person_edit(tx, index, &pid(ANA), undo_id)
    })
}

fn code(r: Result<impl std::fmt::Debug>) -> &'static str {
    r.unwrap_err().code()
}

#[test]
fn an_edit_is_undone_to_the_exact_previous_values() {
    let (_tmp, lib) = library();
    let before = person_state(&lib, &pid(ANA)).unwrap();
    let saved = edit(
        &lib,
        &PersonEditSpec {
            expected: Some(before.revision.clone()),
            name: Some("Ana Silva".into()),
            aliases: Some(vec!["Ani".into()]),
            notes: Some("Prefers mornings.\n".into()),
            ..PersonEditSpec::default()
        },
    )
    .unwrap();
    assert!(saved.changed);
    assert_eq!(
        saved.fields,
        vec![PersonField::Name, PersonField::Aliases, PersonField::Notes]
    );
    assert_eq!(saved.meetings_rerendered, 1);
    assert!(md(&lib, MA).contains("Ana Silva"));
    let now = person_state(&lib, &pid(ANA)).unwrap();
    assert_eq!(now.revision, saved.revision, "the next edit's baseline");
    assert_eq!(now.person.emails, before.person.emails, "untouched");
    let undo_id = saved.undo_id.unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let file = lib.person_dir(&pid(ANA)).join(crate::ops::PERSON_UNDO_FILE);
        let mode = fs::metadata(file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let undone = undo(&lib, &undo_id).unwrap();
    assert_eq!(undone.undo_id, None, "no redo");
    let after = person_state(&lib, &pid(ANA)).unwrap();
    assert_eq!(after.person, before.person);
    assert_eq!(after.notes, before.notes);
    assert_eq!(after.revision, before.revision);
    assert!(md(&lib, MA).contains("Ana Example"));
    assert_eq!(code(undo(&lib, &undo_id)), "undo_unavailable", "used once");
    assert_eq!(code(undo(&lib, "pu-unknown")), "undo_unavailable");
}

#[test]
fn concurrent_editors_conflict_only_on_the_same_field() {
    let (_tmp, lib) = library();
    let seen = person_state(&lib, &pid(ANA)).unwrap().revision;
    edit(
        &lib,
        &PersonEditSpec {
            expected: Some(seen.clone()),
            name: Some("Ana Silva".into()),
            ..PersonEditSpec::default()
        },
    )
    .unwrap();

    // Another field, same stale baseline: applies, the new name stays.
    edit(
        &lib,
        &PersonEditSpec {
            expected: Some(seen.clone()),
            notes: Some("Team lead.".into()),
            add_emails: vec!["ana.silva@example.com".into()],
            ..PersonEditSpec::default()
        },
    )
    .unwrap();
    let now = person_state(&lib, &pid(ANA)).unwrap();
    assert_eq!(now.person.name, "Ana Silva");
    assert_eq!(now.notes, "Team lead.");
    assert_eq!(now.person.emails.len(), 2);

    // The same field from the stale baseline: refused, nothing written.
    let stale = edit(
        &lib,
        &PersonEditSpec {
            expected: Some(seen),
            name: Some("Ana Costa".into()),
            notes: Some("overwritten".into()),
            ..PersonEditSpec::default()
        },
    );
    assert_eq!(code(stale), "stale_person");
    assert_eq!(person_state(&lib, &pid(ANA)).unwrap(), now);
}

#[test]
fn undo_is_refused_after_a_later_edit_of_the_same_field() {
    let (_tmp, lib) = library();
    let first = edit(
        &lib,
        &PersonEditSpec {
            name: Some("Ana Silva".into()),
            ..PersonEditSpec::default()
        },
    )
    .unwrap();
    edit(
        &lib,
        &PersonEditSpec {
            name: Some("Ana Costa".into()),
            ..PersonEditSpec::default()
        },
    )
    .unwrap();
    assert_eq!(code(undo(&lib, &first.undo_id.unwrap())), "stale_person");
    assert_eq!(
        person_state(&lib, &pid(ANA)).unwrap().person.name,
        "Ana Costa"
    );
}

#[test]
fn undo_never_takes_an_email_someone_else_now_has() {
    let (_tmp, lib) = library();
    let removed = edit(
        &lib,
        &PersonEditSpec {
            remove_emails: vec!["ANA@example.com".into()],
            ..PersonEditSpec::default()
        },
    )
    .unwrap();
    with_index(&lib, |tx, index| {
        edit_person(
            tx,
            index,
            &pid(BIA),
            &PersonEditSpec {
                emails: Some(vec!["ana@example.com".into()]),
                ..PersonEditSpec::default()
            },
        )
    })
    .unwrap();
    assert_eq!(
        code(undo(&lib, &removed.undo_id.unwrap())),
        "undo_unavailable"
    );
    assert!(
        person_state(&lib, &pid(ANA))
            .unwrap()
            .person
            .emails
            .is_empty()
    );
    // And an edit cannot take it either.
    let taken = edit(
        &lib,
        &PersonEditSpec {
            add_emails: vec!["ana@example.com".into()],
            ..PersonEditSpec::default()
        },
    );
    assert_eq!(code(taken), "invalid_input");
}

#[test]
fn forgetting_the_person_ends_every_undo_and_recreates_nothing() {
    let (_tmp, lib) = library();
    let saved = edit(
        &lib,
        &PersonEditSpec {
            name: Some("Ana Silva".into()),
            ..PersonEditSpec::default()
        },
    )
    .unwrap();
    with_index(&lib, |tx, index| forget_person(tx, index, ANA)).unwrap();
    assert!(!lib.person_dir(&pid(ANA)).exists());
    assert_eq!(code(undo(&lib, &saved.undo_id.unwrap())), "not_found");
    assert!(!lib.person_dir(&pid(ANA)).exists(), "no person, no voice");
    let stale = edit(
        &lib,
        &PersonEditSpec {
            name: Some("Ana Example".into()),
            ..PersonEditSpec::default()
        },
    );
    assert_eq!(code(stale), "not_found");
}

#[test]
fn undo_after_forget_voice_restores_names_only() {
    let (_tmp, lib) = library();
    let saved = edit(
        &lib,
        &PersonEditSpec {
            name: Some("Ana Silva".into()),
            ..PersonEditSpec::default()
        },
    )
    .unwrap();
    with_index(&lib, |tx, index| forget_voice(tx, index, ANA)).unwrap();
    undo(&lib, &saved.undo_id.unwrap()).unwrap();
    assert_eq!(
        person_state(&lib, &pid(ANA)).unwrap().person.name,
        "Ana Example"
    );
    assert!(
        lib.load_voiceprints(&pid(ANA))
            .unwrap()
            .embeddings
            .is_empty()
    );
    assert!(
        !lib.person_dir(&pid(ANA))
            .join(paths::VOICEPRINTS_JSON)
            .exists()
    );
}

#[test]
fn edits_keep_me_and_refuse_bad_values_without_writing() {
    let (_tmp, lib) = library();
    let mut me = lib.load_person(&pid(ANA)).unwrap();
    me.is_self = true;
    lib.write(|tx| tx.save_person(&me)).unwrap();
    let before = person_state(&lib, &pid(ANA)).unwrap();
    for bad in [
        PersonEditSpec {
            name: Some("Ana\u{202E}Evil".into()),
            ..PersonEditSpec::default()
        },
        PersonEditSpec {
            name: Some("   ".into()),
            ..PersonEditSpec::default()
        },
        PersonEditSpec {
            add_emails: vec!["not-an-address".into()],
            ..PersonEditSpec::default()
        },
        PersonEditSpec {
            aliases: Some(vec!["A".into()]),
            add_aliases: vec!["B".into()],
            ..PersonEditSpec::default()
        },
    ] {
        assert_eq!(code(edit(&lib, &bad)), "invalid_input");
    }
    assert_eq!(person_state(&lib, &pid(ANA)).unwrap(), before);
    let renamed = edit(
        &lib,
        &PersonEditSpec {
            name: Some("Ana Silva".into()),
            ..PersonEditSpec::default()
        },
    )
    .unwrap();
    assert!(renamed.person.is_self, "an edit never changes Me");
    let unchanged = edit(&lib, &PersonEditSpec::default()).unwrap();
    assert!(!unchanged.changed && unchanged.undo_id.is_none());
}

#[test]
fn person_view_reads_the_indexed_meetings_files_decide() {
    let (_tmp, lib) = library();
    // Ana is only a participant here: the index lists the meeting, its
    // file says she never spoke.
    let mut listed = meeting(MB, 2, vec![spk(1, None)]);
    listed.participants.push(Participant {
        name: Some("Ana Example".into()),
        email: None,
        is_self: false,
        person_id: Some(pid(ANA)),
        evidence: Vec::new(),
    });
    // Named, newer, and grouped: S2 merged into S1 counts once.
    let mut newer = ana_named(MC, 3);
    newer.speakers.push(spk(2, None));
    newer.speaker_groups = vec![schema::SpeakerGroup::new(
        sid(1),
        [sid(2)],
        schema::GroupOrigin::User,
    )];
    with_index(&lib, |tx, index| {
        for m in [&listed, &newer] {
            tx.create_meeting_dir(m)?;
            tx.save_meeting(m)?;
            index.upsert_meeting(tx, m)?;
        }
        Ok(())
    })
    .unwrap();

    let index = Index::open_readonly(lib.root()).unwrap();
    let view = person_view(&lib, &index, &pid(ANA)).unwrap();
    assert_eq!(view.person.name, "Ana Example");
    assert_eq!(
        view.revision,
        person_state(&lib, &pid(ANA)).unwrap().revision
    );
    let meetings: Vec<(&str, Vec<SpeakerId>)> = view
        .meetings
        .iter()
        .map(|m| (m.meeting_id.as_str(), m.speakers.clone()))
        .collect();
    assert_eq!(meetings, vec![(MC, vec![sid(1)]), (MA, vec![sid(1)])]);
    let stored = lib.load_meeting(&mid(MC)).unwrap();
    assert_eq!(view.meetings[0].talk_s, effective_talk_s(&stored, sid(1)));
    assert_eq!(view.voiceprints.len(), 1);
    assert_eq!(view.voiceprints[0].count, 1);
    assert_eq!(view.voiceprints[0].strength, Strength::Confirmed);

    with_index(&lib, |tx, index| forget_person(tx, index, ANA)).unwrap();
    assert!(matches!(
        person_view(&lib, &index, &pid(ANA)),
        Err(Error::NotFound(_))
    ));
}
