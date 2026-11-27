use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset};
use rusqlite::Connection;
use tempfile::TempDir;

use super::*;
use crate::render::speaker_label;
use crate::schema::{
    Assignment, LanguageSpan, Meeting, Participant, Person, Segment, Source, Speaker, Suggestion,
};
use crate::{Library, MeetingId, PersonId, SpeakerId};

const ANA: &str = "p-a1a1a1";
const BRUNO: &str = "p-b2b2b2";
const CAIO: &str = "p-c3c3c3";
const M1: &str = "m-000001";
const M2: &str = "m-000002";

fn ts(s: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(s).unwrap()
}

fn pid(s: &str) -> PersonId {
    PersonId::parse(s).unwrap()
}

fn mid(s: &str) -> MeetingId {
    MeetingId::parse(s).unwrap()
}

fn people() -> Vec<Person> {
    let mut ana = Person::new(pid(ANA), "Ana Example");
    ana.emails.push("Ana@Example.com".into());
    ana.aliases.push("Aninha".into());
    let mut bruno = Person::new(pid(BRUNO), "Bruno Sample");
    bruno.is_self = true;
    bruno.emails.push("bruno@example.org".into());
    let caio = Person::new(pid(CAIO), "Caio Demo");
    vec![ana, bruno, caio]
}

fn speaker(
    n: u32,
    person: Option<&str>,
    assignment: Assignment,
    talk_s: f64,
    suggestion: Option<(&str, f32)>,
) -> Speaker {
    let mut s = Speaker::new(SpeakerId::new(n).unwrap(), "t0");
    s.person_id = person.map(pid);
    s.assignment = assignment;
    s.talk_s = talk_s;
    s.score = person.map(|_| 0.8);
    s.suggestion = suggestion.map(|(p, score)| Suggestion {
        person_id: pid(p),
        score,
    });
    s
}

fn seg(id: u32, speaker: Option<u32>, start: f64, text: &str) -> Segment {
    Segment {
        speech_overlap: Vec::new(),
        id,
        track: "t0".into(),
        speaker: speaker.map(|n| SpeakerId::new(n).unwrap()),
        start,
        end: start + 2.0,
        text: text.into(),
        words: Vec::new(),
    }
}

fn meetings() -> Vec<Meeting> {
    let mut m1 = Meeting::new(mid(M1), Source::Import, ts("2026-11-10T10:00:00-03:00"));
    m1.title = Some("Roadmap review".into());
    m1.duration_s = 600.0;
    m1.languages.push(LanguageSpan {
        lang: "pt".into(),
        from_s: 0.0,
        to_s: 600.0,
    });
    m1.tags.push("planning".into());
    m1.participants = vec![
        Participant {
            name: Some("Ana Example".into()),
            email: Some("ana@example.com".into()),
            person_id: Some(pid(ANA)),
            ..Default::default()
        },
        Participant {
            name: Some("Dora Guest".into()),
            email: Some("dora@example.net".into()),
            ..Default::default()
        },
    ];
    m1.speakers = vec![
        speaker(1, Some(ANA), Assignment::Confirmed, 120.0, None),
        speaker(2, None, Assignment::None, 90.0, Some((CAIO, 0.6))),
        speaker(3, Some(BRUNO), Assignment::Auto, 60.0, None),
    ];
    m1.segments = vec![
        seg(
            0,
            Some(1),
            1.0,
            "We should ship the café feature this week.",
        ),
        seg(1, Some(2), 4.0, "What about the budget?"),
        seg(2, Some(3), 7.0, "The budget is approved."),
        seg(3, None, 10.0, "Reunião começa agora."),
    ];

    let mut m2 = Meeting::new(mid(M2), Source::Recording, ts("2026-11-12T09:00:00Z"));
    m2.title = Some("Weekly sync".into());
    m2.duration_s = 300.0;
    m2.tags = vec!["Sync".into(), "decision".into()];
    m2.speakers = vec![
        speaker(1, Some(BRUNO), Assignment::Confirmed, 200.0, None),
        speaker(2, None, Assignment::Suggested, 30.0, Some((ANA, 0.55))),
    ];
    m2.segments = vec![
        seg(0, Some(1), 0.5, "The budget review is next week."),
        seg(1, Some(2), 3.0, r#"Ship it -- "now"* please."#),
    ];
    vec![m1, m2]
}

/// A library with an initially empty index, then unindexed people and meetings.
fn setup() -> (TempDir, Library) {
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path()).unwrap();
    lib.write(|tx| {
        for p in people() {
            tx.save_person(&p)?;
        }
        for m in meetings() {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)?;
        }
        Ok(())
    })
    .unwrap();
    (tmp, lib)
}

/// Indexes meetings before people, so names arrive after the meetings.
fn index_incrementally(lib: &Library) -> Index {
    let mut idx = Index::open(lib.root()).unwrap();
    lib.write(|tx| {
        for m in meetings() {
            idx.upsert_meeting(tx, &m)?;
        }
        for p in people() {
            idx.upsert_person(tx, &p)?;
        }
        Ok(())
    })
    .unwrap();
    idx
}

fn rebuild(lib: &Library) -> RebuildReport {
    lib.write(|tx| Index::rebuild(tx)).unwrap()
}

fn reader(lib: &Library) -> Index {
    Index::open_readonly(lib.root()).unwrap()
}

fn text(t: &str) -> SearchQuery {
    SearchQuery {
        text: Some(t.into()),
        ..Default::default()
    }
}

fn person(p: &str) -> SearchQuery {
    SearchQuery {
        person: Some(p.into()),
        ..Default::default()
    }
}

/// `(meeting, segment)` of each hit.
fn keys(hits: &[SearchHit]) -> Vec<(String, u32)> {
    hits.iter()
        .map(|h| (h.meeting_id.to_string(), h.segment_id))
        .collect()
}

fn meeting_keys(hits: &[MeetingHit]) -> Vec<String> {
    hits.iter().map(|h| h.meeting_id.to_string()).collect()
}

fn fts_integrity(idx: &Index) {
    idx.conn
        .execute(
            "INSERT INTO utterances_fts(utterances_fts) VALUES ('integrity-check')",
            [],
        )
        .unwrap();
}

#[derive(Debug, PartialEq)]
struct Snapshot {
    hits: Vec<Vec<SearchHit>>,
    meetings: Vec<Vec<MeetingHit>>,
    review: Vec<ReviewItem>,
    people: Vec<PersonSummary>,
    ids: Vec<MeetingId>,
}

fn snapshot(idx: &Index) -> Snapshot {
    let queries = [
        text("budget"),
        text("ship"),
        person("ana example"),
        person("aninha"),
        SearchQuery {
            tag: Some("planning".into()),
            ..Default::default()
        },
        SearchQuery::default(),
        SearchQuery {
            text: Some("budget".into()),
            limit: Some(1),
            ..Default::default()
        },
    ];
    let meeting_queries = [
        text("ana"),
        text("sync"),
        person("bruno sample"),
        SearchQuery::default(),
    ];
    Snapshot {
        hits: queries.iter().map(|q| idx.search(q).unwrap()).collect(),
        meetings: meeting_queries
            .iter()
            .map(|q| idx.search_meetings(q).unwrap())
            .collect(),
        review: idx.review_queue(None).unwrap(),
        people: idx.list_people().unwrap(),
        ids: idx.meeting_ids().unwrap(),
    }
}

#[test]
fn rebuild_from_files_equals_incremental_state() {
    let (_tmp, lib) = setup();
    let idx = index_incrementally(&lib);
    fts_integrity(&idx);
    let incremental = snapshot(&idx);
    assert_eq!(incremental.hits[0].len(), 3);
    assert_eq!(incremental.ids, vec![mid(M2), mid(M1)]);
    drop(idx);

    // Over the existing database (rows cleared in place).
    let report = rebuild(&lib);
    assert_eq!(
        (report.meetings, report.people, report.utterances),
        (2, 3, 6)
    );
    assert!(report.skipped.is_empty());
    assert_eq!(snapshot(&reader(&lib)), incremental);

    // Speaker names are the ones meeting.md shows.
    let lookup = lib.people_lookup().unwrap();
    let all = reader(&lib).search(&SearchQuery::default()).unwrap();
    assert_eq!(all.len(), 6);
    for hit in &all {
        let m = lib.load_meeting(&hit.meeting_id).unwrap();
        assert_eq!(hit.speaker_name, speaker_label(&m, hit.speaker_id, &lookup));
    }
    let names: Vec<&str> = all.iter().map(|h| h.speaker_name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Bruno Sample",
            "Speaker 2",
            "Ana Example",
            "Speaker 2",
            "Bruno Sample",
            "Unknown"
        ]
    );

    // From scratch, after `.index/` was deleted.
    fs::remove_dir_all(lib.root().join(paths::INDEX_DIR)).unwrap();
    assert_eq!(
        Index::open_readonly(lib.root()).err().map(|e| e.code()),
        Some("not_found")
    );
    rebuild(&lib);
    // FTS5 runs `integrity-check` as an INSERT, which a `query_only`
    // reader refuses, so check through a writable connection.
    fts_integrity(&Index::open(lib.root()).unwrap());
    assert_eq!(snapshot(&reader(&lib)), incremental);
}

