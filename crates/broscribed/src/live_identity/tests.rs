//! Pure and replay tests of the daemon's live speaker preview (plan D): the
//! turn table, decision fencing and persistence, the progress watchdog and
//! the worker → daemon chain. No capture, no real audio, no models.

use std::sync::Arc;
use std::time::Duration;

use broscribe_capture::journal::{JournalWriter, SessionMeta, Track as CaptureTrack};
use broscribe_core::live_identity::{LiveAction, STALE_REVISION, read_sidecar};
use broscribe_core::{Library, ops};
use broscribe_ipc::live::{
    LiveAttributeOutcome, LiveAttributeParams, LiveIdentitySettings, LiveSegment, LiveSpeaker,
    LiveTurnStatus, LiveWord, SpeakerSpan, SpeakerWindow, SuggestionSource, UnresolvedReason,
    VoiceSuggestion,
};
use broscribe_ipc::{Event, RequestId, Track};

use super::*;
use crate::bus::EventBus;
use crate::live::test_support::{TimingWorker, ps_str, worker as script_worker};

fn seg(start: f64, end: f64, words: &[(f64, f64)]) -> LiveSegment {
    LiveSegment {
        track: Track::System,
        start_s: start,
        end_s: end,
        text: "synthetic words".into(),
        speaker: LiveSpeaker::Others,
        words: words
            .iter()
            .map(|&(a, b)| LiveWord {
                text: "w".into(),
                start_s: a,
                end_s: b,
            })
            .collect(),
    }
}

struct Span(f64, f64, u32, bool);

fn window(
    epoch: u64,
    from: f64,
    to: f64,
    spans: &[Span],
    voices: &[(u32, Option<&str>)],
    evidence: &str,
) -> SpeakerWindow {
    SpeakerWindow {
        epoch,
        track: Track::System,
        from_sample: sample(from),
        to_sample: sample(to),
        spans: spans
            .iter()
            .map(|s| SpeakerSpan {
                from_sample: sample(s.0),
                to_sample: sample(s.1),
                voice: s.2,
                overlap: s.3,
                from: None,
                to: None,
            })
            .collect(),
        voices: voices
            .iter()
            .map(|&(voice, person)| VoiceSuggestion {
                voice,
                clean_s: 2.0,
                person_id: person.map(str::to_owned),
                source: person.map(|_| SuggestionSource::Library),
                uncalibrated: false,
            })
            .collect(),
        evidence: evidence.into(),
        elapsed_ms: 60,
        behind: false,
    }
}

fn names(id: &str) -> Option<String> {
    match id {
        "p-ana" => Some("Ana".into()),
        "p-bia" => Some("Bia".into()),
        _ => None,
    }
}

fn params(
    turn: &broscribe_ipc::live::LiveTurn,
    action: LiveAction,
    person: Option<&str>,
) -> LiveAttributeParams {
    LiveAttributeParams {
        session_id: turn.session_id.clone(),
        request_id: RequestId::random(),
        turn_id: turn.turn_id.clone(),
        revision: turn.revision,
        epoch: turn.epoch,
        evidence: turn.evidence.clone(),
        action,
        person_ref: person.map(str::to_owned),
    }
}

#[test]
fn record_live_turns_revise_with_windows_and_decisions_stay_on_their_range() {
    let mut board = PreviewBoard::new("s1", LiveIdentitySettings::default());
    let first = board.ingest_segment(&seg(1.0, 4.0, &[(1.0, 2.0), (2.5, 4.0)]), &names);
    assert_eq!(first.turn_id, format!("system-{}", sample(1.0)));
    assert_eq!((first.status, first.revision), (LiveTurnStatus::Preview, 1));
    let changed = board.ingest_window(
        &window(
            1,
            0.0,
            10.0,
            &[Span(0.0, 10.0, 1, false)],
            &[(1, Some("p-ana"))],
            "e1",
        ),
        &names,
    );
    let turn = changed
        .into_iter()
        .find(|t| t.turn_id == first.turn_id)
        .unwrap();
    assert_eq!(turn.status, LiveTurnStatus::Suggested);
    assert_eq!(turn.voice, Some(1));
    assert_eq!(turn.epoch, 1);
    assert_eq!(turn.suggestion.as_ref().unwrap().person.name, "Ana");
    assert!(turn.revision > first.revision);
    // The same window again changes nothing (no spurious revisions).
    assert!(
        board
            .ingest_window(
                &window(
                    1,
                    0.0,
                    10.0,
                    &[Span(0.0, 10.0, 1, false)],
                    &[(1, Some("p-ana"))],
                    "e1"
                ),
                &names,
            )
            .is_empty()
    );
    // Fences.
    assert!(
        board
            .check(&params(&turn, LiveAction::Confirm, Some("p-ana")))
            .is_ok()
    );
    let mut stale = params(&turn, LiveAction::Confirm, Some("p-ana"));
    stale.revision -= 1;
    assert_eq!(board.check(&stale).unwrap_err().code(), STALE_REVISION);
    let mut wrong = params(&turn, LiveAction::Confirm, Some("p-ana"));
    wrong.evidence = "old".into();
    assert_eq!(board.check(&wrong).unwrap_err().code(), STALE_EVIDENCE);
    let mut gone = params(&turn, LiveAction::Confirm, Some("p-ana"));
    gone.turn_id = "system-1".into();
    assert_eq!(board.check(&gone).unwrap_err().code(), STALE_TURN);
    let mut other = params(&turn, LiveAction::Confirm, Some("p-ana"));
    other.session_id = "s2".into();
    assert_eq!(board.check(&other).unwrap_err().code(), "stale_session");

    // Name this turn as Bia: only this turn, never later speech of the voice.
    let named = board
        .apply(
            &turn.turn_id,
            Some(LivePerson {
                person_id: "p-bia".into(),
                name: "Bia".into(),
            }),
            &names,
        )
        .unwrap();
    assert_eq!(named.status, LiveTurnStatus::Named);
    assert!(named.revision > turn.revision);
    let later = board.ingest_segment(&seg(5.0, 8.0, &[]), &names);
    assert_eq!(later.status, LiveTurnStatus::Suggested);
    assert!(later.named.is_none());
    // Later windows keep the decision; the suggestion stays visible beside it.
    board.ingest_window(
        &window(
            1,
            5.0,
            15.0,
            &[Span(5.0, 15.0, 1, false)],
            &[(1, Some("p-ana"))],
            "e1",
        ),
        &names,
    );
    let kept = board.turn(&turn.turn_id).unwrap().clone();
    assert_eq!(kept.status, LiveTurnStatus::Named);
    assert_eq!(kept.named.as_ref().unwrap().person_id, "p-bia");
    // Clear returns to the model's output.
    let cleared = board.apply(&turn.turn_id, None, &names).unwrap();
    assert_eq!(cleared.status, LiveTurnStatus::Suggested);
    assert!(cleared.revision > kept.revision);
    // An older epoch's window is ignored.
    assert!(
        board
            .ingest_window(
                &window(0, 0.0, 10.0, &[Span(0.0, 10.0, 7, false)], &[], "e1"),
                &names
            )
            .is_empty()
    );
}

