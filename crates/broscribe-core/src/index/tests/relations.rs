//! Person relations (layout 8): the meetings a person is related to, as
//! candidates for reading their files.

use super::*;
use crate::live_identity::{LiveAction, LiveAttribution, LiveOutcome, LiveRange};
use crate::schema::UtterancePerson;

fn live(decision: u64, person: &str, outcome: LiveOutcome, user_override: bool) -> LiveAttribution {
    LiveAttribution {
        turn_id: format!("t{decision}"),
        decision,
        action: LiveAction::Name,
        person_id: Some(pid(person)),
        range: LiveRange {
            track: "t0".into(),
            from_sample: 0,
            to_sample: 16_000,
            from: None,
            to: None,
        },
        from_s: None,
        to_s: None,
        supported: Vec::new(),
        outcome,
        at: ts("2026-11-15T10:00:00Z"),
        user_override,
        forget_fence: None,
    }
}

/// [`setup`]'s meetings plus: M1 segment 2 attributed to Caio by a line
/// correction; M2 with a live decision for Ana awaiting review, one for
/// Ana already applied and one for Caio the user settled.
fn related_meetings() -> Vec<Meeting> {
    let mut ms = meetings();
    let f = crate::ops::segment_fingerprint(&ms[0].segments[2]);
    ms[0].utterance_people.push(UtterancePerson {
        track: f.track,
        start: f.start,
        end: f.end,
        text_sha256: f.text_sha256,
        person_id: pid(CAIO),
        at: ts("2026-11-15T10:00:00Z"),
        live_decision: None,
    });
    ms[1].live_attributions = vec![
        live(1, ANA, LiveOutcome::Unmapped, false),
        live(2, ANA, LiveOutcome::Applied, false),
        live(3, CAIO, LiveOutcome::Conflicting, true),
    ];
    ms
}

fn save(lib: &Library, ms: &[Meeting]) {
    lib.write(|tx| {
        for m in ms {
            tx.save_meeting(m)?;
        }
        Ok(())
    })
    .unwrap();
}

fn relations(idx: &Index, person: &str) -> Vec<(String, Vec<PersonRelation>)> {
    idx.person_meeting_candidates(&pid(person))
        .unwrap()
        .into_iter()
        .map(|c| (c.meeting_id.to_string(), c.relations))
        .collect()
}

#[test]
fn person_relations_name_every_related_meeting_newest_first() {
    use PersonRelation::{Line, LiveReview, Participant, Speaker};
    let (_tmp, lib) = setup();
    save(&lib, &related_meetings());
    rebuild(&lib);
    let check = |idx: &Index| {
        // Ana: a confirmed speaker and a linked participant of M1, and an
        // open live decision in M2 (the applied one and M2's suggestion
        // name nobody).
        assert_eq!(
            relations(idx, ANA),
            [
                (M2.to_owned(), vec![LiveReview]),
                (M1.to_owned(), vec![Speaker, Participant]),
            ]
        );
        assert_eq!(
            relations(idx, BRUNO),
            [
                (M2.to_owned(), vec![Speaker]),
                (M1.to_owned(), vec![Speaker])
            ]
        );
        // Caio: the corrected line; his settled live decision and M1's
        // suggestion are not relations.
        assert_eq!(relations(idx, CAIO), [(M1.to_owned(), vec![Line])]);
        assert!(relations(idx, "p-dddddd").is_empty());
        let c = &idx.person_meeting_candidates(&pid(ANA)).unwrap()[0];
        assert_eq!(c.started_at, ts("2026-11-12T09:00:00Z"));
    };
    check(&reader(&lib));

    // Incremental writes give the same rows as the rebuild.
    let mut idx = Index::open(lib.root()).unwrap();
    lib.write(|tx| {
        for m in related_meetings() {
            idx.upsert_meeting(tx, &m)?;
        }
        Ok(())
    })
    .unwrap();
    check(&idx);
    check(&reader(&lib));

    // Settling Ana's open decision takes the relation away; removing the
    // meeting's rows takes all of them.
    let mut m2 = related_meetings().remove(1);
    m2.live_attributions[0].user_override = true;
    lib.write(|tx| {
        tx.save_meeting(&m2)?;
        idx.upsert_meeting(tx, &m2)
    })
    .unwrap();
    assert_eq!(
        relations(&reader(&lib), ANA),
        [(M1.to_owned(), vec![Speaker, Participant])]
    );
    lib.write(|tx| idx.remove_meeting(tx, &mid(M1))).unwrap();
    assert!(relations(&reader(&lib), ANA).is_empty());
    assert!(relations(&reader(&lib), CAIO).is_empty());
}

#[test]
fn upgraded_indexes_refuse_person_relations_until_rebuilt() {
    let (_tmp, lib) = setup();
    save(&lib, &related_meetings());
    rebuild(&lib);
    let before = reader(&lib).read_revision().unwrap();
    downgrade_to_v7(&lib);
    assert!(pending(&lib).rebuild, "an older layout needs a rebuild");

    // A reader upgrades it in place: the new table is empty, and saying so
    // would pass missing relations off as none.
    let idx = reader(&lib);
    assert_eq!(idx.version, INDEX_SCHEMA_VERSION);
    assert_ne!(idx.read_revision().unwrap().instance, before.instance);
    let err = idx.person_meeting_candidates(&pid(ANA)).unwrap_err();
    assert_eq!(err.code(), "schema");
    assert!(err.to_string().contains("broscribe index rebuild"));
    drop(idx);
    assert!(pending(&lib).rebuild);

    // Indexing one meeting does not backfill the others.
    let mut idx = Index::open(lib.root()).unwrap();
    lib.write(|tx| idx.upsert_meeting(tx, &related_meetings()[0]))
        .unwrap();
    assert_eq!(
        reader(&lib)
            .person_meeting_candidates(&pid(ANA))
            .unwrap_err()
            .code(),
        "schema"
    );
    assert!(pending(&lib).rebuild);

    // The repair rebuilds and every relation is back.
    let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
    assert!(report.rebuilt.is_some());
    assert!(pending(&lib).is_current());
    assert_eq!(relations(&reader(&lib), ANA).len(), 2);
    assert_eq!(relations(&reader(&lib), CAIO).len(), 1);
}