#[test]
fn person_changes_and_removals_stay_consistent_with_rebuild() {
    let (_tmp, lib) = setup();
    let mut idx = index_incrementally(&lib);
    let mut ana = people().remove(0);
    ana.name = "Ana Renamed".into();
    lib.write(|tx| {
        tx.save_person(&ana)?;
        idx.upsert_person(tx, &ana)
    })
    .unwrap();
    let hits = idx.search(&person("ana renamed")).unwrap();
    assert_eq!(keys(&hits), [(M1.to_owned(), 0)]);
    assert_eq!(hits[0].speaker_name, "Ana Renamed");
    assert_eq!(
        meeting_keys(&idx.search_meetings(&text("renamed")).unwrap()),
        [M1]
    );

    // Removing a meeting and indexing it again keeps the FTS consistent.
    lib.write(|tx| {
        assert!(idx.remove_meeting(tx, &mid(M1))?);
        assert!(!idx.remove_meeting(tx, &mid(M1))?);
        Ok(())
    })
    .unwrap();
    fts_integrity(&idx);
    assert!(idx.search(&text("café")).unwrap().is_empty());
    assert_eq!(idx.meeting_ids().unwrap(), vec![mid(M2)]);
    let m1 = lib.load_meeting(&mid(M1)).unwrap();
    lib.write(|tx| idx.upsert_meeting(tx, &m1)).unwrap();
    fts_integrity(&idx);
    assert_eq!(idx.search(&text("café")).unwrap().len(), 1);

    let incremental = snapshot(&idx);
    rebuild(&lib);
    assert_eq!(snapshot(&reader(&lib)), incremental);

    // A removed person's speakers fall back to "Speaker N".
    lib.write(|tx| {
        tx.delete_person_dir(&pid(CAIO))?;
        assert!(idx.remove_person(tx, &pid(CAIO))?);
        tx.delete_person_dir(&pid(BRUNO))?;
        assert!(idx.remove_person(tx, &pid(BRUNO))?);
        Ok(())
    })
    .unwrap();
    let hits = idx
        .search(&SearchQuery {
            meeting: Some(mid(M2)),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits[0].speaker_name, "Speaker 1");
    assert!(idx.search_meetings(&text("bruno")).unwrap().is_empty());
    let incremental = snapshot(&idx);
    rebuild(&lib);
    assert_eq!(snapshot(&reader(&lib)), incremental);
}

#[test]
fn person_filter_by_id_name_alias_and_email() {
    let (_tmp, lib) = setup();
    let idx = index_incrementally(&lib);
    for who in ["ANA EXAMPLE", " ana@example.COM ", "aninha", ANA] {
        assert_eq!(
            keys(&idx.search(&person(who)).unwrap()),
            [(M1.to_owned(), 0)],
            "{who}"
        );
    }
    assert!(idx.search(&person("nobody")).unwrap().is_empty());

    // Confirmed and auto speakers count; a suggestion does not.
    let bruno = idx.search(&person("Bruno Sample")).unwrap();
    assert_eq!(keys(&bruno), [(M2.to_owned(), 0), (M1.to_owned(), 2)]);
    let q = SearchQuery {
        text: Some("budget".into()),
        person: Some("bruno@example.org".into()),
        ..Default::default()
    };
    assert_eq!(idx.search(&q).unwrap().len(), 2);

    assert_eq!(
        meeting_keys(&idx.search_meetings(&person("Ana Example")).unwrap()),
        [M1]
    );
    assert_eq!(
        meeting_keys(&idx.search_meetings(&person("dora@example.net")).unwrap()),
        [M1]
    );
    assert_eq!(
        meeting_keys(&idx.search_meetings(&person(BRUNO)).unwrap()),
        [M2, M1]
    );
}

#[test]
fn tag_and_date_filters() {
    let (_tmp, lib) = setup();
    let idx = index_incrementally(&lib);
    let tag = |t: &str| SearchQuery {
        tag: Some(t.into()),
        ..Default::default()
    };
    let hits = idx.search(&tag("SYNC")).unwrap();
    assert_eq!(keys(&hits), [(M2.to_owned(), 0), (M2.to_owned(), 1)]);
    let meetings = idx.search_meetings(&tag("planning")).unwrap();
    assert_eq!(meeting_keys(&meetings), [M1]);
    assert_eq!(meetings[0].tags, ["planning"]);
    assert_eq!(
        idx.search_meetings(&tag("decision")).unwrap()[0].tags,
        ["decision", "Sync"]
    );
    assert!(idx.search(&tag("missing")).unwrap().is_empty());

    let cut = ts("2026-11-11T00:00:00Z");
    let since = SearchQuery {
        since: Some(cut),
        ..Default::default()
    };
    let until = SearchQuery {
        until: Some(cut),
        ..Default::default()
    };
    assert!(
        idx.search(&since)
            .unwrap()
            .iter()
            .all(|h| h.meeting_id == mid(M2))
    );
    assert_eq!(idx.search(&until).unwrap().len(), 4);
    assert_eq!(meeting_keys(&idx.search_meetings(&since).unwrap()), [M2]);
    assert_eq!(meeting_keys(&idx.search_meetings(&until).unwrap()), [M1]);
    // `since` is inclusive, `until` exclusive.
    let exact = SearchQuery {
        since: Some(ts("2026-11-12T09:00:00Z")),
        until: Some(ts("2026-11-12T09:00:01Z")),
        ..Default::default()
    };
    assert_eq!(meeting_keys(&idx.search_meetings(&exact).unwrap()), [M2]);

    let one = SearchQuery {
        meeting: Some(mid(M1)),
        limit: Some(2),
        ..Default::default()
    };
    assert_eq!(
        keys(&idx.search(&one).unwrap()),
        [(M1.to_owned(), 0), (M1.to_owned(), 1)]
    );
}

#[test]
fn search_ignores_diacritics_and_case() {
    let (_tmp, lib) = setup();
    let idx = index_incrementally(&lib);
    let hits = idx.search(&text("CAFE")).unwrap();
    assert_eq!(keys(&hits), [(M1.to_owned(), 0)]);
    assert!(hits[0].snippet.contains("**café**"), "{}", hits[0].snippet);
    assert_eq!(hits[0].speaker_name, "Ana Example");
    assert_eq!(hits[0].meeting_title.as_deref(), Some("Roadmap review"));
    assert_eq!(hits[0].started_at, ts("2026-11-10T10:00:00-03:00"));

    let hits = idx.search(&text("reuniao COMECA")).unwrap();
    assert_eq!(keys(&hits), [(M1.to_owned(), 3)]);
    assert_eq!(hits[0].speaker_id, None);
    assert_eq!(hits[0].speaker_name, "Unknown");
    assert_eq!(idx.search(&text("café")).unwrap().len(), 1);
    assert_eq!(
        meeting_keys(&idx.search_meetings(&text("roadmap")).unwrap()),
        [M1]
    );
}

#[test]
fn fts_syntax_in_user_text_is_quoted() {
    assert_eq!(fts_query("budget"), Some(r#""budget""#.into()));
    assert_eq!(fts_query(r#"say "hi"#), Some(r#""say" AND """hi""#.into()));
    assert_eq!(fts_query("foo*"), Some(r#""foo*""#.into()));
    assert_eq!(fts_query("-ship"), Some(r#""-ship""#.into()));
    assert_eq!(
        fts_query("NOT OR AND"),
        Some(r#""NOT" AND "OR" AND "AND""#.into())
    );
    assert_eq!(fts_query(r#" - * " ( ) "#), None);

    let (_tmp, lib) = setup();
    let idx = index_incrementally(&lib);
    assert_eq!(
        keys(&idx.search(&text(r#"ship -- "now"*"#)).unwrap()),
        [(M2.to_owned(), 1)]
    );
    assert_eq!(idx.search(&text(r#""now"#)).unwrap().len(), 1);
    // `-` is not negation, `OR`/`NOT` are terms, `*` is not a prefix.
    assert_eq!(idx.search(&text("-budget")).unwrap().len(), 3);
    assert!(idx.search(&text("budget OR approved")).unwrap().is_empty());
    assert!(idx.search(&text("NOT budget")).unwrap().is_empty());
    assert!(idx.search(&text("bud*")).unwrap().is_empty());
    for raw in [
        "*",
        "-",
        "\"",
        "title:budget",
        "(budget",
        "budget)",
        "^budget",
        "NEAR(a b)",
    ] {
        idx.search(&text(raw)).unwrap();
        idx.search_meetings(&text(raw)).unwrap();
    }
    assert!(idx.search(&text("*")).unwrap().is_empty());
    assert!(idx.search(&text("   ")).unwrap().is_empty());
}

#[test]
fn reader_keeps_its_snapshot_across_rebuild() {
    let (_tmp, lib) = setup();
    drop(index_incrementally(&lib));
    let title = |idx: &Index| {
        idx.search_meetings(&SearchQuery {
            meeting: Some(mid(M1)),
            ..Default::default()
        })
        .unwrap()[0]
            .title
            .clone()
            .unwrap()
    };

    let old = reader(&lib);
    old.conn.execute_batch("BEGIN").unwrap();
    assert_eq!(title(&old), "Roadmap review");

    lib.write(|tx| {
        tx.update_meeting(&mid(M1), |m| {
            m.title = Some("Roadmap retro".into());
            Ok(())
        })?;
        Index::rebuild(tx)
    })
    .unwrap();

    // The open read transaction still sees the database before the rebuild.
    assert_eq!(title(&old), "Roadmap review");
    assert!(old.search_meetings(&text("retro")).unwrap().is_empty());
    // A fresh read sees the rebuilt data.
    let fresh = reader(&lib);
    assert_eq!(title(&fresh), "Roadmap retro");
    assert_eq!(fresh.search_meetings(&text("retro")).unwrap().len(), 1);
    old.conn.execute_batch("COMMIT").unwrap();
    assert_eq!(title(&old), "Roadmap retro");

    // Readers cannot write.
    let mut ro = reader(&lib);
    let err = lib
        .write(|tx| ro.upsert_person(tx, &people()[0]))
        .unwrap_err();
    assert_eq!(err.code(), "invalid_input");
}

#[test]
fn review_queue_and_people() {
    let (_tmp, lib) = setup();
    let idx = index_incrementally(&lib);
    let review = idx.review_queue(None).unwrap();
    let order: Vec<(String, String)> = review
        .iter()
        .map(|r| (r.meeting_id.to_string(), r.speaker_id.to_string()))
        .collect();
    assert_eq!(
        order,
        [
            (M1.to_owned(), "S2".to_owned()),
            (M2.to_owned(), "S2".to_owned()),
        ]
    );
    assert_eq!(review[0].assignment, Assignment::None);
    assert_eq!(review[0].suggested_person_id, Some(pid(CAIO)));
    assert_eq!(review[0].suggested_name.as_deref(), Some("Caio Demo"));
    assert_eq!(review[0].suggested_score, Some(0.6));
    assert_eq!(review[1].assignment, Assignment::Suggested);
    assert!(review.iter().all(|r| r.assignment != Assignment::Auto));
    assert_eq!(idx.review_queue(Some(1)).unwrap().len(), 1);

    let people = idx.list_people().unwrap();
    let summary: Vec<(&str, u32, f64)> = people
        .iter()
        .map(|p| (p.name.as_str(), p.meetings, p.talk_s))
        .collect();
    assert_eq!(
        summary,
        [
            ("Ana Example", 1, 120.0),
            ("Bruno Sample", 2, 260.0),
            ("Caio Demo", 0, 0.0)
        ]
    );
    assert_eq!(people[0].emails, ["Ana@Example.com"]);
    assert_eq!(people[0].aliases, ["Aninha"]);
    assert!(people[1].is_self);
}

#[test]
fn review_keeps_unresolved_identities_but_not_trusted_automatic_names() {
    let (_tmp, lib) = setup();
    let mut idx = index_incrementally(&lib);
    let mut person = lib.load_person(&pid(CAIO)).unwrap();
    person.needs_review = true;
    let mut meeting = lib.load_meeting(&mid(M1)).unwrap();
    meeting.speakers = vec![
        speaker(1, Some(ANA), Assignment::Confirmed, 90.0, None),
        speaker(2, Some(BRUNO), Assignment::Auto, 80.0, None),
        speaker(3, Some(CAIO), Assignment::Auto, 70.0, None),
        speaker(4, Some(CAIO), Assignment::Confirmed, 60.0, None),
        speaker(5, None, Assignment::Suggested, 50.0, Some((ANA, 0.999))),
        speaker(6, Some("p-d4d4d4"), Assignment::Auto, 40.0, None),
        speaker(7, None, Assignment::Auto, 30.0, None),
        speaker(8, Some("p-d4d4d4"), Assignment::Confirmed, 20.0, None),
        speaker(9, None, Assignment::None, 10.0, None),
    ];
    // Caption/roster naming can be automatic without a voice score.
    meeting.speakers[1].score = None;
    lib.write(|tx| {
        tx.save_person(&person)?;
        tx.save_meeting(&meeting)?;
        idx.upsert_person(tx, &person)?;
        idx.upsert_meeting(tx, &meeting)
    })
    .unwrap();
    let pending = |index: &Index| {
        let mut ids: Vec<_> = index
            .review_queue(None)
            .unwrap()
            .into_iter()
            .filter(|row| row.meeting_id == mid(M1))
            .map(|row| row.speaker_id.to_string())
            .collect();
        ids.sort();
        ids
    };
    let expected = ["S3", "S4", "S5", "S6", "S7", "S9"];
    assert_eq!(pending(&idx), expected);
    drop(idx);
    rebuild(&lib);
    let rebuilt = Index::open_readonly(lib.root()).unwrap();
    assert_eq!(pending(&rebuilt), expected);
}

#[test]
fn schema_version_mismatch_requires_rebuild() {
    let (_tmp, lib) = setup();
    drop(index_incrementally(&lib));
    {
        let conn = Connection::open(Index::path(lib.root())).unwrap();
        conn.execute(
            "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
        conn.execute_batch("CREATE TABLE legacy(x)").unwrap();
    }
    assert_eq!(
        Index::open(lib.root()).err().map(|e| e.code()),
        Some("schema")
    );
    assert_eq!(
        Index::open_readonly(lib.root()).err().map(|e| e.code()),
        Some("schema")
    );

    let report = rebuild(&lib);
    assert_eq!(report.meetings, 2);
    let idx = reader(&lib);
    let legacy: i64 = idx
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'legacy'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(legacy, 0);
    assert_eq!(idx.search(&text("budget")).unwrap().len(), 3);
    Index::open(lib.root()).unwrap();
}

/// Turns the index back into the version-4 layout (before the reported
/// model columns and template results), keeping its rows.
fn downgrade_to_v4(lib: &Library) {
    downgrade_to_v5(lib);
    let conn = Connection::open(Index::path(lib.root())).unwrap();
    conn.execute_batch(
        "ALTER TABLE enrichments DROP COLUMN requested_model;
         ALTER TABLE enrichments DROP COLUMN model_reported;
         UPDATE meta SET value = '4' WHERE key = 'schema_version';",
    )
    .unwrap();
}

/// Turns the index back into the version-5 layout (before template
/// results), keeping its other rows.
fn downgrade_to_v5(lib: &Library) {
    downgrade_to_v6(lib);
    let conn = Connection::open(Index::path(lib.root())).unwrap();
    conn.execute_batch(
        "DELETE FROM enrichment_fts WHERE kind = 'template';
         DROP TABLE template_results;
         UPDATE meta SET value = '5' WHERE key = 'schema_version';",
    )
    .unwrap();
}

/// Turns the index back into the version-6 layout (before speaker tracks,
/// unnamed labels and meeting setups), keeping its other rows.
fn downgrade_to_v6(lib: &Library) {
    downgrade_to_v7(lib);
    let conn = Connection::open(Index::path(lib.root())).unwrap();
    conn.execute_batch(
        "ALTER TABLE speakers DROP COLUMN track;
         ALTER TABLE speakers DROP COLUMN label;
         ALTER TABLE meetings DROP COLUMN setup;
         UPDATE meta SET value = '6' WHERE key = 'schema_version';",
    )
    .unwrap();
}

/// Turns the index back into the version-7 layout (before person
/// relations), keeping its other rows.
fn downgrade_to_v7(lib: &Library) {
    downgrade_to_v8(lib);
    let conn = Connection::open(Index::path(lib.root())).unwrap();
    conn.execute_batch(
        "DROP TABLE person_meetings;
         UPDATE meta SET value = '7' WHERE key = 'schema_version';",
    )
    .unwrap();
}

/// Turns the index back into the version-8 layout (before people's hues),
/// keeping its other rows.
fn downgrade_to_v8(lib: &Library) {
    let conn = Connection::open(Index::path(lib.root())).unwrap();
    conn.execute_batch(
        "ALTER TABLE people DROP COLUMN hue;
         UPDATE meta SET value = '8' WHERE key = 'schema_version';",
    )
    .unwrap();
}

/// A person's hue lives in their file: a rebuild and an incremental
/// update index the same one, whatever hue the caller's struct holds.
#[test]
fn hues_come_from_the_files_and_survive_a_rebuild() {
    let (_tmp, lib) = setup();
    let mut idx = index_incrementally(&lib);
    let stored: BTreeMap<PersonId, u8> = lib
        .list_people()
        .unwrap()
        .into_iter()
        .map(|p| (p.id.clone(), p.hue.expect("saved people store a hue")))
        .collect();
    let hues = |idx: &Index| -> BTreeMap<PersonId, u8> {
        idx.list_people()
            .unwrap()
            .into_iter()
            .map(|p| (p.id, p.hue))
            .collect()
    };
    assert_eq!(hues(&idx), stored);
    rebuild(&lib);
    assert_eq!(hues(&reader(&lib)), stored);
    // A caller passing a different hue changes neither the file nor the index.
    let mut ana = people().remove(0);
    ana.hue = Some(stored[&ana.id] % crate::schema::PERSON_HUES + 1);
    lib.write(|tx| {
        tx.save_person(&ana)?;
        idx.upsert_person(tx, &ana)
    })
    .unwrap();
    assert_eq!(hues(&idx), stored);
}

/// Layout 8 had no hues: the upgrade adds the column in place, without
/// asking for a rebuild, and every person reads with the hue their file
/// gives (derived from the ID, since no file held one before).
#[test]
fn version_8_index_gains_hues_in_place() {
    let (_tmp, lib) = setup();
    let mut idx = index_incrementally(&lib);
    let before = idx.list_people().unwrap();
    downgrade_to_v8(&lib);
    drop(idx);
    idx = Index::open(lib.root()).unwrap();
    assert_eq!(idx.version, INDEX_SCHEMA_VERSION);
    assert!(!schema::rebuild_required(&idx.conn).unwrap());
    let after = idx.list_people().unwrap();
    for person in &after {
        assert_eq!(person.hue, crate::schema::derived_hue(&person.id));
    }
    assert_eq!(
        after.iter().map(|p| &p.id).collect::<Vec<_>>(),
        before.iter().map(|p| &p.id).collect::<Vec<_>>()
    );
}

#[test]
fn read_only_version_8_index_lists_people_with_derived_hues() {
    let (_tmp, lib) = setup();
    rebuild(&lib);
    downgrade_to_v8(&lib);
    let mut conn =
        Connection::open_with_flags(Index::path(lib.root()), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let version = migrate_if_writable(&mut conn, 8).unwrap();
    assert_eq!(version, 8);
    let index = Index {
        conn,
        read_only: true,
        vectors: false,
        version,
    };
    let people = index.list_people().unwrap();
    assert!(!people.is_empty());
    for person in people {
        assert_eq!(person.hue, crate::schema::derived_hue(&person.id));
    }
}

fn meta_value(lib: &Library, key: &str) -> String {
    Connection::open(Index::path(lib.root()))
        .unwrap()
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .unwrap()
}

fn with_enrichment(
    mut m: Meeting,
    provider: &str,
    requested: Option<&str>,
    model: Option<&str>,
    reported: bool,
    at: &str,
) -> Meeting {
    m.enrichment = Some(
        serde_json::from_value(serde_json::json!({
            "provider": provider,
            "model": model,
            "requested_model": requested,
            "model_reported": reported,
            "created_at": at,
            "summary": "Synthetic summary.",
        }))
        .unwrap(),
    );
    m
}

#[test]
fn version_4_index_is_upgraded_in_place() {
    let (_tmp, lib) = setup();
    rebuild(&lib);
    downgrade_to_v4(&lib);
    // A reader upgrades it; search keeps working, no rebuild in between.
    let idx = reader(&lib);
    assert_eq!(idx.version, INDEX_SCHEMA_VERSION);
    assert_eq!(idx.search(&text("budget")).unwrap().len(), 3);
    assert!(idx.resolved_models("claude-cli").unwrap().is_empty());
    drop(idx);
    assert_eq!(
        meta_value(&lib, "schema_version"),
        INDEX_SCHEMA_VERSION.to_string()
    );

    // A writer publishes an enrichment with requested and reported models.
    let mut writer = Index::open(lib.root()).unwrap();
    let m = with_enrichment(
        meetings().remove(0),
        "claude-cli",
        Some("opus"),
        Some("opus-q-5"),
        true,
        "2026-11-15T10:00:00-03:00",
    );
    lib.write(|tx| {
        tx.save_meeting(&m)?;
        writer.upsert_meeting(tx, &m)
    })
    .unwrap();
    let idx = reader(&lib);
    assert_eq!(idx.search(&text("budget")).unwrap().len(), 3);
    assert_eq!(
        idx.resolved_models("claude-cli").unwrap(),
        BTreeMap::from([(Some("opus".to_owned()), "opus-q-5".to_owned())])
    );
}

#[test]
fn writer_upgrades_version_4_keeping_the_completeness_mark() {
    let (_tmp, lib) = setup();
    rebuild(&lib);
    downgrade_to_v4(&lib);
    Connection::open(Index::path(lib.root()))
        .unwrap()
        .execute("UPDATE meta SET value = '0' WHERE key = 'complete'", [])
        .unwrap();
    Index::open(lib.root()).unwrap();
    assert_eq!(
        meta_value(&lib, "schema_version"),
        INDEX_SCHEMA_VERSION.to_string()
    );
    assert_eq!(meta_value(&lib, "complete"), "0");
    assert_eq!(
        Index::open_readonly(lib.root()).err().map(|e| e.code()),
        Some("not_found"),
        "still incomplete"
    );

    // Older versions still need a rebuild.
    Connection::open(Index::path(lib.root()))
        .unwrap()
        .execute(
            "UPDATE meta SET value = '3' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    for err in [
        Index::open(lib.root()).err(),
        Index::open_readonly(lib.root()).err(),
    ] {
        assert_eq!(err.map(|e| e.code()), Some("schema"));
    }
}

#[cfg(unix)]
#[test]
fn read_only_version_4_index_is_read_without_hints() {
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, lib) = setup();
    rebuild(&lib);
    downgrade_to_v4(&lib);
    let db = Index::path(lib.root());
    let mode = |m| fs::set_permissions(&db, fs::Permissions::from_mode(m)).unwrap();
    mode(0o444);
    if fs::OpenOptions::new().write(true).open(&db).is_ok() {
        // Running as root: file modes do not stop writes.
        mode(0o644);
        return;
    }
    let idx = reader(&lib);
    assert_eq!(idx.version, 4);
    // Read as it is, and reported as needing a rebuild.
    let health = Index::health(lib.root()).unwrap();
    assert!(health.rebuild);
    assert_eq!(health.database, IndexDatabase::Outdated);
    assert_eq!(idx.search(&text("budget")).unwrap().len(), 3);
    assert!(idx.resolved_models("claude-cli").unwrap().is_empty());
    drop(idx);
    // SQLite gives the `-wal`/`-shm` files it created the database's mode.
    mode(0o644);
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        if side.exists() {
            fs::set_permissions(&side, fs::Permissions::from_mode(0o644)).unwrap();
        }
    }
    assert_eq!(meta_value(&lib, "schema_version"), "4");
    // The next open that can write upgrades it.
    assert_eq!(reader(&lib).version, INDEX_SCHEMA_VERSION);
}

#[test]
fn resolved_models_newest_reported_per_request() {
    let (_tmp, lib) = setup();
    let mut ms = meetings();
    let at = |n: u32| ts(&format!("2026-11-0{n}T10:00:00-03:00"));
    let extra = [
        Meeting::new(mid("m-000003"), Source::Import, at(3)),
        Meeting::new(mid("m-000004"), Source::Import, at(4)),
        Meeting::new(mid("m-000005"), Source::Import, at(5)),
    ];
    let [m3, m4, m5] = extra;
    let enriched = vec![
        // Newest default resolution: 12:00Z (its text sorts first).
        with_enrichment(
            ms.remove(0),
            "claude-cli",
            None,
            Some("q-default-1"),
            true,
            "2026-11-15T09:00:00-03:00",
        ),
        // Older (11:30Z) despite the later-looking local time.
        with_enrichment(
            ms.remove(0),
            "claude-cli",
            None,
            Some("q-default-2"),
            true,
            "2026-11-15T13:30:00+02:00",
        ),
        // Newest, but only requested: ignored.
        with_enrichment(
            m3,
            "claude-cli",
            None,
            Some("alias-only"),
            false,
            "2026-11-16T10:00:00-03:00",
        ),
        with_enrichment(
            m4,
            "claude-cli",
            Some("opus"),
            Some("opus-q-5"),
            true,
            "2026-11-14T10:00:00-03:00",
        ),
        // Another provider.
        with_enrichment(
            m5,
            "other-cli",
            None,
            Some("other-1"),
            true,
            "2026-11-17T10:00:00-03:00",
        ),
    ];
    lib.write(|tx| {
        for m in &enriched {
            if ![M1, M2].contains(&m.id.as_str()) {
                tx.create_meeting_dir(m)?;
            }
            tx.save_meeting(m)?;
        }
        Ok(())
    })
    .unwrap();
    rebuild(&lib);
    let idx = reader(&lib);
    assert_eq!(
        idx.resolved_models("claude-cli").unwrap(),
        BTreeMap::from([
            (None, "q-default-1".to_owned()),
            (Some("opus".to_owned()), "opus-q-5".to_owned()),
        ])
    );
    assert_eq!(
        idx.resolved_models("other-cli").unwrap(),
        BTreeMap::from([(None, "other-1".to_owned())])
    );
    assert!(idx.resolved_models("absent-cli").unwrap().is_empty());
}

#[test]
fn rebuild_skips_unreadable_files() {
    let (_tmp, lib) = setup();
    let dir = lib.meeting_dir(&mid(M2)).unwrap();
    fs::write(dir.join(paths::MEETING_JSON), "{ not json").unwrap();
    let report = rebuild(&lib);
    assert_eq!(report.meetings, 1);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].path, dir.join(paths::MEETING_JSON));
    assert_eq!(reader(&lib).meeting_ids().unwrap(), vec![mid(M1)]);
}

#[test]
fn windows_are_indexed_and_pending_until_cached() {
    let (_tmp, lib) = setup();
    let report = rebuild(&lib);
    // M1: four speaker turns (1, 2, 3, unknown); M2: two.
    assert_eq!(report.windows, 6);
    assert_eq!(report.vectors_pruned, 0);
    let idx = reader(&lib);
    assert!(!idx.vectors_attached());
    assert!(!idx.has_vectors("m").unwrap());
    let pending = idx.pending_windows("m", None).unwrap();
    assert_eq!(pending.len(), 6);
    let m1 = lib.load_meeting(&mid(M1)).unwrap();
    let expected = meeting_windows(&m1);
    assert_eq!(
        idx.window_revision(&mid(M1)).unwrap(),
        Some(window_revision(&expected))
    );
    for w in &expected {
        assert!(pending.iter().any(|p| p.meeting_id == mid(M1)
            && p.text_sha256 == w.text_sha256
            && p.text == w.text));
    }
    assert_eq!(idx.pending_windows("m", Some(&mid(M2))).unwrap().len(), 2);
    assert_eq!(idx.text_hashes().unwrap().len(), 6);
    drop(idx);

    // Cached texts are no longer pending for that model only.
    let mut cache = VectorCache::open(lib.root()).unwrap();
    lib.write(|tx| {
        cache.insert(
            tx,
            "m",
            &[
                (expected[0].text_sha256.clone(), vec![1.0, 0.0]),
                (expected[1].text_sha256.clone(), vec![0.0, 1.0]),
            ],
        )
    })
    .unwrap();
    let idx = reader(&lib);
    assert!(idx.vectors_attached());
    assert!(idx.has_vectors("m").unwrap());
    assert!(!idx.has_vectors("other").unwrap());
    assert_eq!(idx.pending_windows("m", None).unwrap().len(), 4);
    assert_eq!(idx.pending_windows("other", None).unwrap().len(), 6);

    // Semantic search: closest window first, scored by cosine; filters apply.
    let semantic = |v: Vec<f32>| SearchQuery {
        text: Some("anything".into()),
        mode: SearchMode::Semantic,
        query_vector: Some(QueryVector {
            model: "m".into(),
            vector: v,
        }),
        ..SearchQuery::default()
    };
    let hits = idx.search(&semantic(vec![0.1, 1.0])).unwrap();
    assert_eq!(keys(&hits), [(M1.to_owned(), 1), (M1.to_owned(), 0)]);
    assert!(hits[0].score.unwrap() > hits[1].score.unwrap());
    assert_eq!(hits[0].snippet, "What about the budget?");
    let other_meeting = SearchQuery {
        meeting: Some(mid(M2)),
        ..semantic(vec![0.1, 1.0])
    };
    assert!(idx.search(&other_meeting).unwrap().is_empty());
    let by_person = SearchQuery {
        person: Some(ANA.into()),
        ..semantic(vec![0.1, 1.0])
    };
    assert_eq!(keys(&idx.search(&by_person).unwrap()), [(M1.to_owned(), 0)]);
    // Another dimension matches nothing; no query vector is refused.
    assert!(
        idx.search(&semantic(vec![1.0, 0.0, 0.0]))
            .unwrap()
            .is_empty()
    );
    let err = idx
        .search(&SearchQuery {
            query_vector: None,
            ..semantic(Vec::new())
        })
        .unwrap_err();
    assert_eq!(err.code(), "invalid_input");
    drop(idx);

    // A rebuild keeps referenced vectors and prunes the others.
    lib.write(|tx| {
        let mut m = tx.library().load_meeting(&mid(M1))?;
        m.segments[0].text = "Different words now.".into();
        tx.save_meeting(&m)
    })
    .unwrap();
    assert_eq!(rebuild(&lib).vectors_pruned, 1);
    assert_eq!(
        VectorCache::open(lib.root()).unwrap().count(None).unwrap(),
        1
    );
}

#[test]
fn keyword_search_works_without_or_with_a_broken_vector_cache() {
    let (_tmp, lib) = setup();
    rebuild(&lib);
    let expected = reader(&lib).search(&text("budget")).unwrap();
    assert_eq!(expected.len(), 3);

    fs::write(VectorCache::path(lib.root()), "not a database at all").unwrap();
    let idx = reader(&lib);
    assert!(!idx.vectors_attached());
    assert_eq!(idx.search(&text("budget")).unwrap(), expected);
    // Hybrid without vectors: the keyword ranking, one hit per window.
    let hybrid = idx
        .search(&SearchQuery {
            mode: SearchMode::Hybrid,
            ..text("budget")
        })
        .unwrap();
    assert_eq!(keys(&hybrid), keys(&expected));
    assert!(hybrid.iter().all(|h| h.score.is_some()));
    drop(idx);

    // A rebuild replaces the unreadable cache.
    rebuild(&lib);
    assert_eq!(
        VectorCache::open(lib.root()).unwrap().count(None).unwrap(),
        0
    );
    assert!(reader(&lib).vectors_attached());
}

#[test]
fn app_and_source_filters() {
    let (_tmp, lib) = setup();
    lib.write(|tx| {
        for (id, app) in [(M1, "Zoom"), (M2, "Teams")] {
            let mut m = tx.library().load_meeting(&mid(id))?;
            m.app = Some(app.into());
            tx.save_meeting(&m)?;
        }
        Ok(())
    })
    .unwrap();
    rebuild(&lib);
    let idx = reader(&lib);
    assert_eq!(idx.meeting_apps().unwrap(), ["Teams", "Zoom"]);

    let all = idx.search_meetings(&SearchQuery::default()).unwrap();
    assert_eq!(meeting_keys(&all), [M2, M1]);
    assert_eq!(all[0].source, Source::Recording);
    assert_eq!(all[0].app.as_deref(), Some("Teams"));
    assert_eq!(all[1].source, Source::Import);

    let zoom = SearchQuery {
        app: Some(" zoom ".into()),
        ..SearchQuery::default()
    };
    assert_eq!(meeting_keys(&idx.search_meetings(&zoom).unwrap()), [M1]);
    let hits = idx.search(&zoom).unwrap();
    assert_eq!(hits.len(), 4);
    assert!(hits.iter().all(|h| h.meeting_id == mid(M1)));

    let recorded = SearchQuery {
        source: Some(Source::Recording),
        ..text("budget")
    };
    assert_eq!(keys(&idx.search(&recorded).unwrap()), [(M2.to_owned(), 0)]);
    let imported = SearchQuery {
        source: Some(Source::Import),
        ..SearchQuery::default()
    };
    assert_eq!(meeting_keys(&idx.search_meetings(&imported).unwrap()), [M1]);
}

#[test]
fn recreated_index_stays_incomplete_until_rebuild() {
    let (_tmp, lib) = setup();
    let index_dir = lib.root().join(paths::INDEX_DIR);
    fs::remove_dir_all(index_dir).unwrap();
    let mut index = Index::open(lib.root()).unwrap();
    let err = Index::open_readonly(lib.root()).err().unwrap();
    assert!(err.to_string().contains("run `broscribe index rebuild`"));
    lib.write(|tx| index.upsert_meeting(tx, &meetings()[0]))
        .unwrap();
    assert!(
        Index::open_readonly(lib.root()).is_err(),
        "partial writes must not clear incompleteness"
    );
    lib.write(|tx| Index::rebuild(tx)).unwrap();
    assert_eq!(
        Index::open_readonly(lib.root())
            .unwrap()
            .meeting_ids()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn recreated_index_with_only_people_stays_incomplete() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path()).unwrap();
    lib.write(|tx| {
        for p in people() {
            tx.save_person(&p)?;
        }
        Ok(())
    })
    .unwrap();
    fs::remove_dir_all(lib.root().join(paths::INDEX_DIR)).unwrap();
    Index::open(lib.root()).unwrap();
    assert!(Index::open_readonly(lib.root()).is_err());
    lib.write(|tx| Index::rebuild(tx)).unwrap();
    assert!(Index::open_readonly(lib.root()).is_ok());
}

#[test]
fn fresh_empty_library_index_is_readable() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path()).unwrap();
    Index::open(lib.root()).unwrap();
    assert!(
        Index::open_readonly(lib.root())
            .unwrap()
            .meeting_ids()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn first_library_write_initializes_index_before_meeting_is_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path()).unwrap();
    let meeting = meetings().remove(0);
    lib.write(|tx| {
        tx.create_meeting_dir(&meeting)?;
        tx.save_meeting(&meeting)?;
        Index::open(tx.root())?.upsert_meeting(tx, &meeting)
    })
    .unwrap();
    assert_eq!(
        Index::open_readonly(lib.root())
            .unwrap()
            .meeting_ids()
            .unwrap(),
        vec![meeting.id],
    );
}

#[test]
fn first_write_after_index_deletion_still_requires_rebuild() {
    let (_tmp, lib) = setup();
    fs::remove_dir_all(lib.root().join(paths::INDEX_DIR)).unwrap();
    lib.write(|tx| Index::open(tx.root())?.upsert_meeting(tx, &meetings()[0]))
        .unwrap();
    assert!(Index::open_readonly(lib.root()).is_err());
    rebuild(&lib);
    assert_eq!(reader(&lib).meeting_ids().unwrap().len(), 2);
}

#[test]
fn line_corrections_drive_keyword_semantic_and_hybrid_person_filters() {
    let (_tmp, lib) = setup();
    // M1 segment 2 ("The budget is approved.", S3 auto-named Bruno) was Caio.
    lib.write(|tx| {
        let mut m = tx.library().load_meeting(&mid(M1))?;
        let f = crate::ops::segment_fingerprint(&m.segments[2]);
        m.utterance_people.push(crate::schema::UtterancePerson {
            track: f.track,
            start: f.start,
            end: f.end,
            text_sha256: f.text_sha256,
            person_id: pid(CAIO),
            at: ts("2026-11-15T10:00:00Z"),
            live_decision: None,
        });
        tx.save_meeting(&m)
    })
    .unwrap();
    rebuild(&lib);
    let m1 = lib.load_meeting(&mid(M1)).unwrap();
    let windows = meeting_windows(&m1);
    let mut cache = VectorCache::open(lib.root()).unwrap();
    let vectors: Vec<(String, Vec<f32>)> = windows
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let mut v = vec![0.0; windows.len()];
            v[i] = 1.0;
            (w.text_sha256.clone(), v)
        })
        .collect();
    lib.write(|tx| cache.insert(tx, "m", &vectors)).unwrap();
    let corrected = windows.iter().position(|w| w.segment_ids == [2]).unwrap();
    assert_eq!(windows[corrected].person_id, Some(pid(CAIO)));

    let query = |person: &str, mode: SearchMode| {
        let mut v = vec![0.0; windows.len()];
        v[corrected] = 1.0;
        SearchQuery {
            text: Some("budget".into()),
            person: Some(person.into()),
            mode,
            query_vector: Some(QueryVector {
                model: "m".into(),
                vector: v,
            }),
            ..SearchQuery::default()
        }
    };
    let check = |idx: &Index| {
        for mode in [
            SearchMode::Keyword,
            SearchMode::Semantic,
            SearchMode::Hybrid,
        ] {
            let hits = idx.search(&query("caio demo", mode)).unwrap();
            assert_eq!(keys(&hits), [(M1.to_owned(), 2)], "{mode:?}");
            assert_eq!(hits[0].speaker_name, "Caio Demo");
            let bruno: Vec<_> = keys(&idx.search(&query("bruno sample", mode)).unwrap());
            assert!(!bruno.contains(&(M1.to_owned(), 2)), "{mode:?}: {bruno:?}");
        }
        let meetings = idx.search_meetings(&person("caio demo")).unwrap();
        assert_eq!(meeting_keys(&meetings), [M1]);
    };
    check(&reader(&lib));
    let before = snapshot(&reader(&lib));

    // Incremental writes and a rebuild agree.
    let mut idx = Index::open(lib.root()).unwrap();
    lib.write(|tx| idx.upsert_meeting(tx, &m1)).unwrap();
    drop(idx);
    check(&reader(&lib));
    assert_eq!(snapshot(&reader(&lib)), before);
    rebuild(&lib);
    check(&reader(&lib));
    assert_eq!(snapshot(&reader(&lib)), before);
}

fn pending(lib: &Library) -> IndexHealth {
    Index::health(lib.root()).unwrap()
}

#[test]
fn canonical_saves_stay_pending_until_their_index_update() {
    let (_tmp, lib) = setup();
    // `setup` saved people and meetings without indexing them.
    let listed = pending(&lib);
    assert_eq!(listed.meetings, BTreeSet::from([mid(M1), mid(M2)]));
    assert_eq!(listed.people.len(), 3);
    assert!(!listed.rebuild);

    let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
    assert_eq!((report.meetings, report.people, report.removed), (2, 3, 0));
    assert!(report.skipped.is_empty() && report.rebuilt.is_none());
    assert!(pending(&lib).is_current());
    assert!(
        !lib.root()
            .join(paths::INDEX_DIR)
            .join(PENDING_REPAIR)
            .exists()
    );
    let repaired = snapshot(&reader(&lib));
    rebuild(&lib);
    assert_eq!(snapshot(&reader(&lib)), repaired);

    // A write whose index update never ran (it failed, or the process died
    // after the save) leaves the meeting listed; the stale row is detectable.
    let mut m1 = lib.load_meeting(&mid(M1)).unwrap();
    m1.title = Some("Quarterly forecast".into());
    let err = lib
        .write(|tx| -> Result<()> {
            tx.save_meeting(&m1)?;
            Err(Error::Internal("index update failed".into()))
        })
        .unwrap_err();
    assert_eq!(err.code(), "internal");
    assert_eq!(pending(&lib).meetings, BTreeSet::from([mid(M1)]));
    assert!(
        reader(&lib)
            .search_meetings(&text("forecast"))
            .unwrap()
            .is_empty()
    );
    let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
    assert_eq!(report.meetings, 1);
    assert!(pending(&lib).is_current());
    assert_eq!(
        meeting_keys(&reader(&lib).search_meetings(&text("forecast")).unwrap()),
        [M1]
    );

    // Saved and indexed in one write: nothing stays listed.
    m1.title = Some("Quarterly plan".into());
    let mut ana = people().remove(0);
    ana.name = "Ana Planner".into();
    lib.write(|tx| {
        tx.save_meeting(&m1)?;
        tx.save_person(&ana)?;
        let mut idx = Index::open(tx.root())?;
        idx.upsert_meeting(tx, &m1)?;
        idx.upsert_person(tx, &ana)
    })
    .unwrap();
    assert!(pending(&lib).is_current());

    // A save after a rebuild in the same write is listed again.
    lib.write(|tx| {
        Index::rebuild(tx)?;
        tx.save_meeting(&m1)
    })
    .unwrap();
    assert_eq!(pending(&lib).meetings, BTreeSet::from([mid(M1)]));
    lib.write(|tx| Index::repair_pending(tx)).unwrap();

    // A listed meeting whose folder is gone loses its rows.
    let m2 = lib.load_meeting(&mid(M2)).unwrap();
    let dir2 = lib.meeting_dir(&mid(M2)).unwrap();
    lib.write(|tx| tx.save_meeting(&m2)).unwrap();
    fs::remove_dir_all(&dir2).unwrap();
    let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
    assert_eq!(report.removed, 1);
    assert_eq!(reader(&lib).meeting_ids().unwrap(), vec![mid(M1)]);

    // An unreadable list asks for, and gets, a full rebuild.
    fs::write(lib.root().join(paths::INDEX_DIR).join(PENDING_REPAIR), b"{").unwrap();
    assert!(pending(&lib).rebuild);
    let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
    assert_eq!(report.rebuilt.map(|r| r.meetings), Some(1));
    assert!(pending(&lib).is_current());
}

/// A rebuild that cannot read a meeting or a person keeps them pending:
/// the person by its ID, the meeting folder as a full rebuild (nothing read
/// proves which meeting it holds). What it read answers searches, the index
/// is not current, and repairs while the files stay unreadable keep it so.
/// Once the files read again (restored as they were, with no write listing
/// them anew) the repair indexes them and the index is current.
#[test]
fn a_rebuild_keeps_what_it_could_not_read_pending_until_indexed() {
    let (_tmp, lib) = setup();
    let meeting_json = lib.meeting_dir(&mid(M2)).unwrap();
    let meeting_json = meeting_json.join(paths::MEETING_JSON);
    let person_md = lib.person_dir(&pid(CAIO)).join(paths::PERSON_MD);
    let saved_meeting = fs::read(&meeting_json).unwrap();
    let saved_person = fs::read(&person_md).unwrap();
    fs::write(&meeting_json, "{ not json").unwrap();
    fs::write(&person_md, "not a person").unwrap();
    // A deleted index (its list with it): only a rebuild repairs it.
    fs::remove_dir_all(lib.root().join(paths::INDEX_DIR)).unwrap();
    assert!(pending(&lib).rebuild);

    let mut expected = vec![meeting_json.clone(), person_md.clone()];
    expected.sort();
    for _ in 0..3 {
        let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
        let rebuilt = report.rebuilt.expect("a rebuild");
        assert_eq!((rebuilt.meetings, rebuilt.people), (1, 2));
        let mut skipped: Vec<PathBuf> = rebuilt.skipped.into_iter().map(|s| s.path).collect();
        skipped.sort();
        assert_eq!(skipped, expected);
        let health = pending(&lib);
        assert_eq!(health.database, IndexDatabase::Ready);
        assert!(health.rebuild && !health.is_current(), "{health:?}");
        assert!(health.meetings.is_empty());
        assert_eq!(health.people, BTreeSet::from([pid(CAIO)]));
        let idx = reader(&lib);
        assert_eq!(idx.meeting_ids().unwrap(), vec![mid(M1)]);
        assert_eq!(
            meeting_keys(&idx.search_meetings(&text("roadmap")).unwrap()),
            [M1]
        );
    }

    fs::write(&meeting_json, &saved_meeting).unwrap();
    fs::write(&person_md, &saved_person).unwrap();
    let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
    let rebuilt = report.rebuilt.unwrap();
    assert_eq!((rebuilt.meetings, rebuilt.people), (2, 3));
    assert!(rebuilt.skipped.is_empty());
    assert!(pending(&lib).is_current());
    let idx = reader(&lib);
    assert_eq!(idx.meeting_ids().unwrap(), vec![mid(M2), mid(M1)]);
    assert_eq!(
        meeting_keys(&idx.search_meetings(&text("weekly")).unwrap()),
        [M2]
    );
}

/// What a meeting folder holds decides, not its name: an unreadable
/// `meeting.json` that reads again holding another meeting ID than the one
/// its folder name ends with is indexed under the ID it holds, and only
/// then is the index current. A `person.md` holding another person than its
/// folder's is never indexed: it stays pending, not current, until fixed.
#[test]
fn a_skipped_file_is_indexed_by_the_identity_it_holds() {
    let (_tmp, lib) = setup();
    let meeting_json = lib.meeting_dir(&mid(M2)).unwrap();
    let meeting_json = meeting_json.join(paths::MEETING_JSON);
    let person_md = lib.person_dir(&pid(CAIO)).join(paths::PERSON_MD);
    let saved_person = fs::read(&person_md).unwrap();
    fs::write(&meeting_json, "{ not json").unwrap();
    fs::write(&person_md, "not a person").unwrap();
    fs::remove_dir_all(lib.root().join(paths::INDEX_DIR)).unwrap();
    lib.write(|tx| Index::repair_pending(tx)).unwrap();
    assert!(pending(&lib).rebuild);

    // Readable again, with no write listing them: the meeting under an ID
    // its folder name does not end with, the person file holding Ana.
    const MOVED: &str = "m-0000b0";
    let mut moved = meetings().remove(1);
    moved.id = mid(MOVED);
    moved.title = Some("Relocated okapi sync".into());
    fs::write(&meeting_json, serde_json::to_vec(&moved).unwrap()).unwrap();
    let ana_md = fs::read(lib.person_dir(&pid(ANA)).join(paths::PERSON_MD)).unwrap();
    fs::write(&person_md, ana_md).unwrap();
    for _ in 0..2 {
        let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
        let skipped = match report.rebuilt {
            Some(rebuilt) => rebuilt.skipped,
            None => report.skipped,
        };
        assert_eq!(skipped.len(), 1);
        let health = pending(&lib);
        assert!(!health.rebuild && !health.is_current(), "{health:?}");
        assert!(health.meetings.is_empty());
        assert_eq!(health.people, BTreeSet::from([pid(CAIO)]));
        let idx = reader(&lib);
        assert_eq!(idx.meeting_ids().unwrap(), vec![mid(MOVED), mid(M1)]);
        assert_eq!(
            meeting_keys(&idx.search_meetings(&text("okapi")).unwrap()),
            [MOVED]
        );
    }

    fs::write(&person_md, &saved_person).unwrap();
    let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
    assert_eq!(report.people, 1);
    assert!(pending(&lib).is_current());
    assert_eq!(
        reader(&lib).meeting_ids().unwrap(),
        vec![mid(MOVED), mid(M1)]
    );
}

/// A rebuild interrupted after reading the files and before its commit
/// (a failing statement here; a crash rolls back the same way) has already
/// listed what it could not read: the previous rows keep answering and
/// nothing reads as current, though the database it rolled back to is
/// ready and the list was empty.
#[test]
fn a_rebuild_interrupted_before_its_commit_keeps_what_it_skipped_listed() {
    let (_tmp, lib) = setup();
    lib.write(|tx| Index::repair_pending(tx)).unwrap();
    assert!(pending(&lib).is_current());
    let meeting_json = lib.meeting_dir(&mid(M2)).unwrap();
    let meeting_json = meeting_json.join(paths::MEETING_JSON);
    let person_md = lib.person_dir(&pid(CAIO)).join(paths::PERSON_MD);
    let saved_meeting = fs::read(&meeting_json).unwrap();
    let saved_person = fs::read(&person_md).unwrap();
    fs::write(&meeting_json, "{ not json").unwrap();
    fs::write(&person_md, "not a person").unwrap();
    let conn = Connection::open(Index::path(lib.root())).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER interrupt BEFORE INSERT ON meta WHEN NEW.key = 'complete'
         BEGIN SELECT RAISE(ABORT, 'interrupted'); END",
    )
    .unwrap();

    // `broscribe index rebuild`.
    let err = lib.write(|tx| Index::rebuild(tx)).unwrap_err();
    assert_eq!(err.code(), "internal");
    let broken = |lib: &Library| {
        let health = pending(lib);
        assert_eq!(health.database, IndexDatabase::Ready);
        assert!(health.rebuild && !health.is_current(), "{health:?}");
        assert_eq!(health.people, BTreeSet::from([pid(CAIO)]));
    };
    broken(&lib);
    assert_eq!(reader(&lib).meeting_ids().unwrap(), vec![mid(M2), mid(M1)]);

    conn.execute_batch("DROP TRIGGER interrupt").unwrap();
    drop(conn);
    let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
    let rebuilt = report.rebuilt.unwrap();
    assert_eq!((rebuilt.meetings, rebuilt.skipped.len()), (1, 2));
    broken(&lib);
    assert_eq!(reader(&lib).meeting_ids().unwrap(), vec![mid(M1)]);

    fs::write(&meeting_json, &saved_meeting).unwrap();
    fs::write(&person_md, &saved_person).unwrap();
    let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
    assert!(report.rebuilt.is_some_and(|r| r.skipped.is_empty()));
    assert!(pending(&lib).is_current());
    assert_eq!(reader(&lib).meeting_ids().unwrap(), vec![mid(M2), mid(M1)]);
}

/// A repair by ID cannot reach a duplicate of another meeting's ID, nor an
/// unreadable folder (here one whose name ends with no ID): the rebuild
/// that skipped it keeps asking for a rebuild (each repair rebuilds and
/// skips it again) while what it read answers searches, until it is gone.
#[test]
fn a_rebuild_keeps_asking_for_itself_while_it_skips_a_meeting_folder() {
    let (_tmp, lib) = setup();
    lib.write(|tx| Index::repair_pending(tx)).unwrap();
    let dir1 = lib.meeting_dir(&mid(M1)).unwrap();
    let name = dir1.file_name().unwrap().to_str().unwrap().to_owned();
    let duplicate = fs::read(dir1.join(paths::MEETING_JSON)).unwrap();
    for (extra, json) in [
        (dir1.with_file_name(format!("{name} copy")), duplicate),
        (dir1.with_file_name("notes"), b"{ not json".to_vec()),
    ] {
        fs::create_dir(&extra).unwrap();
        fs::write(extra.join(paths::MEETING_JSON), json).unwrap();
        let check = |rebuilt: RebuildReport| {
            assert_eq!(rebuilt.meetings, 2);
            assert_eq!(rebuilt.skipped.len(), 1);
            assert_eq!(rebuilt.skipped[0].path, extra.join(paths::MEETING_JSON));
            let health = pending(&lib);
            assert_eq!(health.database, IndexDatabase::Ready);
            assert!(health.rebuild && !health.is_current(), "{health:?}");
            assert!(health.meetings.is_empty() && health.people.is_empty());
            assert_eq!(reader(&lib).meeting_ids().unwrap(), vec![mid(M2), mid(M1)]);
        };
        // `broscribe index rebuild`, then the repairs that follow.
        check(rebuild(&lib));
        for _ in 0..2 {
            let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
            check(report.rebuilt.expect("only a rebuild reads the folder"));
        }

        fs::remove_dir_all(&extra).unwrap();
        let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
        assert!(report.rebuilt.is_some_and(|r| r.skipped.is_empty()));
        assert!(pending(&lib).is_current());
    }
}

/// Every file and folder under `root`, sorted: what a read must leave as
/// it found it.
fn tree(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path.clone());
            }
            out.push(path);
        }
    }
    out.sort();
    out
}

/// A library with nothing to index needs no database; once it holds
/// meetings or people, a deleted, recreated-incomplete or foreign-layout
/// database is never current, even with nothing listed as pending, and the
/// repair rebuilds it so search finds the meetings again. Reading the
/// verdict without a database creates nothing.
#[test]
fn health_needs_a_database_that_answers_searches() {
    let tmp = tempfile::tempdir().unwrap();
    let index_dir = tmp.path().join(paths::INDEX_DIR);
    // Creates the library's folders (`.index/` among them), no database.
    let lib = Library::open(tmp.path()).unwrap();
    let before = tree(tmp.path());
    let empty = pending(&lib);
    assert_eq!(empty.database, IndexDatabase::NotNeeded);
    assert!(empty.is_current(), "{empty:?}");
    assert_eq!(tree(tmp.path()), before, "the verdict creates nothing");
    assert!(!Index::path(tmp.path()).exists());
    let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
    assert_eq!(report, RepairReport::default());
    assert_eq!(pending(&lib).database, IndexDatabase::Ready);

    // People alone need an index too.
    fs::remove_dir_all(&index_dir).unwrap();
    lib.write(|tx| tx.save_person(&people()[0])).unwrap();
    fs::remove_dir_all(&index_dir).unwrap();
    assert_eq!(pending(&lib).database, IndexDatabase::Missing);
    assert!(!index_dir.exists());

    let (_tmp, lib) = setup();
    lib.write(|tx| Index::repair_pending(tx)).unwrap();
    let healthy = pending(&lib);
    assert_eq!(healthy.database, IndexDatabase::Ready);
    assert!(healthy.is_current(), "{healthy:?}");
    let index_dir = lib.root().join(paths::INDEX_DIR);
    let budget = |lib: &Library| reader(lib).search(&text("budget")).unwrap().len();
    assert_eq!(budget(&lib), 3);

    type Break = fn(&Library);
    let breaks: [(IndexDatabase, Break); 4] = [
        // Deleted: nothing lists the meetings any more.
        (IndexDatabase::Missing, |_| {}),
        // Recreated by any later write, before a rebuild.
        (IndexDatabase::Incomplete, |lib| {
            lib.write(|_| Ok(())).unwrap();
        }),
        // Recreated without a schema.
        (IndexDatabase::Incomplete, |lib| {
            Connection::open(Index::path(lib.root()))
                .unwrap()
                .execute_batch("PRAGMA user_version = 1")
                .unwrap();
        }),
        // Another version's layout.
        (IndexDatabase::Incompatible, |lib| {
            lib.write(|tx| Index::rebuild(tx).map(|_| ())).unwrap();
            Connection::open(Index::path(lib.root()))
                .unwrap()
                .execute(
                    "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
                    [],
                )
                .unwrap();
        }),
    ];
    for (state, make) in breaks {
        fs::remove_dir_all(&index_dir).unwrap();
        fs::create_dir(&index_dir).unwrap();
        make(&lib);
        let before = tree(lib.root());
        let broken = pending(&lib);
        assert_eq!(broken.database, state);
        assert!(broken.rebuild && !broken.is_current(), "{broken:?}");
        assert!(broken.meetings.is_empty() && broken.people.is_empty());
        if state == IndexDatabase::Missing {
            assert_eq!(tree(lib.root()), before, "the verdict creates nothing");
            assert!(!Index::path(lib.root()).exists());
        }
        assert!(Index::open_readonly(lib.root()).is_err());

        let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
        assert_eq!(report.rebuilt.map(|r| (r.meetings, r.people)), Some((2, 3)));
        let repaired = pending(&lib);
        assert_eq!(repaired.database, IndexDatabase::Ready);
        assert!(repaired.is_current(), "{repaired:?}");
        assert_eq!(budget(&lib), 3);
    }
}

#[test]
fn multi_statement_reads_see_one_snapshot_while_a_writer_commits() {
    let (_tmp, lib) = setup();
    let mut writer = index_incrementally(&lib);
    let r = reader(&lib);
    let mut renamed = people().remove(0);
    renamed.name = "Ana Snapshot".into();
    let (before, during) = r
        .read_snapshot(|| {
            let before = r.list_people()?;
            lib.write(|tx| {
                tx.save_person(&renamed)?;
                writer.upsert_person(tx, &renamed)
            })?;
            Ok((before, r.list_people()?))
        })
        .unwrap();
    assert_eq!(before, during);
    assert!(
        r.list_people()
            .unwrap()
            .iter()
            .any(|p| p.name == "Ana Snapshot")
    );
}

/// The library watcher reports `broscribe.sqlite` and its `-wal` file as
/// index changes. A reader that checkpointed and deleted the WAL when it
/// closed would report a change of its own after every read.
#[test]
fn closing_a_reader_neither_checkpoints_nor_removes_the_wal() {
    let (_tmp, lib) = setup();
    // The writer closes last: it checkpoints and removes its WAL.
    drop(index_incrementally(&lib));
    let db_path = Index::path(lib.root());
    let wal = PathBuf::from(format!("{}-wal", db_path.display()));
    let files = || {
        let mut v: Vec<(String, u64)> = fs::read_dir(lib.root().join(paths::INDEX_DIR))
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().unwrap().len(),
                )
            })
            .collect();
        v.sort();
        (v, fs::read(&db_path).unwrap())
    };
    let read = || assert_eq!(reader(&lib).meeting_ids().unwrap().len(), 2);
    read();
    assert!(wal.exists(), "a closing reader must leave the WAL in place");
    let after_first = files();
    read();
    read();
    assert_eq!(files(), after_first);
}

#[test]
fn metadata_only_upserts_keep_the_transcript_rows() {
    let (_tmp, lib) = setup();
    let mut idx = index_incrementally(&lib);
    // Counts the utterance and window rows this connection writes.
    idx.conn
        .execute_batch(
            "CREATE TEMP TABLE transcript_writes(n INTEGER);
             CREATE TEMP TRIGGER utterance_writes AFTER INSERT ON main.utterances
             BEGIN INSERT INTO transcript_writes VALUES (1); END;
             CREATE TEMP TRIGGER window_writes AFTER INSERT ON main.windows
             BEGIN INSERT INTO transcript_writes VALUES (1); END;",
        )
        .unwrap();
    let writes = |idx: &Index| -> i64 {
        idx.conn
            .query_row("SELECT count(*) FROM temp.transcript_writes", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    let mut m1 = lib.load_meeting(&mid(M1)).unwrap();
    let write = |idx: &mut Index, m: &Meeting| {
        lib.write(|tx| {
            tx.save_meeting(m)?;
            idx.upsert_meeting(tx, m)
        })
        .unwrap();
    };
    write(&mut idx, &m1);
    let before = writes(&idx);

    m1.title = Some("Roadmap retro".into());
    m1.tags.push("retro".into());
    write(&mut idx, &m1);
    assert_eq!(
        writes(&idx),
        before,
        "a title and tag change rewrites no transcript row"
    );
    assert_eq!(
        meeting_keys(&idx.search_meetings(&text("retro")).unwrap()),
        [M1]
    );
    assert_eq!(idx.search(&text("café")).unwrap().len(), 1);
    fts_integrity(&idx);

    m1.segments[0].text = "We should ship the tea feature this week.".into();
    write(&mut idx, &m1);
    assert!(
        writes(&idx) >= before + 4,
        "a text change rewrites the meeting's utterances"
    );
    assert!(idx.search(&text("café")).unwrap().is_empty());
    assert_eq!(idx.search(&text("tea")).unwrap().len(), 1);
    fts_integrity(&idx);

    let incremental = snapshot(&idx);
    drop(idx);
    rebuild(&lib);
    assert_eq!(snapshot(&reader(&lib)), incremental);
}

#[test]
fn unnamed_mic_speakers_show_their_label_in_search_and_actions() {
    let (_tmp, lib) = setup();
    let mut m2 = meetings().remove(1);
    let mut mic = speaker(4, None, Assignment::None, 20.0, None);
    mic.track = "mic".into();
    m2.speakers.push(mic);
    m2.segments
        .push(seg(2, Some(4), 6.0, "Microphone check from the room."));
    let mut m2 = with_enrichment(m2, "claude-cli", None, None, false, "2026-11-15T10:00:00Z");
    m2.enrichment.as_mut().unwrap().actions = vec![
        serde_json::from_value(serde_json::json!({
            "id": "a1",
            "text": "Send the slides",
            "owner_speaker": SpeakerId::new(4).unwrap(),
        }))
        .unwrap(),
    ];
    let mut idx = Index::open(lib.root()).unwrap();
    let lookup = lib.people_lookup().unwrap();
    for (setup, label) in [(None, "Unknown (mic)"), (Some("in_person"), "Speaker 4")] {
        m2.setup = setup.map(str::to_owned);
        lib.write(|tx| {
            tx.save_meeting(&m2)?;
            idx.upsert_meeting(tx, &m2)
        })
        .unwrap();
        assert_eq!(
            speaker_label(&m2, Some(SpeakerId::new(4).unwrap()), &lookup),
            label
        );
        for index in [&idx, &reader(&lib)] {
            let hits = index.search(&text("microphone")).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].speaker_name, label);
            let actions = index.list_actions(&ActionQuery::default()).unwrap();
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0].owner.as_deref(), Some(label));
        }
    }
}

#[test]
fn layout_6_upgrade_is_marked_for_rebuild_until_one_runs() {
    let (_tmp, lib) = setup();
    // An existing library indexed by a layout-6 build, with a mic speaker.
    let mut m2 = meetings().remove(1);
    let mut mic = speaker(4, None, Assignment::None, 20.0, None);
    mic.track = "mic".into();
    m2.speakers.push(mic);
    m2.segments
        .push(seg(2, Some(4), 6.0, "Microphone check from the room."));
    lib.write(|tx| tx.save_meeting(&m2)).unwrap();
    rebuild(&lib);
    downgrade_to_v6(&lib);
    assert!(pending(&lib).rebuild, "an older layout needs a rebuild");

    // A reader upgrades it in place. Its old rows still say `Speaker 4`,
    // and the index says they are not current.
    let idx = reader(&lib);
    assert_eq!(idx.version, INDEX_SCHEMA_VERSION);
    assert_eq!(
        idx.search(&text("microphone")).unwrap()[0].speaker_name,
        "Speaker 4"
    );
    drop(idx);
    assert!(pending(&lib).rebuild, "upgraded rows are not current");
    // Indexing another meeting does not make the old rows current.
    lib.write(|tx| Index::open(tx.root())?.upsert_meeting(tx, &meetings()[0]))
        .unwrap();
    assert!(pending(&lib).rebuild);

    let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
    assert!(report.rebuilt.is_some());
    assert!(pending(&lib).is_current());
    assert_eq!(
        reader(&lib).search(&text("microphone")).unwrap()[0].speaker_name,
        "Unknown (mic)"
    );
}

mod paging;
mod relations;
mod templates;