#[test]
fn record_live_mixed_overlapping_and_unsupported_turns_stay_unresolved() {
    let mut board = PreviewBoard::new("s1", LiveIdentitySettings::default());
    let mixed = board.ingest_segment(&seg(1.0, 7.0, &[(1.0, 2.0), (6.0, 7.0)]), &names);
    let overlapped = board.ingest_segment(&seg(12.0, 14.0, &[]), &names);
    let short = board.ingest_segment(&seg(16.0, 18.0, &[(16.0, 18.0)]), &names);
    board.ingest_window(
        &window(
            1,
            0.0,
            20.0,
            &[
                Span(0.0, 5.0, 1, false),
                Span(5.0, 10.0, 2, false),
                Span(11.0, 15.0, 1, false),
                Span(11.5, 14.5, 1, true),
                Span(16.0, 16.4, 2, false),
            ],
            &[(1, Some("p-ana")), (2, None)],
            "e1",
        ),
        &names,
    );
    let status = |id: &str| {
        let t = board.turn(id).unwrap();
        (t.status, t.unresolved, t.suggestion.is_some())
    };
    assert_eq!(
        status(&mixed.turn_id),
        (
            LiveTurnStatus::Unresolved,
            Some(UnresolvedReason::Mixed),
            false
        )
    );
    assert_eq!(
        status(&overlapped.turn_id),
        (
            LiveTurnStatus::Unresolved,
            Some(UnresolvedReason::Overlap),
            false
        )
    );
    assert_eq!(
        status(&short.turn_id),
        (
            LiveTurnStatus::Unresolved,
            Some(UnresolvedReason::Unsupported),
            false
        )
    );
}

#[test]
fn record_live_evidence_change_drops_suggestions_and_forgotten_names() {
    let mut board = PreviewBoard::new("s1", LiveIdentitySettings::default());
    let turn = board.ingest_segment(&seg(1.0, 4.0, &[]), &names);
    board.ingest_window(
        &window(
            1,
            0.0,
            10.0,
            &[Span(0.0, 10.0, 1, false)],
            &[(1, Some("p-ana"))],
            "e1",
        ),
        &names,
    );
    let named = board
        .apply(
            &turn.turn_id,
            Some(LivePerson {
                person_id: "p-gone".into(),
                name: "Gone".into(),
            }),
            &|id| {
                (id == "p-gone")
                    .then(|| "Gone".to_owned())
                    .or_else(|| names(id))
            },
        )
        .unwrap();
    assert_eq!(named.status, LiveTurnStatus::Named);
    // Forgetting changes the evidence: suggestions computed before go, and
    // a person that no longer exists is no longer shown.
    let changed = board.refresh_evidence("e2", &names);
    let after = changed.iter().find(|t| t.turn_id == turn.turn_id).unwrap();
    assert!(after.named.is_none());
    assert!(after.suggestion.is_none());
    assert_eq!(after.status, LiveTurnStatus::Voice);
    assert_eq!(after.evidence, "e2");
    assert!(board.refresh_evidence("e2", &names).is_empty());
}

#[test]
fn record_live_board_is_bounded() {
    let settings = LiveIdentitySettings::default();
    let mut board = PreviewBoard::new("s1", settings.clone());
    for i in 0..250u64 {
        let at = i as f64 * 2.0;
        board.ingest_segment(&seg(at, at + 1.5, &[]), &names);
    }
    assert_eq!(board.len(), settings.max_turns);
    for k in 0..100u64 {
        let to = 10.0 + k as f64 * 5.0;
        board.ingest_window(
            &window(
                1,
                to - 10.0,
                to,
                &[
                    Span(to - 10.0, to - 5.0, 1, false),
                    Span(to - 5.0, to, 2, false),
                ],
                &[(1, None), (2, None)],
                "e1",
            ),
            &names,
        );
    }
    // Spans of the last `retain_s` only (two per 5 s hop, plus clipping).
    assert!(board.span_count() <= 2 * (settings.retain_s / settings.hop_s) as usize + 4);
    // Turns beyond the horizon keep what they showed last.
    let oldest = board.snapshot().turns[0].clone();
    assert!(oldest.end_s < 400.0);
    assert_ne!(oldest.status, LiveTurnStatus::Preview);
}

// ---- decisions through the daemon path ----

struct Fixture {
    _tmp: tempfile::TempDir,
    lib: Library,
    journal_root: std::path::PathBuf,
    preview: Arc<LivePreview>,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path().join("library")).unwrap();
    let journal_root = tmp.path().join("journal");
    let mut journal =
        JournalWriter::create(&journal_root, "s1", SessionMeta::default(), 5.0, 1).unwrap();
    journal
        .begin_segment(CaptureTrack::System, 0, None, 48_000, 0)
        .unwrap();
    for second in 0..10u64 {
        journal
            .append(
                CaptureTrack::System,
                second * 1_000_000_000,
                &vec![0.0; 48_000],
            )
            .unwrap();
    }
    let dir = journal.dir().to_owned();
    let preview = LivePreview::new(
        "s1".into(),
        dir,
        LiveIdentitySettings::default(),
        EventBus::new(256),
    );
    preview.attach_library(Some(lib.clone()));
    preview.on_segment(&seg(1.0, 4.0, &[]));
    preview.on_segment(&seg(6.0, 8.0, &[]));
    Fixture {
        _tmp: tmp,
        lib,
        journal_root,
        preview,
    }
}

fn shown(f: &Fixture, index: usize) -> broscribe_ipc::live::LiveTurn {
    f.preview.snapshot().turns[index].clone()
}

fn attribute_ok(f: &Fixture, p: LiveAttributeParams) -> LiveAttributeOutcome {
    let value = attribute(&f.lib, &f.journal_root, &f.preview, p, || Ok(())).unwrap();
    serde_json::from_value(value).unwrap()
}

/// A preview segment of `track`.
fn seg_on(track: Track, start: f64, end: f64) -> LiveSegment {
    LiveSegment {
        track,
        ..seg(start, end, &[])
    }
}

/// `record.live` for "s1", as a client decodes it.
fn record_live(preview: &LivePreview) -> LiveSnapshot {
    serde_json::from_value(snapshot_for(Some(preview), "s1").unwrap()).unwrap()
}

fn turn_ids(snapshot: &LiveSnapshot) -> Vec<String> {
    snapshot.turns.iter().map(|t| t.turn_id.clone()).collect()
}

/// The two tracks close their speech regions independently, so the agent
/// publishes turns out of source-time order. `record.live` returns them in
/// the order the agent drops them at its bound, so a client seeded from it
/// (the panel, `record live --follow`) drops exactly the turn the agent
/// drops, and the turns it still shows can be named.
#[test]
fn record_live_snapshot_keeps_the_order_the_agent_evicts_in() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path().join("library")).unwrap();
    let journal_root = tmp.path().join("journal");
    let mut journal =
        JournalWriter::create(&journal_root, "s1", SessionMeta::default(), 5.0, 1).unwrap();
    for track in [CaptureTrack::Mic, CaptureTrack::System] {
        journal.begin_segment(track, 0, None, 48_000, 0).unwrap();
    }
    for second in 0..10u64 {
        for track in [CaptureTrack::Mic, CaptureTrack::System] {
            journal
                .append(track, second * 1_000_000_000, &vec![0.0; 48_000])
                .unwrap();
        }
    }
    let dir = journal.dir().to_owned();
    drop(journal);
    let preview = LivePreview::new(
        "s1".into(),
        dir,
        LiveIdentitySettings::default(),
        EventBus::new(1024),
    );
    preview.attach_library(Some(lib.clone()));
    let cap = LiveIdentitySettings::default().max_turns;
    assert_eq!(cap, 200);
    // Every system turn is published before the mic turn that started 10 s
    // earlier (the longer mic region closes later), up to the full bound.
    let mut published = Vec::with_capacity(cap);
    for k in 0..cap {
        let pair = (k / 2) as f64 * 20.0;
        let (track, start) = if k % 2 == 0 {
            (Track::System, pair + 10.0)
        } else {
            (Track::Mic, pair)
        };
        preview.on_segment(&seg_on(track, start, start + 1.5));
        published.push(format!("{}-{}", track.as_str(), sample(start)));
    }
    let before = record_live(&preview);
    assert_eq!(before.session_id, "s1");
    assert_eq!(turn_ids(&before)[..2], ["system-160000", "mic-0"]);
    assert_eq!(turn_ids(&before), published, "publication order, not time");

    // The next turn after the resync: the agent drops its first publication.
    let sub = preview.bus.subscribe();
    let next_start = (cap / 2) as f64 * 20.0 + 10.0;
    preview.on_segment(&seg_on(Track::System, next_start, next_start + 1.5));
    let Some(Event::LiveTurn(next)) = sub.try_next() else {
        panic!("the new turn is published");
    };
    assert_eq!(next.session_id, "s1");
    assert_eq!(next.revision, 1);
    let after = record_live(&preview);
    let mut client: std::collections::VecDeque<String> = turn_ids(&before).into();
    client.push_back(next.turn_id.clone());
    while client.len() > cap {
        client.pop_front();
    }
    assert_eq!(turn_ids(&after), Vec::from(client));
    assert!(!turn_ids(&after).contains(&"system-160000".to_owned()));
    assert_eq!(after.turns[0].turn_id, "mic-0", "the earliest line is kept");
    // Every kept turn is unchanged: revisions, epochs, sessions, evidence.
    assert_eq!(after.turns[..cap - 1], before.turns[1..]);
    assert_eq!(after.turns[cap - 1], *next);

    // The dropped turn is refused and creates nothing; the kept one is named.
    let err = attribute(
        &lib,
        &journal_root,
        &preview,
        params(&before.turns[0], LiveAction::Name, Some("Bia Example")),
        || Ok(()),
    )
    .unwrap_err();
    assert_eq!(err.code, STALE_TURN);
    assert!(lib.list_people().unwrap().is_empty());
    let mic = after.turns[0].clone();
    let named: LiveAttributeOutcome = serde_json::from_value(
        attribute(
            &lib,
            &journal_root,
            &preview,
            params(&mic, LiveAction::Name, Some("Ana Example")),
            || Ok(()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(named.turn.turn_id, "mic-0");
    assert_eq!(named.turn.status, LiveTurnStatus::Named);
    assert!(named.turn.revision > mic.revision);
    let decision = read_sidecar(lib.root(), "s1").unwrap().unwrap();
    let decision = decision.current("mic-0").unwrap();
    assert_eq!(decision.range.track, "mic");
    assert_eq!(decision.range.from_sample, 0);
    // Naming moves nothing: the agent still drops in publication order.
    let named_view = record_live(&preview);
    assert_eq!(turn_ids(&named_view), turn_ids(&after));
    assert_eq!(named_view.turns[0], named.turn);
    // The old revision and another session stay refused.
    let err = attribute(
        &lib,
        &journal_root,
        &preview,
        params(&mic, LiveAction::Name, Some("Someone Else")),
        || Ok(()),
    )
    .unwrap_err();
    assert!(matches!(
        err.code.as_str(),
        "stale_revision" | "stale_evidence"
    ));
    assert_eq!(
        snapshot_for(Some(&preview), "s2").unwrap_err().code,
        "stale_session"
    );
}

#[test]
fn record_live_name_creates_a_name_only_person_and_persists_the_exact_range() {
    let f = fixture();
    let turn = shown(&f, 0);
    assert!(
        !turn.evidence.is_empty(),
        "stamped with the library evidence"
    );
    let out = attribute_ok(
        &f,
        params(&turn, LiveAction::Name, Some("New Acquaintance")),
    );
    let created = out.created_person_id.clone().unwrap();
    let person = ops::find_person(&f.lib, "New Acquaintance")
        .unwrap()
        .unwrap();
    assert_eq!(person.id.as_str(), created);
    assert_eq!(out.turn.status, LiveTurnStatus::Named);
    // No voiceprint is ever written by a live decision.
    assert!(
        f.lib
            .load_voiceprints(&person.id)
            .unwrap()
            .embeddings
            .is_empty()
    );
    let sidecar = read_sidecar(f.lib.root(), "s1").unwrap().unwrap();
    let decision = sidecar.current(&turn.turn_id).unwrap();
    assert_eq!(decision.action, LiveAction::Name);
    assert_eq!(decision.range.from_sample, sample(1.0));
    assert_eq!(decision.range.to_sample, sample(4.0));
    assert_eq!(decision.range.from.unwrap().seq, 0);
    assert_eq!(decision.range.from.unwrap().frame, 48_000);
    assert!(decision.range.to.is_some());
    // The other turn is untouched.
    assert!(shown(&f, 1).named.is_none());
    assert!(
        read_sidecar(f.lib.root(), "s1")
            .unwrap()
            .unwrap()
            .current(&shown(&f, 1).turn_id)
            .is_none()
    );

    // A retried click on the earlier revision creates nothing more.
    let before = f.lib.list_people().unwrap().len();
    let err = attribute(
        &f.lib,
        &f.journal_root,
        &f.preview,
        params(&turn, LiveAction::Name, Some("Someone Else")),
        || Ok(()),
    )
    .unwrap_err();
    assert!(matches!(
        err.code.as_str(),
        "stale_revision" | "stale_evidence"
    ));
    assert_eq!(f.lib.list_people().unwrap().len(), before);

    // Clear on the current revision: durable, returns to model output.
    let current = out.turn.clone();
    let cleared = attribute_ok(&f, params(&current, LiveAction::Clear, None));
    assert!(cleared.turn.named.is_none());
    let sidecar = read_sidecar(f.lib.root(), "s1").unwrap().unwrap();
    assert_eq!(
        sidecar.current(&turn.turn_id).unwrap().action,
        LiveAction::Clear
    );
    assert!(sidecar.current(&turn.turn_id).unwrap().decision > decision.decision);
}

#[test]
fn record_live_stale_session_or_refusals_create_nothing() {
    let f = fixture();
    let turn = shown(&f, 0);
    let err = attribute(
        &f.lib,
        &f.journal_root,
        &f.preview,
        params(&turn, LiveAction::Name, Some("Ghost Person")),
        || Err::<(), _>(lifecycle::stale_session("s1")),
    )
    .unwrap_err();
    assert_eq!(err.code, "stale_session");
    assert!(ops::find_person(&f.lib, "Ghost Person").unwrap().is_none());
    // Confirm without a suggestion, and Clear without a name, are refused.
    for (action, person) in [
        (LiveAction::Confirm, Some("p-ana")),
        (LiveAction::Clear, None),
        (LiveAction::Name, None),
    ] {
        let err = attribute(
            &f.lib,
            &f.journal_root,
            &f.preview,
            params(&turn, action, person),
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(err.code, "invalid_input");
    }
    // A discarded session is stale.
    lifecycle::write_discard_intent(&f.journal_root, "s1").unwrap();
    let err = attribute(
        &f.lib,
        &f.journal_root,
        &f.preview,
        params(&turn, LiveAction::Name, Some("Ghost Person")),
        || Ok(()),
    )
    .unwrap_err();
    assert_eq!(err.code, "stale_session");
    assert!(ops::find_person(&f.lib, "Ghost Person").unwrap().is_none());
    assert_eq!(read_sidecar(f.lib.root(), "s1").unwrap(), None);
}

#[test]
fn record_live_forget_person_unnames_live_turns() {
    let f = fixture();
    let turn = shown(&f, 0);
    let out = attribute_ok(&f, params(&turn, LiveAction::Name, Some("Short Lived")));
    let id = out.created_person_id.unwrap();
    ops::with_index(&f.lib, |tx, index| ops::forget_person(tx, index, &id)).unwrap();
    let sidecar = read_sidecar(f.lib.root(), "s1").unwrap().unwrap();
    assert!(sidecar.current(&turn.turn_id).unwrap().forgotten);
    // The next evidence check (a window, a picker) drops the name.
    let fingerprint = evidence_fingerprint(&f.lib).unwrap();
    let lib = f.lib.clone();
    let changed =
        lock(&f.preview.board).refresh_evidence(&fingerprint, &|i: &str| name_of(Some(&lib), i));
    let after = changed.iter().find(|t| t.turn_id == turn.turn_id).unwrap();
    assert!(after.named.is_none());
}

#[test]
fn record_live_cached_person_names_show_a_rename_on_the_next_segment() {
    let f = fixture();
    let (mut person, _) = ops::resolve_or_new_person(&f.lib, "Ana Example").unwrap();
    let save = |person: &broscribe_core::schema::Person| {
        ops::with_index(&f.lib, |tx, index| {
            tx.save_person(person)?;
            index.upsert_person(tx, person)
        })
        .unwrap();
    };
    save(&person);
    let id = person.id.as_str().to_owned();
    // A window suggests the person for the one voice of 0–10 s.
    f.preview.on_window(&window(
        1,
        0.0,
        10.0,
        &[Span(0.0, 10.0, 0, false)],
        &[(0, Some(id.as_str()))],
        "e1",
    ));
    let suggested_at = |start: f64| {
        f.preview
            .snapshot()
            .turns
            .into_iter()
            .find(|t| t.from_sample == sample(start))
            .and_then(|t| t.suggestion)
            .map(|s| s.person.name)
    };
    f.preview.on_segment(&seg(2.0, 3.0, &[]));
    assert_eq!(suggested_at(2.0).as_deref(), Some("Ana Example"));
    f.preview.on_segment(&seg(3.5, 4.5, &[]));
    assert_eq!(suggested_at(3.5).as_deref(), Some("Ana Example"));
    // Renamed elsewhere: the next segment shows the new name, never the
    // cached one.
    person.name = "Ana Renamed Example".into();
    save(&person);
    f.preview.on_segment(&seg(4.6, 5.6, &[]));
    assert_eq!(suggested_at(4.6).as_deref(), Some("Ana Renamed Example"));
    // Gone from the library: no name to show, so no suggestion.
    std::fs::remove_dir_all(f.lib.person_dir(&person.id)).unwrap();
    f.preview.on_segment(&seg(8.5, 9.5, &[]));
    assert_eq!(suggested_at(8.5), None);
}

// ---- supervision ----

/// Fast but valid settings: the shortest hop the driver accepts.
fn quick_settings() -> LiveIdentitySettings {
    LiveIdentitySettings {
        window_s: 0.5,
        hop_s: broscribe_ipc::live::MIN_HOP_S,
        retain_s: 1.0,
        progress_timeout_s: 0.4,
        ..Default::default()
    }
}

/// [`quick_settings`] with a 0.8 s progress limit, eight times the timing
/// fixture's 0.1 s line period, so scheduling delays under load are not
/// silence. The watchdog checks once per limit: a worker whose lines did
/// not count as signs of life would be killed within 1.6 s.
fn patient_settings() -> LiveIdentitySettings {
    LiveIdentitySettings {
        progress_timeout_s: 0.8,
        ..quick_settings()
    }
}

/// Every `live_identity` state `supervise` publishes for `worker` within
/// `run_for`, the final one after the session ended included.
async fn identity_states(
    worker: WorkerCommand,
    settings: LiveIdentitySettings,
    run_for: Duration,
    fixture: Option<TimingWorker>,
) -> Vec<LiveIdentityState> {
    let tmp = tempfile::tempdir().unwrap();
    let bus = EventBus::new(1024);
    let sub = bus.subscribe();
    let preview = LivePreview::new("s1".into(), tmp.path().join("journal"), settings, bus);
    let stop = Arc::new(Shutdown::new());
    let supervisor = Arc::clone(&preview).supervise(
        WorkerCommand {
            data_dir_override: Some(tmp.path().join("data")),
            ..worker
        },
        tmp.path().join("library"),
        tmp.path().join("data"),
        Arc::clone(&stop),
        Arc::new(
            broscribe_core::maintenance::ModeLease::acquire(
                &tmp.path().join("data"),
                broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
            )
            .unwrap(),
        ),
    );
    let task = tokio::spawn(async move {
        supervisor.await;
        // The supervisor has closed stdin and reaped/joined every child.
        drop(fixture);
    });
    tokio::time::sleep(run_for).await;
    stop.trigger();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let mut states = Vec::new();
    while let Some(event) = sub.try_next() {
        if let Event::LiveIdentity {
            session_id,
            identity,
        } = event
        {
            assert_eq!(session_id, "s1");
            states.push(identity);
        }
    }
    states
}

async fn identity_epochs(
    worker: WorkerCommand,
    settings: LiveIdentitySettings,
    run_for: Duration,
    fixture: Option<TimingWorker>,
) -> Vec<u64> {
    let mut ready_epochs = Vec::new();
    for state in identity_states(worker, settings, run_for, fixture).await {
        if state.is_available() && !ready_epochs.contains(&state.epoch) {
            ready_epochs.push(state.epoch);
        }
    }
    ready_epochs
}

fn speakers_line(event: &SpeakersEvent) -> String {
    event.to_line().unwrap()
}

fn ready_line() -> String {
    speakers_line(&SpeakersEvent::Ready {
        epoch: 0,
        diarizer: "d".into(),
        embedder: "e".into(),
    })
}

fn idle_heartbeat_line() -> String {
    speakers_line(&SpeakersEvent::Heartbeat {
        pending_s: 0.0,
        busy_ms: 0,
    })
}

#[tokio::test]
async fn record_live_watchdog_restarts_a_worker_without_useful_progress() {
    // Ready, then heartbeats with audio pending but no window ever.
    let fixture = TimingWorker::new();
    let worker = fixture.command(&[
        "speakers",
        "0",
        "",
        &ready_line(),
        &speakers_line(&SpeakersEvent::Heartbeat {
            pending_s: 10.0,
            busy_ms: 5,
        }),
    ]);
    let epochs = identity_epochs(
        worker,
        quick_settings(),
        Duration::from_secs(4),
        Some(fixture),
    )
    .await;
    assert!(epochs.len() >= 2, "restarted: {epochs:?}");
    assert_eq!(epochs[0], 1);
}

#[tokio::test]
async fn record_live_idle_heartbeats_are_not_killed() {
    // Ready, then idle heartbeats for well past the 1.6 s within which a
    // silent worker is killed: nothing pending is not a stall. Compile
    // before supervision; interpreter cold startup is not worker silence.
    let fixture = TimingWorker::new();
    let worker = fixture.command(&["speakers", "0", "", &ready_line(), &idle_heartbeat_line()]);
    let states = identity_states(
        worker,
        patient_settings(),
        Duration::from_secs(3),
        Some(fixture),
    )
    .await;
    assert!(states.iter().any(|s| s.is_available()), "{states:?}");
    assert!(
        states
            .iter()
            .all(|s| s.epoch == 1 && !matches!(s.status, LiveIdentityStatus::Restarting { .. })),
        "{states:?}"
    );
}

#[tokio::test]
async fn record_live_watchdog_restarts_a_worker_stuck_in_its_first_window() {
    // Ready, then heartbeats from a worker whose first window never ends:
    // nothing pending (it was read before the window began), busy growing.
    let fixture = TimingWorker::new();
    let worker = fixture.command(&["speakers-busy", "0", "", &ready_line(), ""]);
    let epochs = identity_epochs(
        worker,
        quick_settings(),
        Duration::from_secs(4),
        Some(fixture),
    )
    .await;
    assert!(epochs.len() >= 2, "restarted: {epochs:?}");
}

#[tokio::test]
async fn record_live_disabled_identity_keeps_track_only_preview() {
    let settings = LiveIdentitySettings {
        enabled: false,
        ..Default::default()
    };
    let epochs = identity_epochs(
        script_worker("exit 1", "exit 1"),
        settings,
        Duration::from_millis(300),
        None,
    )
    .await;
    assert!(epochs.is_empty());
}

#[tokio::test]
async fn record_live_speakers_loading_is_reported_until_ready() {
    let loading = speakers_line(&SpeakersEvent::Loading {
        diarizer: "d".into(),
        embedder: "e".into(),
        elapsed_ms: 1,
    });
    // Loading lines every 0.1 s for 2 s, past the 1.6 s within which a
    // silent worker is killed: signs of life, so it is not killed. The
    // window leaves room for slow hosted runners, where the twenty lines
    // took 3.5 s; after ready the fixture sends heartbeats every 50 ms, so
    // a longer window cannot show a restart.
    let fixture = TimingWorker::new();
    let worker = fixture.command(&[
        "speakers",
        "20",
        &loading,
        &ready_line(),
        &idle_heartbeat_line(),
    ]);
    let states = identity_states(
        worker,
        patient_settings(),
        Duration::from_millis(8000),
        Some(fixture),
    )
    .await;
    let ready = states
        .iter()
        .position(|s| s.status == LiveIdentityStatus::Ready)
        .unwrap_or_else(|| panic!("never ready: {states:?}"));
    assert_eq!(
        states[0],
        LiveIdentityState {
            status: LiveIdentityStatus::Loading { elapsed_ms: 0 },
            epoch: 1
        }
    );
    assert!(
        states[..ready]
            .iter()
            .all(|s| s.epoch == 1 && matches!(s.status, LiveIdentityStatus::Loading { .. })),
        "{states:?}"
    );
    assert!(ready > 1, "worker-reported loading shown: {states:?}");
    assert_eq!(states[ready].epoch, 1);
    assert!(
        !states
            .iter()
            .any(|s| matches!(s.status, LiveIdentityStatus::Restarting { .. })),
        "{states:?}"
    );
    assert_eq!(states.last().unwrap().status, LiveIdentityStatus::Idle);
}

#[tokio::test]
async fn record_live_speakers_heartbeats_and_warnings_before_ready_are_no_sign_of_life() {
    let warning = speakers_line(&SpeakersEvent::Warning {
        message: "synthetic".into(),
    });
    let fixture = TimingWorker::new();
    let worker = fixture.command(&["speakers-unready", &idle_heartbeat_line(), &warning]);
    let states = identity_states(
        worker,
        quick_settings(),
        Duration::from_millis(1500),
        Some(fixture),
    )
    .await;
    assert!(
        states.contains(&LiveIdentityState {
            status: LiveIdentityStatus::Restarting {
                reason: WorkerProblem::Silent
            },
            epoch: 1
        }),
        "{states:?}"
    );
    assert!(
        !states.iter().any(LiveIdentityState::is_available),
        "{states:?}"
    );
}

/// The next `live_identity` state with status `want`.
async fn next_identity(
    sub: &mut crate::bus::Subscriber,
    want: LiveIdentityStatus,
) -> LiveIdentityState {
    let found = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = sub.next().await {
            if let Event::LiveIdentity {
                identity: state, ..
            } = event
                && state.status == want
            {
                return Some(state);
            }
        }
        None
    })
    .await;
    match found {
        Ok(Some(state)) => state,
        _ => panic!("no {want:?}"),
    }
}

/// `[live] identity` turned off mid-recording stops only the companion,
/// through end of stdin; the board keeps its turns and the user's decision,
/// and later turns are track-only. Turned on again, a fresh epoch starts.
#[tokio::test]
async fn record_live_identity_toggle_stops_only_the_companion_and_resumes_with_a_new_epoch() {
    let f = fixture();
    let data_dir = f.journal_root.with_file_name("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let marks = f.journal_root.with_file_name("marks");
    // Records each start and each end of stdin.
    let script = format!(
        "echo start >> '{marks}'; printf '%s\\n' '{ready}'; \
         (while true; do printf '%s\\n' '{beat}'; sleep 0.1; done) & beat=$!; \
         cat > /dev/null; kill $beat; echo eof >> '{marks}'",
        marks = marks.display(),
        ready = ready_line(),
        beat = idle_heartbeat_line(),
    );
    // One process drains the binary SpeakersStart stream asynchronously
    // while emitting NDJSON; EOF must be observed before Disabled is shown.
    let ps = format!(
        "fx_append {marks} ('start'+[char]10); fx_echo {ready}; \
         $buffer=New-Object byte[] 4096; $read=$fx_in.ReadAsync($buffer,0,$buffer.Length); \
         while($true){{\
         if($read.IsCompleted){{\
         if($read.GetAwaiter().GetResult() -eq 0){{break}}; \
         $read=$fx_in.ReadAsync($buffer,0,$buffer.Length)\
         }}; fx_echo {beat}; Start-Sleep -Milliseconds 100\
         }}; fx_append {marks} ('eof'+[char]10)",
        marks = ps_str(marks.to_str().unwrap()),
        ready = ps_str(&ready_line()),
        beat = ps_str(&idle_heartbeat_line()),
    );
    let named = attribute_ok(
        &f,
        params(&shown(&f, 0), LiveAction::Name, Some("Kept Name")),
    );
    let mut sub = f.preview.bus.subscribe();
    let stop = Arc::new(Shutdown::new());
    let task = tokio::spawn(
        Arc::clone(&f.preview).supervise(
            WorkerCommand {
                data_dir_override: Some(data_dir.clone()),
                ..script_worker(&script, &ps)
            },
            f.lib.root().to_owned(),
            data_dir.clone(),
            Arc::clone(&stop),
            Arc::new(
                broscribe_core::maintenance::ModeLease::acquire(
                    &data_dir,
                    broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                )
                .unwrap(),
            ),
        ),
    );
    assert_eq!(
        next_identity(&mut sub, LiveIdentityStatus::Ready)
            .await
            .epoch,
        1
    );

    Config::update(&data_dir, |c| {
        c.live.identity = Some(false);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        next_identity(&mut sub, LiveIdentityStatus::Disabled)
            .await
            .epoch,
        1
    );
    assert_eq!(std::fs::read_to_string(&marks).unwrap(), "start\neof\n");
    let snapshot = f.preview.snapshot();
    assert_eq!(snapshot.identity.status, LiveIdentityStatus::Disabled);
    assert_eq!(snapshot.turns.len(), 2);
    assert_eq!(snapshot.turns[0].named, named.turn.named);
    assert!(
        read_sidecar(f.lib.root(), "s1")
            .unwrap()
            .unwrap()
            .current(&named.turn.turn_id)
            .is_some()
    );
    f.preview.on_segment(&seg(12.0, 13.0, &[]));
    let snapshot = f.preview.snapshot();
    assert_eq!(snapshot.turns.len(), 3);
    assert_eq!(snapshot.turns[2].status, LiveTurnStatus::Preview);

    Config::update(&data_dir, |c| {
        c.live.identity = Some(true);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        next_identity(&mut sub, LiveIdentityStatus::Ready)
            .await
            .epoch,
        2
    );
    stop.trigger();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        f.preview.snapshot().identity.status,
        LiveIdentityStatus::Idle
    );
    assert_eq!(
        std::fs::read_to_string(&marks).unwrap(),
        "start\neof\nstart\neof\n"
    );
    assert_eq!(
        f.preview.snapshot().turns[0].named,
        named.turn.named,
        "the decision outlives the companion"
    );
}

/// The daemon validates hops with the one minimum the live-speakers driver
/// also enforces (both through `LiveIdentitySettings::validate`): a shorter
/// one is reported as invalid settings once and no worker is started (and
/// restarted) only to refuse it.
#[tokio::test]
async fn record_live_hop_below_the_driver_minimum_is_refused_before_any_worker() {
    let tmp = tempfile::tempdir().unwrap();
    let started = tmp.path().join("started");
    let bus = EventBus::new(64);
    let mut sub = bus.subscribe();
    let settings = LiveIdentitySettings {
        hop_s: 0.2,
        ..quick_settings()
    };
    let preview = LivePreview::new("s1".into(), tmp.path().join("journal"), settings, bus);
    let stop = Arc::new(Shutdown::new());
    let worker = script_worker(
        &format!("touch '{}'; exit 1", started.display()),
        &format!("fx_append {} ''; exit 1", ps_str(started.to_str().unwrap())),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        Arc::clone(&preview).supervise(
            WorkerCommand {
                data_dir_override: Some(tmp.path().join("data")),
                ..worker
            },
            tmp.path().join("library"),
            tmp.path().join("data"),
            Arc::clone(&stop),
            Arc::new(
                broscribe_core::maintenance::ModeLease::acquire(
                    &tmp.path().join("data"),
                    broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                )
                .unwrap(),
            ),
        ),
    )
    .await
    .expect("invalid settings end supervision at once");
    let state = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(event) = sub.next().await {
            if let Event::LiveIdentity {
                session_id,
                identity,
            } = event
            {
                assert_eq!(session_id, "s1");
                return identity;
            }
        }
        panic!("no identity state published");
    })
    .await
    .unwrap();
    assert_eq!(
        state,
        LiveIdentityState {
            status: LiveIdentityStatus::Unavailable {
                reason: WorkerProblem::InvalidSettings
            },
            epoch: 0
        }
    );
    assert!(!started.exists(), "no worker for settings it refuses");
}

// ---- worker → daemon chain ----

/// Labels 0.25 s blocks by dominant tone, like a window-local diarizer.
struct ToneDiarizer;

impl broscribe_models::Diarizer for ToneDiarizer {
    fn diarize(
        &self,
        samples: &[f32],
    ) -> broscribe_core::Result<Vec<broscribe_models::DiarSegment>> {
        let mut labels: Vec<u64> = Vec::new();
        let mut out: Vec<broscribe_models::DiarSegment> = Vec::new();
        for (i, chunk) in samples.chunks(4_000).enumerate() {
            let rms = (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt();
            if rms < 1e-3 || chunk.len() < 2_000 {
                continue;
            }
            let bucket = (broscribe_models::fakes::ToneEmbedder::dominant_frequency(chunk) / 20.0)
                .round() as u64;
            let speaker = labels.iter().position(|b| *b == bucket).unwrap_or_else(|| {
                labels.push(bucket);
                labels.len() - 1
            }) as u32;
            let start = (i * 4_000) as f64 / 16_000.0;
            let end = start + chunk.len() as f64 / 16_000.0;
            match out.last_mut() {
                Some(last) if last.speaker == speaker && (last.end_s - start).abs() < 1e-9 => {
                    last.end_s = end;
                }
                _ => out.push(broscribe_models::DiarSegment {
                    start_s: start,
                    end_s: end,
                    speaker,
                }),
            }
        }
        Ok(out)
    }
}

struct FixedEvidence(Arc<broscribe_pipeline::live_identity::Evidence>);

impl broscribe_pipeline::live_identity::EvidenceSource for FixedEvidence {
    fn current(
        &mut self,
    ) -> broscribe_core::Result<Arc<broscribe_pipeline::live_identity::Evidence>> {
        Ok(Arc::clone(&self.0))
    }
}

#[test]
fn record_live_worker_windows_drive_board_turns() {
    use broscribe_ipc::live::SpeakersStart;
    use broscribe_pipeline::live_identity::{Driver, Evidence, SpeakerModels};
    let tmp = tempfile::tempdir().unwrap();
    let mut journal =
        JournalWriter::create(tmp.path(), "s1", SessionMeta::default(), 5.0, 1).unwrap();
    journal
        .begin_segment(CaptureTrack::System, 0, None, 48_000, 0)
        .unwrap();
    let mut driver = Driver::new(&SpeakersStart {
        session_id: "s1".into(),
        journal_dir: journal.dir().to_owned(),
        epoch: 1,
        settings: LiveIdentitySettings::default(),
    })
    .unwrap();
    let embedder = broscribe_models::fakes::ToneEmbedder::new("tone", 16);
    let models = SpeakerModels {
        diarizer: &ToneDiarizer,
        embedder: &embedder,
    };
    let mut evidence = FixedEvidence(Arc::new(Evidence {
        fingerprint: "e1".into(),
        ..Default::default()
    }));
    let mut board = PreviewBoard::new("s1", LiveIdentitySettings::default());
    // The ASR segment arrives before speaker analysis covers it.
    let turn = board.ingest_segment(&seg(2.0, 4.0, &[(2.0, 3.0), (3.1, 4.0)]), &names);
    assert_eq!(turn.status, LiveTurnStatus::Preview);
    let tone: Vec<f32> = (0..48_000)
        .map(|i| (0.3 * (2.0 * std::f64::consts::PI * 200.0 * i as f64 / 48_000.0).sin()) as f32)
        .collect();
    let mut revisions = Vec::new();
    for second in 0..15u64 {
        journal
            .append(CaptureTrack::System, second * 1_000_000_000, &tone)
            .unwrap();
        if (second + 1).is_multiple_of(5) {
            driver
                .step(&models, &mut evidence, &|_| {}, &mut |event| {
                    if let SpeakersEvent::Window(w) = &event {
                        for t in board.ingest_window(w, &names) {
                            revisions.push((t.turn_id, t.revision));
                        }
                    }
                    Ok(())
                })
                .unwrap();
        }
    }
    let after = board.turn(&turn.turn_id).unwrap();
    assert_eq!(after.status, LiveTurnStatus::Voice);
    assert_eq!(after.epoch, 1);
    assert_eq!(after.evidence, "e1");
    assert!(after.voice.is_some());
    // Revised once when covered, not on every later window of the voice.
    assert_eq!(
        revisions
            .iter()
            .filter(|(id, _)| *id == turn.turn_id)
            .count(),
        1
    );
}
