//! Synthetic libraries, journals and queue jobs only; invented names.

use std::collections::BTreeSet;

use chrono::DateTime;
use tempfile::TempDir;

use super::*;
use crate::index::SearchQuery;
use crate::queue::LeaseHandle;
use crate::schema::{
    Person, Recording, RecordingSource, RecordingState, Strength, TitleSource, Voiceprint,
    VoiceprintSource, encode_vector,
};

const MODEL: &str = "test-embedder";
const SESSION: &str = "s-20261116-100000-abcd";
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A process probe whose live PIDs the test controls.
struct Probe {
    me: u32,
    alive: std::sync::Mutex<BTreeSet<u32>>,
}

impl Probe {
    fn new() -> Self {
        Self {
            me: 4242,
            alive: std::sync::Mutex::new([4242].into()),
        }
    }
    fn kill(&self, pid: u32) {
        self.alive.lock().unwrap().remove(&pid);
    }
}

impl ProcessProbe for Probe {
    fn is_alive(&self, pid: u32) -> bool {
        self.alive.lock().unwrap().contains(&pid)
    }
    fn current_pid(&self) -> u32 {
        self.me
    }
}

struct Fx {
    tmp: TempDir,
    lib: Library,
    probe: Probe,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path().join("library")).unwrap();
        Self {
            tmp,
            lib,
            probe: Probe::new(),
        }
    }
    fn data_dir(&self) -> PathBuf {
        self.tmp.path().join("data")
    }
    fn journal_root(&self) -> PathBuf {
        journal_root(&self.data_dir())
    }
    fn cache(&self) -> PathBuf {
        self.tmp.path().join("cache")
    }
    fn opts(&self) -> DeleteOptions {
        DeleteOptions {
            journal_root: self.journal_root(),
            cache_dir: Some(self.cache()),
        }
    }
    fn queue(&self) -> Queue {
        Queue::open(self.lib.root()).unwrap()
    }
    /// A stopped journal of `session` (only the manifest fields lifecycle
    /// reads; the capture crate owns the rest of the format).
    fn journal(&self, session: &str) -> PathBuf {
        self.journal_in(session, "stopped", 4242)
    }
    fn journal_in(&self, session: &str, state: &str, owner_pid: u32) -> PathBuf {
        let dir = self.journal_root().join(session);
        fs::create_dir_all(dir.join("mic")).unwrap();
        let manifest = serde_json::json!({
            "session_id": session, "state": state, "owner_pid": owner_pid,
        });
        fs::write(dir.join(SESSION_MANIFEST), manifest.to_string()).unwrap();
        fs::write(dir.join("mic").join("000000.f32"), [0u8; 16]).unwrap();
        dir
    }
    fn save(&self, meeting: &Meeting) {
        self.lib
            .write(|tx| {
                if !tx.library().meeting_exists(&meeting.id)? {
                    tx.create_meeting_dir(meeting)?;
                }
                tx.save_meeting(meeting)?;
                Index::open(tx.root())?.upsert_meeting(tx, meeting)
            })
            .unwrap();
    }
    fn dir(&self, id: &MeetingId) -> PathBuf {
        self.lib.meeting_dir(id).unwrap()
    }
    fn person(&self, id: &str, name: &str, prints: Vec<Voiceprint>) -> PersonId {
        let pid = PersonId::parse(id).unwrap();
        self.lib
            .write(|tx| {
                tx.save_person(&Person::new(pid.clone(), name))?;
                tx.upsert_voiceprints(&pid, prints).map(|_| ())
            })
            .unwrap();
        pid
    }
    fn enqueue(&self, kind: &str, key: &str, input: serde_json::Value) -> JobId {
        let q = self.queue();
        self.lib
            .write(|tx| q.enqueue(tx, kind, key, input))
            .unwrap()
    }
    fn claim(&self, id: &JobId) -> LeaseHandle {
        self.queue()
            .claim(&self.lib, id, &self.probe)
            .unwrap()
            .unwrap()
    }
    fn search(&self, text: &str) -> Vec<MeetingId> {
        Index::open_readonly(self.lib.root())
            .unwrap()
            .search_meetings(&SearchQuery {
                text: Some(text.into()),
                ..SearchQuery::default()
            })
            .unwrap()
            .into_iter()
            .map(|h| h.meeting_id)
            .collect()
    }
}

fn at(s: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(s).unwrap()
}

fn mid(s: &str) -> MeetingId {
    MeetingId::parse(s).unwrap()
}

fn meeting(id: &str, handling: Handling, path: &str) -> Meeting {
    let mut m = Meeting::new(mid(id), Source::Import, at("2026-11-02T10:00:00Z"));
    m.title = Some("Synthetic planning".into());
    m.duration_s = 60.0;
    m.processing.status = ProcessingStatus::Done;
    m.recording = Some(Recording {
        state: RecordingState::Available,
        handling,
        path: path.into(),
        original_path: None,
        sha256: SHA.into(),
        bytes: 4,
        source_sha256: None,
        sources: Vec::new(),
        retention_override: None,
        delete_after: None,
    });
    m
}

fn recording(id: &str, status: ProcessingStatus) -> Meeting {
    let mut m = meeting(id, Handling::Copy, "audio.m4a");
    m.source = Source::Recording;
    m.source_id = Some(format!("recording:{SESSION}"));
    m.processing.status = status;
    m
}

fn print(person: &PersonId, kind: VoiceprintKind, meeting: Option<&str>, n: u8) -> Voiceprint {
    Voiceprint {
        id: format!("vp-{person}-{n}"),
        model: MODEL.into(),
        dim: 2,
        vector_b64: encode_vector(&[1.0, f32::from(n)]),
        source: VoiceprintSource {
            kind,
            meeting_id: meeting.map(mid),
            sha256: None,
            start: 0.0,
            end: 2.0,
            ranges: Vec::new(),
            speaker_id: None,
            speaker_namespace: String::new(),
        },
        device: None,
        strength: Strength::Confirmed,
        created_at: at("2026-11-02T10:00:00Z"),
    }
}

fn code(e: &Error) -> &'static str {
    e.code()
}

// ---- titles ----

#[test]
fn title_precedence_prefers_the_latest_explicit_user_edit() {
    let mut m = meeting("m-000001", Handling::Copy, "audio.m4a");
    m.title_source = Some(TitleSource::Calendar);
    assert!(m.offer_user_title("Typed", at("2026-11-16T10:00:00Z")));
    assert!(!m.offer_user_title("Older", at("2026-11-16T09:00:00Z")));
    assert!(m.offer_user_title("Newer", at("2026-11-16T11:00:00Z")));
    assert_eq!(m.title.as_deref(), Some("Newer"));

    // A legacy user title (no time) is never replaced by calendar or file
    // titles, only by a timestamped explicit edit.
    let mut legacy = meeting("m-000002", Handling::Copy, "audio.m4a");
    legacy.title = Some("Legacy typed".into());
    legacy.title_source = Some(TitleSource::User);
    let mut calendar = legacy.clone();
    calendar.title = Some("Calendar title".into());
    calendar.title_source = Some(TitleSource::Calendar);
    calendar.keep_stronger_title(&legacy);
    assert_eq!(calendar.title.as_deref(), Some("Legacy typed"));
    let mut later = legacy.clone();
    later.keep_stronger_title(&calendar);
    assert_eq!(later.title.as_deref(), Some("Legacy typed"));
    assert!(legacy.offer_user_title("Explicit", at("2026-11-16T10:00:00Z")));

    // A new pass's user title beats the earlier pass's calendar title.
    let mut user = meeting("m-000003", Handling::Copy, "audio.m4a");
    user.offer_user_title("Session edit", at("2026-11-16T10:00:00Z"));
    let mut earlier = user.clone();
    earlier.title = Some("Calendar".into());
    earlier.title_source = Some(TitleSource::Calendar);
    earlier.title_set_at = None;
    user.keep_stronger_title(&earlier);
    assert_eq!(user.title.as_deref(), Some("Session edit"));
}

#[test]
fn set_meeting_title_updates_search_and_keeps_the_folder() {
    let fx = Fx::new();
    let m = meeting("m-000001", Handling::Copy, "audio.m4a");
    fx.save(&m);
    let dir = fx.dir(&m.id);
    let out = set_meeting_title(
        &fx.lib,
        &fx.journal_root(),
        &m.id,
        "  Quarterly zebra review ",
    )
    .unwrap();
    assert_eq!(out.title, "Quarterly zebra review");
    assert_eq!(out.session_id, None);
    let saved = fx.lib.load_meeting(&m.id).unwrap();
    assert_eq!(saved.title_source, Some(TitleSource::User));
    assert!(saved.title_set_at.is_some());
    assert_eq!(fx.dir(&m.id), dir, "the folder is never renamed");
    assert_eq!(fx.search("zebra"), vec![m.id.clone()]);
    for bad in ["", "   ", "two\nlines"] {
        assert_eq!(
            code(&set_meeting_title(&fx.lib, &fx.journal_root(), &m.id, bad).unwrap_err()),
            "invalid_input"
        );
    }
}

#[test]
fn renaming_a_provisional_meeting_reaches_its_session_sidecar() {
    let fx = Fx::new();
    let journal = fx.journal(SESSION);
    let m = recording("m-000001", ProcessingStatus::Provisional);
    fx.save(&m);
    let out = set_meeting_title(&fx.lib, &fx.journal_root(), &m.id, "Provisional rename").unwrap();
    assert_eq!(out.session_id.as_deref(), Some(SESSION));
    let sidecar = read_session_title(&journal).unwrap().unwrap();
    assert_eq!(sidecar.title, "Provisional rename");

    // t1 provisional rename, then t2 session rename: t2 wins everywhere.
    let renamed = set_session_title(
        &fx.lib,
        &fx.journal_root(),
        SESSION,
        "Session rename",
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        renamed,
        SessionTitleOutcome::Saved {
            session_id: SESSION.into(),
            title: "Session rename".into(),
            meeting_id: Some(m.id.clone()),
        }
    );
    assert_eq!(
        read_session_title(&journal).unwrap().unwrap().title,
        "Session rename"
    );
    assert_eq!(
        fx.lib.load_meeting(&m.id).unwrap().title.as_deref(),
        Some("Session rename")
    );
}

#[test]
fn session_title_is_refused_for_stale_sessions_and_routed_after_publication() {
    let fx = Fx::new();
    // No journal: stale, nothing written.
    let err =
        set_session_title(&fx.lib, &fx.journal_root(), SESSION, "Title", || Ok(())).unwrap_err();
    assert_eq!(code(&err), STALE_SESSION);
    let journal = fx.journal(SESSION);
    // The caller's own check refuses: nothing is written.
    let err = set_session_title(&fx.lib, &fx.journal_root(), SESSION, "Title", || {
        Err(stale_session(SESSION))
    })
    .unwrap_err();
    assert_eq!(code(&err), STALE_SESSION);
    assert!(read_session_title(&journal).unwrap().is_none());
    // A stopped session waiting for its final job (no meeting yet).
    set_session_title(
        &fx.lib,
        &fx.journal_root(),
        SESSION,
        "Before saving",
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        read_session_title(&journal).unwrap().unwrap().title,
        "Before saving"
    );
    // Discarded: stale.
    write_discard_intent(&fx.journal_root(), SESSION).unwrap();
    let err =
        set_session_title(&fx.lib, &fx.journal_root(), SESSION, "Late", || Ok(())).unwrap_err();
    assert_eq!(code(&err), STALE_SESSION);
    remove_discard_intent(&fx.journal_root(), SESSION).unwrap();
    // Saved: the meeting must be renamed instead.
    let m = recording("m-000001", ProcessingStatus::Done);
    fx.save(&m);
    assert_eq!(
        set_session_title(&fx.lib, &fx.journal_root(), SESSION, "After", || Ok(())).unwrap(),
        SessionTitleOutcome::AlreadyPublished {
            meeting_id: m.id.clone()
        }
    );
}

#[test]
fn publication_is_blocked_by_an_intent_or_a_missing_manifest() {
    let fx = Fx::new();
    let dir = fx.journal_root().join(SESSION);
    assert!(publication_blocked(&dir, SESSION));
    fx.journal(SESSION);
    assert!(!publication_blocked(&dir, SESSION));
    write_discard_intent(&fx.journal_root(), SESSION).unwrap();
    assert!(publication_blocked(&dir, SESSION));
    assert_eq!(
        code(&check_publication(&dir, SESSION).unwrap_err()),
        RECORDING_DISCARDED
    );
    assert_eq!(discard_intents(&fx.journal_root()).unwrap(), [SESSION]);
    let text =
        fs::read_to_string(discard_intent_path(&fx.journal_root(), SESSION).unwrap()).unwrap();
    assert!(text.contains(SESSION) && !text.contains("title"));
    assert!(discard_intent_path(&fx.journal_root(), "../x").is_err());
}

// ---- deletion ----

/// A copied meeting with audio, a playback cache, samples learned from it
/// and a person's unrelated samples and enrollment.
fn populated(fx: &Fx, handling: Handling) -> (Meeting, PersonId) {
    let m = meeting("m-000001", handling, "audio.m4a");
    fx.save(&m);
    fs::write(fx.dir(&m.id).join("audio.m4a"), b"aac!").unwrap();
    let cache = fx.cache().join(PLAYBACK_CACHE_DIR);
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join(format!("{SHA}.wav")), b"wav").unwrap();
    fs::write(cache.join("other.wav"), b"wav").unwrap();
    let pid = PersonId::parse("p-a1a1a1").unwrap();
    let person = fx.person(
        "p-a1a1a1",
        "Ana Example",
        vec![
            print(&pid, VoiceprintKind::Confirmation, Some("m-000001"), 1),
            print(&pid, VoiceprintKind::Auto, Some("m-000001"), 2),
            print(&pid, VoiceprintKind::Confirmation, Some("m-000009"), 3),
            print(&pid, VoiceprintKind::Enrollment, None, 4),
        ],
    );
    (m, person)
}

#[test]
fn delete_removes_the_meeting_and_its_derived_data_only() {
    let fx = Fx::new();
    let (m, person) = populated(&fx, Handling::Copy);
    let dir = fx.dir(&m.id);
    let plan = delete_plan(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!(plan.state, DeleteState::Planned);
    assert_eq!(plan.voice_samples, 2);
    assert_eq!(plan.people_affected, 1);
    assert_eq!(plan.playback_caches, 1);
    assert_eq!(plan.owned_audio.len(), 1);
    assert!(plan.owned_audio[0].ends_with("/audio.m4a"));
    assert!(!plan.moved_original);
    // The dry run changed nothing.
    assert!(dir.join("audio.m4a").exists());
    assert_eq!(
        fx.lib.load_voiceprints(&person).unwrap().embeddings.len(),
        4
    );

    let report = delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!(report.state, DeleteState::Deleted, "{report:?}");
    assert!(!dir.exists());
    assert!(!fx.lib.meeting_exists(&m.id).unwrap());
    assert!(fx.search("Synthetic").is_empty());
    let left = fx.lib.load_voiceprints(&person).unwrap();
    let kinds: Vec<_> = left.embeddings.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(kinds, ["vp-p-a1a1a1-3", "vp-p-a1a1a1-4"]);
    assert_eq!(fx.lib.load_person(&person).unwrap().name, "Ana Example");
    // The identity recheck was requested before the evidence changed.
    assert!(fx.queue().get(&learning::job_id(MODEL)).is_ok());
    let cache = fx.cache().join(PLAYBACK_CACHE_DIR);
    assert!(!cache.join(format!("{SHA}.wav")).exists());
    assert!(cache.join("other.wav").exists());
    assert!(!fx.lib.root().join(DELETED_DIR).join(m.id.as_str()).exists());
    assert!(!marker_path(fx.lib.root(), &m.id).exists());
    // Deleting again: not found.
    assert_eq!(
        code(&delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap_err()),
        "not_found"
    );
}

#[test]
fn delete_never_touches_referenced_originals() {
    let fx = Fx::new();
    let external = fx.tmp.path().join("elsewhere");
    fs::create_dir_all(&external).unwrap();
    let original = external.join("call.wav");
    let track = external.join("track2.wav");
    fs::write(&original, b"original").unwrap();
    fs::write(&track, b"track").unwrap();
    let mut m = meeting("m-000001", Handling::Reference, original.to_str().unwrap());
    if let Some(rec) = m.recording.as_mut() {
        rec.sources = vec![RecordingSource {
            path: track.clone(),
            track_index: 1,
            offset_s: 0.0,
            sha256: SHA.into(),
        }];
    }
    fx.save(&m);
    let plan = delete_plan(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert!(plan.owned_audio.is_empty());
    assert_eq!(plan.referenced_originals_kept.len(), 2);
    delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!(fs::read(&original).unwrap(), b"original");
    assert_eq!(fs::read(&track).unwrap(), b"track");
}

#[test]
fn a_moved_original_is_reported_as_the_only_copy() {
    let fx = Fx::new();
    let (m, _) = populated(&fx, Handling::Move);
    let plan = delete_plan(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert!(plan.moved_original);
    assert_eq!(plan.owned_audio.len(), 1);
}

#[test]
fn delete_refuses_live_jobs_and_only_copies_without_partial_changes() {
    let fx = Fx::new();
    let (m, person) = populated(&fx, Handling::Copy);
    let held = fx.enqueue(
        "reprocess",
        m.id.as_str(),
        serde_json::json!({ "meeting_id": m.id }),
    );
    let lease = fx.claim(&held);
    let err = delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap_err();
    assert_eq!(code(&err), JOB_RUNNING);
    assert!(fx.dir(&m.id).join("audio.m4a").exists());
    assert_eq!(
        fx.lib.load_voiceprints(&person).unwrap().embeddings.len(),
        4
    );
    assert!(!marker_path(fx.lib.root(), &m.id).exists());
    assert!(
        fx.queue().get(&learning::job_id(MODEL)).is_err(),
        "a refused delete requests no identity recheck"
    );
    lease.release(&fx.lib).unwrap();

    // A move journal in an import job's folder: the only copy is in flight.
    let import = fx.enqueue("import", SHA, serde_json::json!({ "sha256": SHA }));
    fs::write(
        fx.queue().job_dir(&import).join(crate::queue::MOVE_JOURNAL),
        b"{}",
    )
    .unwrap();
    let err = delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap_err();
    assert_eq!(code(&err), ONLY_COPY_IN_FLIGHT);
    assert!(fx.dir(&m.id).exists());
}

#[test]
fn delete_cancels_queued_and_retryable_jobs_so_they_never_republish() {
    let fx = Fx::new();
    let (m, _) = populated(&fx, Handling::Copy);
    let queued = fx.enqueue(
        "enrich",
        m.id.as_str(),
        serde_json::json!({ "meeting_id": m.id }),
    );
    let failed = fx.enqueue(
        "reprocess",
        m.id.as_str(),
        serde_json::json!({ "meeting_id": m.id }),
    );
    let lease = fx.claim(&failed);
    lease.fail(&fx.lib, "synthetic failure").unwrap();
    // A stale holder: its process died, its lease is still recorded.
    let stale = fx.enqueue(
        "confirm-samples",
        "k",
        serde_json::json!({ "meeting_id": m.id }),
    );
    let stale_lease = fx.claim(&stale);
    lease.release(&fx.lib).unwrap();
    let other = fx.enqueue(
        "enrich",
        "m-000777",
        serde_json::json!({ "meeting_id": "m-000777" }),
    );
    fx.probe.kill(4242);
    let report = delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!(report.jobs_cancelled, 3);
    assert_eq!(report.state, DeleteState::Deleted, "{report:?}");
    let q = fx.queue();
    for id in [&queued, &failed, &stale] {
        assert!(matches!(q.get(id), Err(Error::NotFound(_))), "{id} left");
    }
    assert!(q.get(&other).is_ok());
    // The stale holder can no longer commit anything.
    let err = stale_lease
        .commit(&fx.lib, JobState::Done, |_, _| Ok(()))
        .unwrap_err();
    assert_eq!(code(&err), "lease_lost");
}

#[test]
fn unsaved_recordings_are_discarded_not_deleted() {
    let fx = Fx::new();
    fx.journal(SESSION);
    let m = recording("m-000001", ProcessingStatus::Provisional);
    fx.save(&m);
    let err = delete_plan(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap_err();
    assert_eq!(code(&err), RECORDING_ACTIVE);
    assert!(err.to_string().contains(SESSION));
}

#[test]
fn deleting_a_saved_recording_fences_and_removes_its_leftover_journal() {
    let fx = Fx::new();
    let journal = fx.journal(SESSION);
    let m = recording("m-000001", ProcessingStatus::Done);
    fx.save(&m);
    let final_job = fx.enqueue(
        "record-final",
        SESSION,
        serde_json::json!({ "session_id": SESSION }),
    );
    let report = delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!(report.state, DeleteState::Deleted, "{report:?}");
    assert!(!journal.exists());
    assert!(!has_discard_intent(&fx.journal_root(), SESSION));
    assert!(fx.queue().get(&final_job).is_err());
}

#[test]
fn an_interrupted_deletion_finishes_at_the_next_maintenance() {
    for stage in ["marker", "renamed"] {
        let fx = Fx::new();
        let (m, person) = populated(&fx, Handling::Copy);
        let opts = fx.opts();
        let marker = ops::with_index(&fx.lib, |tx, index| {
            let plan = plan(tx.library(), &fx.probe, &opts, &m.id, Mode::User)?;
            if stage == "marker" {
                // Crash right after the marker: nothing else changed.
                let marker = DeletionMarker {
                    meeting_id: m.id.clone(),
                    session_id: None,
                    owns_journal: false,
                    recording_sha256: vec![SHA.into()],
                    revoked: Vec::new(),
                    finished: Vec::new(),
                    cache_dir: Some(fx.cache()),
                    journal_root: fx.journal_root(),
                    created_at: schema::now(),
                };
                fs::create_dir_all(fx.lib.root().join(DELETING_DIR)).unwrap();
                fsutil::write_json_atomic(&marker_path(tx.root(), &m.id), &marker)?;
                Ok(marker)
            } else {
                // Crash after the rename, before any file cleanup.
                commit(tx, index, &fx.probe, &opts, plan, true).map(|(_, marker)| marker)
            }
        })
        .unwrap();
        assert!(meeting_deleted(&fx.lib, &marker.meeting_id));
        assert_eq!(
            finish_pending_deletions(&fx.lib, &fx.probe).unwrap(),
            1,
            "{stage}"
        );
        assert!(!fx.lib.meeting_exists(&m.id).unwrap(), "{stage}");
        assert!(!marker_path(fx.lib.root(), &m.id).exists(), "{stage}");
        assert!(!fx.lib.root().join(DELETED_DIR).join(m.id.as_str()).exists());
        assert_eq!(
            fx.lib.load_voiceprints(&person).unwrap().embeddings.len(),
            2
        );
        assert!(fx.search("Synthetic").is_empty());
        // Idempotent.
        assert_eq!(finish_pending_deletions(&fx.lib, &fx.probe).unwrap(), 0);
    }
}

// ---- discard ----

#[test]
fn discard_after_process_now_removes_everything_and_never_republishes() {
    let fx = Fx::new();
    let journal = fx.journal(SESSION);
    fs::write(journal.join("captions.jsonl"), b"{}\n").unwrap();
    let m = recording("m-000001", ProcessingStatus::Provisional);
    fx.save(&m);
    fs::write(fx.dir(&m.id).join("audio.m4a"), b"aac!").unwrap();
    let provisional = fx.enqueue(
        "record-provisional",
        &format!("{SESSION}@1"),
        serde_json::json!({ "session_id": SESSION }),
    );
    // A provisional job still running for this session (live lease).
    let running = fx.claim(&provisional);
    write_discard_intent(&fx.journal_root(), SESSION).unwrap();
    let report = discard_session(&fx.lib, &fx.probe, &fx.opts(), SESSION, AGENT_IDLE).unwrap();
    assert_eq!(report.meetings_deleted, 1);
    assert_eq!(report.jobs_cancelled, 1);
    assert!(!report.complete, "the running job's process is still alive");
    assert!(has_discard_intent(&fx.journal_root(), SESSION));
    assert!(!fx.lib.meeting_exists(&m.id).unwrap());
    assert!(!journal.exists());
    // Its late commit is refused: no resurrection.
    let err = running
        .commit(&fx.lib, JobState::Done, |_, _| Ok(()))
        .unwrap_err();
    assert_eq!(code(&err), "lease_lost");
    assert!(publication_blocked(&journal, SESSION));
    drop(running);
    fx.probe.kill(4242);
    let report = discard_session(&fx.lib, &fx.probe, &fx.opts(), SESSION, AGENT_IDLE).unwrap();
    assert!(report.complete, "{report:?}");
    assert!(!has_discard_intent(&fx.journal_root(), SESSION));
    assert!(fx.queue().get(&provisional).is_err());
    // Without the journal, publication stays blocked after the intent went.
    assert!(publication_blocked(&journal, SESSION));
}

#[test]
fn discard_of_a_saved_session_keeps_the_meeting() {
    let fx = Fx::new();
    fx.journal(SESSION);
    let m = recording("m-000001", ProcessingStatus::Done);
    fx.save(&m);
    write_discard_intent(&fx.journal_root(), SESSION).unwrap();
    let report = discard_session(&fx.lib, &fx.probe, &fx.opts(), SESSION, AGENT_IDLE).unwrap();
    assert_eq!(report.saved_meeting, Some(m.id.clone()));
    assert!(fx.lib.meeting_exists(&m.id).unwrap());
    assert!(!has_discard_intent(&fx.journal_root(), SESSION));
}

#[test]
fn name_only_people_created_during_a_session_survive_its_discard() {
    let fx = Fx::new();
    fx.journal(SESSION);
    let pid = fx.person("p-c3c3c3", "Cara Example", Vec::new());
    write_discard_intent(&fx.journal_root(), SESSION).unwrap();
    assert!(
        discard_session(&fx.lib, &fx.probe, &fx.opts(), SESSION, AGENT_IDLE)
            .unwrap()
            .complete
    );
    assert!(fx.lib.load_person(&pid).is_ok());
}

/// The running agent, which no longer captures or stops the session.
const AGENT_IDLE: CaptureOwnership = CaptureOwnership::Agent { live: false };

/// A crash after the intent was written but before capture closed the
/// journal leaves it marked `recording`: the cleanup must still finish.
#[test]
fn discard_finishes_a_journal_orphaned_while_recording() {
    // At the agent's startup no capture runs anywhere: an owner PID that
    // happens to be alive (reused) does not make it live.
    let fx = Fx::new();
    let journal = fx.journal_in(SESSION, "recording", 4242);
    write_discard_intent(&fx.journal_root(), SESSION).unwrap();
    let report = discard_session(
        &fx.lib,
        &fx.probe,
        &fx.opts(),
        SESSION,
        CaptureOwnership::NoneRunning,
    )
    .unwrap();
    assert!(report.complete, "{report:?}");
    assert!(!journal.exists());
    assert!(!has_discard_intent(&fx.journal_root(), SESSION));

    // A running agent that no longer captures it: its owner, another
    // process, is dead, or the owner is this process whose capture ended
    // without closing the journal.
    for owner in [9999, 4242] {
        let fx = Fx::new();
        let journal = fx.journal_in(SESSION, "recording", owner);
        write_discard_intent(&fx.journal_root(), SESSION).unwrap();
        let report = discard_session(&fx.lib, &fx.probe, &fx.opts(), SESSION, AGENT_IDLE).unwrap();
        assert!(report.complete, "owner {owner}: {report:?}");
        assert!(!journal.exists());
    }
}

/// A capture that may still write the journal is never erased under it.
#[test]
fn discard_waits_for_a_live_capture_owner() {
    let fx = Fx::new();
    let m = recording("m-000001", ProcessingStatus::Provisional);
    let journal = fx.journal_in(SESSION, "recording", 4242);
    fx.save(&m);
    write_discard_intent(&fx.journal_root(), SESSION).unwrap();
    // The agent still captures (or is stopping) the session.
    let report = discard_session(
        &fx.lib,
        &fx.probe,
        &fx.opts(),
        SESSION,
        CaptureOwnership::Agent { live: true },
    )
    .unwrap();
    assert!(!report.complete);
    // Another live process owns the journal.
    fx.probe.alive.lock().unwrap().insert(7777);
    let journal = {
        fs::remove_dir_all(&journal).unwrap();
        fx.journal_in(SESSION, "recording", 7777)
    };
    let report = discard_session(&fx.lib, &fx.probe, &fx.opts(), SESSION, AGENT_IDLE).unwrap();
    assert!(!report.complete);
    // Nothing was touched while it might be live.
    assert!(journal.join("mic").join("000000.f32").exists());
    assert!(fx.lib.meeting_exists(&m.id).unwrap());
    assert!(has_discard_intent(&fx.journal_root(), SESSION));
    // Once that owner is gone, the orphan is cleaned up.
    fx.probe.kill(7777);
    let report = discard_session(&fx.lib, &fx.probe, &fx.opts(), SESSION, AGENT_IDLE).unwrap();
    assert!(report.complete, "{report:?}");
    assert!(!journal.exists());
    assert!(!fx.lib.meeting_exists(&m.id).unwrap());
}

/// A manifest that does not read as this session's is never permission to
/// erase: the folder stays, the intent stays (still fencing publication).
#[test]
fn discard_leaves_an_unreadable_or_foreign_journal_in_place() {
    for manifest in [
        "{not json".to_owned(),
        serde_json::json!({ "session_id": "s-other", "state": "stopped", "owner_pid": 1 })
            .to_string(),
    ] {
        let fx = Fx::new();
        let journal = fx.journal(SESSION);
        fs::write(journal.join(SESSION_MANIFEST), &manifest).unwrap();
        write_discard_intent(&fx.journal_root(), SESSION).unwrap();
        for ownership in [CaptureOwnership::NoneRunning, AGENT_IDLE] {
            let report =
                discard_session(&fx.lib, &fx.probe, &fx.opts(), SESSION, ownership).unwrap();
            assert!(!report.complete);
            assert!(journal.join("mic").join("000000.f32").exists());
            assert!(has_discard_intent(&fx.journal_root(), SESSION));
        }
    }
}

#[test]
fn intents_and_markers_hold_identifiers_only() {
    let fx = Fx::new();
    let (m, _) = populated(&fx, Handling::Copy);
    let opts = fx.opts();
    ops::with_index(&fx.lib, |tx, index| {
        let plan = plan(tx.library(), &fx.probe, &opts, &m.id, Mode::User)?;
        commit(tx, index, &fx.probe, &opts, plan, true)
    })
    .unwrap();
    let text = fs::read_to_string(marker_path(fx.lib.root(), &m.id)).unwrap();
    assert!(!text.contains("Synthetic planning") && !text.contains("Ana Example"));
}

// ---- council corrections ----

/// A tag removed during a recording after "Process now" is recorded on the
/// provisional meeting (`tags_removed`), which the final pass keeps, so it
/// cannot come back; re-adding it clears the removal.
#[test]
fn session_tag_changes_reach_the_provisional_meeting() {
    let fx = Fx::new();
    fx.journal(SESSION);
    let mut provisional = recording("m-000001", ProcessingStatus::Provisional);
    provisional.add_tag("alpha");
    fx.save(&provisional);
    let changed =
        apply_session_tags(&fx.lib, &fx.journal_root(), SESSION, &[], &["Alpha".into()]).unwrap();
    assert_eq!(changed, vec![provisional.id.clone()]);
    let saved = fx.lib.load_meeting(&provisional.id).unwrap();
    assert!(saved.effective_tags().is_empty());
    assert_eq!(saved.tags_removed, ["Alpha"]);
    apply_session_tags(&fx.lib, &fx.journal_root(), SESSION, &["alpha".into()], &[]).unwrap();
    let saved = fx.lib.load_meeting(&provisional.id).unwrap();
    assert_eq!(saved.effective_tags(), ["alpha"]);
    assert!(saved.tags_removed.is_empty());
    // A saved final meeting is the user's to tag; the session leaves it alone.
    let mut done = saved;
    done.processing.status = ProcessingStatus::Done;
    fx.save(&done);
    assert!(
        apply_session_tags(&fx.lib, &fx.journal_root(), SESSION, &[], &["alpha".into()])
            .unwrap()
            .is_empty()
    );
}

/// While the journal says when the session started, title and tag changes
/// read only the meeting folders of that start minute: an unreadable
/// meeting of another minute is never in the way, while one of the
/// session's own minute may be its meeting, so the change is refused rather
/// than that meeting skipped. Once the manifest is gone every meeting is
/// read and unreadable ones are skipped, so a saved session is still found.
#[test]
fn session_lookups_read_only_the_session_start_minute() {
    let fx = Fx::new();
    let journal = fx.journal(SESSION);
    fs::write(
        journal.join(SESSION_MANIFEST),
        serde_json::json!({
            "session_id": SESSION, "state": "stopped", "owner_pid": 4242,
            "started_at": "2026-11-02T10:00:30Z",
        })
        .to_string(),
    )
    .unwrap();
    let provisional = recording("m-000001", ProcessingStatus::Provisional);
    fx.save(&provisional);
    let corrupt = |id: &str, started: &str| {
        let mut m = meeting(id, Handling::Copy, "audio.m4a");
        m.started_at = at(started);
        fx.save(&m);
        fs::write(
            fx.dir(&m.id).join(crate::paths::MEETING_JSON),
            b"{ not json",
        )
        .unwrap();
    };
    corrupt("m-000002", "2026-11-02T09:00:00Z");

    let tags =
        |add: &str| apply_session_tags(&fx.lib, &fx.journal_root(), SESSION, &[add.into()], &[]);
    assert_eq!(tags("beta").unwrap(), vec![provisional.id.clone()]);
    let title = |t: &str| set_session_title(&fx.lib, &fx.journal_root(), SESSION, t, || Ok(()));
    assert_eq!(
        title("Targeted").unwrap(),
        SessionTitleOutcome::Saved {
            session_id: SESSION.into(),
            title: "Targeted".into(),
            meeting_id: Some(provisional.id.clone()),
        }
    );

    corrupt("m-000003", "2026-11-02T10:00:45Z");
    let before = fx.lib.load_meeting(&provisional.id).unwrap();
    assert_eq!(code(&tags("gamma").unwrap_err()), "schema");
    assert_eq!(code(&title("Refused").unwrap_err()), "schema");
    assert_eq!(fx.lib.load_meeting(&provisional.id).unwrap(), before);
    assert_eq!(
        read_session_title(&journal).unwrap().unwrap().title,
        "Targeted"
    );

    let mut saved = before;
    saved.processing.status = ProcessingStatus::Done;
    fx.save(&saved);
    fs::remove_file(journal.join(SESSION_MANIFEST)).unwrap();
    assert_eq!(
        title("After").unwrap(),
        SessionTitleOutcome::AlreadyPublished {
            meeting_id: provisional.id.clone()
        }
    );
    assert!(tags("delta").unwrap().is_empty());
}

/// A failed final attempt leaves a folder without `meeting.json` and samples
/// under the meeting ID its job recorded: discard removes exactly those
/// record outputs (proven by job, ID, start minute and missing
/// `meeting.json`) before revoking the job, and keeps anything else.
#[test]
fn discard_removes_an_unpublished_attempt_and_keeps_foreign_files() {
    let fx = Fx::new();
    let journal = fx.journal(SESSION);
    fs::write(
        journal.join(SESSION_MANIFEST),
        serde_json::json!({
            "session_id": SESSION, "state": "stopped", "owner_pid": 4242,
            "started_at": "2026-11-16T10:00:00Z",
        })
        .to_string(),
    )
    .unwrap();
    let job = fx.enqueue(
        "record-final",
        SESSION,
        serde_json::json!({ "session_id": SESSION }),
    );
    fs::write(
        fx.queue().job_dir(&job).join(RECORD_MEETING_ID_FILE),
        "m-00abcd\n",
    )
    .unwrap();
    let month = fx.lib.root().join("2026").join("11");
    let owned = month.join("2026-11-16-1000-untitled-00abcd");
    let decoy = month.join("2026-11-16-0900-untitled-00abcd");
    for dir in [&owned, &decoy] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("audio.m4a"), b"aac!").unwrap();
    }
    fs::write(owned.join("roster.json"), b"{}").unwrap();
    fs::write(owned.join("notes.txt"), b"the user's own file").unwrap();
    let pid = PersonId::parse("p-a1a1a1").unwrap();
    fx.person(
        "p-a1a1a1",
        "Ana Example",
        vec![
            print(&pid, VoiceprintKind::Auto, Some("m-00abcd"), 1),
            print(&pid, VoiceprintKind::Confirmation, Some("m-000009"), 2),
        ],
    );
    write_discard_intent(&fx.journal_root(), SESSION).unwrap();
    let report = discard_session(&fx.lib, &fx.probe, &fx.opts(), SESSION, AGENT_IDLE).unwrap();
    assert_eq!(report.unpublished_removed, 1);
    assert!(report.complete, "{report:?}");
    assert!(!owned.join("audio.m4a").exists() && !owned.join("roster.json").exists());
    assert!(
        owned.join("notes.txt").exists(),
        "files a record job never writes stay"
    );
    assert!(
        decoy.join("audio.m4a").exists(),
        "another start minute is not this session's"
    );
    let left = fx.lib.load_voiceprints(&pid).unwrap();
    let ids: Vec<_> = left.embeddings.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["vp-p-a1a1a1-2"]);
    assert!(fx.queue().get(&learning::job_id(MODEL)).is_ok());
    assert!(fx.queue().get(&job).is_err());
}

/// A deletion interrupted after its folder moved still removes the index
/// rows and text vectors when recovery finishes it.
#[test]
fn a_deletion_resumed_after_the_folder_moved_still_clears_the_index() {
    let fx = Fx::new();
    let (m, _) = populated(&fx, Handling::Copy);
    let marker = DeletionMarker {
        meeting_id: m.id.clone(),
        session_id: None,
        owns_journal: false,
        recording_sha256: vec![SHA.into()],
        revoked: Vec::new(),
        finished: Vec::new(),
        cache_dir: Some(fx.cache()),
        journal_root: fx.journal_root(),
        created_at: schema::now(),
    };
    fs::create_dir_all(fx.lib.root().join(DELETING_DIR)).unwrap();
    fsutil::write_json_atomic(&marker_path(fx.lib.root(), &m.id), &marker).unwrap();
    fs::create_dir_all(fx.lib.root().join(DELETED_DIR)).unwrap();
    fs::rename(fx.dir(&m.id), staged_path(fx.lib.root(), &m.id)).unwrap();
    assert_eq!(fx.search("Synthetic"), vec![m.id.clone()], "still indexed");
    assert_eq!(finish_pending_deletions(&fx.lib, &fx.probe).unwrap(), 1);
    assert!(fx.search("Synthetic").is_empty());
    assert!(!marker_path(fx.lib.root(), &m.id).exists());
}

/// When the locked changes do not all apply, the marker stays for recovery,
/// which finishes once the cause is gone. The failure is real: a SQLite
/// trigger aborts the vector-cache prune that runs after the folder moved.
#[test]
fn a_partly_applied_deletion_keeps_its_marker_until_recovery_finishes() {
    use crate::index::vectors::VectorCache;
    let fx = Fx::new();
    let (m, _) = populated(&fx, Handling::Copy);
    // A cache with an entry no window uses (so the prune must delete it),
    // and a trigger that refuses every delete.
    drop(VectorCache::open(fx.lib.root()).unwrap());
    let vectors = VectorCache::path(fx.lib.root());
    let conn = rusqlite::Connection::open(&vectors).unwrap();
    conn.execute_batch(
        "INSERT INTO vectors(model, text_sha256, dim, vector)
             VALUES ('test-text', 'unreferenced', 1, x'0000803f');
         CREATE TRIGGER refuse_prune BEFORE DELETE ON vectors
             BEGIN SELECT RAISE(ABORT, 'synthetic prune failure'); END;",
    )
    .unwrap();
    let report = delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!(report.state, DeleteState::CleanupPending, "{report:?}");
    assert!(
        !fx.lib.meeting_exists(&m.id).unwrap(),
        "the folder already moved"
    );
    assert!(marker_path(fx.lib.root(), &m.id).exists());
    // Recovery cannot finish while the failure lasts.
    assert_eq!(finish_pending_deletions(&fx.lib, &fx.probe).unwrap(), 0);
    assert!(marker_path(fx.lib.root(), &m.id).exists());
    conn.execute_batch("DROP TRIGGER refuse_prune;").unwrap();
    drop(conn);
    assert_eq!(finish_pending_deletions(&fx.lib, &fx.probe).unwrap(), 1);
    assert!(!marker_path(fx.lib.root(), &m.id).exists());
    assert_eq!(
        VectorCache::open(fx.lib.root())
            .unwrap()
            .count(None)
            .unwrap(),
        0
    );
}

/// A job claimed between the plan and the commit refuses the delete in the
/// same queue-lock section that would revoke it: nothing changes.
#[test]
fn a_job_claimed_after_planning_refuses_the_delete_atomically() {
    let fx = Fx::new();
    let (m, person) = populated(&fx, Handling::Copy);
    let job = fx.enqueue(
        "enrich",
        m.id.as_str(),
        serde_json::json!({ "meeting_id": m.id }),
    );
    let opts = fx.opts();
    let mut lease = None;
    let err = ops::with_index(&fx.lib, |tx, index| {
        let plan = plan(tx.library(), &fx.probe, &opts, &m.id, Mode::User)?;
        assert!(plan.revoke.contains(&job), "queued when planned");
        lease = Some(fx.claim(&job));
        commit(tx, index, &fx.probe, &opts, plan, true)
    })
    .unwrap_err();
    assert_eq!(code(&err), JOB_RUNNING);
    assert!(!marker_path(fx.lib.root(), &m.id).exists());
    assert!(
        fx.queue().get(&learning::job_id(MODEL)).is_err(),
        "the recheck is requested only once the delete is admitted"
    );
    assert!(fx.dir(&m.id).join("audio.m4a").exists());
    assert_eq!(
        fx.lib.load_voiceprints(&person).unwrap().embeddings.len(),
        4
    );
    let claimed = fx.queue().get(&job).unwrap();
    assert_eq!(claimed.state, JobState::Running);
    assert_ne!(claimed.error.as_deref(), Some(REVOKED));
    // The worker's lease is intact: it can still commit.
    lease
        .unwrap()
        .commit(&fx.lib, JobState::Done, |_, _| Ok(()))
        .unwrap();
}

// ---- resumed deletions, admission, stable folders, diagnostics ----

/// A vector cache with an entry no window uses (so the prune after the
/// rename must delete it) and a trigger that refuses every delete: a real
/// failure of the locked steps after the folder moved.
fn refusing_prune(fx: &Fx) -> rusqlite::Connection {
    use crate::index::vectors::VectorCache;
    drop(VectorCache::open(fx.lib.root()).unwrap());
    let conn = rusqlite::Connection::open(VectorCache::path(fx.lib.root())).unwrap();
    conn.execute_batch(
        "INSERT INTO vectors(model, text_sha256, dim, vector)
             VALUES ('test-text', 'unreferenced', 1, x'0000803f');
         CREATE TRIGGER refuse_prune BEFORE DELETE ON vectors
             BEGIN SELECT RAISE(ABORT, 'synthetic prune failure'); END;",
    )
    .unwrap();
    conn
}

/// A delete whose locked steps failed after the staged rename is committed:
/// deleting the same meeting again resumes its marker (never `not_found`),
/// reports it pending while the cause lasts and finishes it afterwards.
#[test]
fn deleting_again_finishes_a_committed_deletion_after_its_folder_moved() {
    let fx = Fx::new();
    let (m, person) = populated(&fx, Handling::Copy);
    let conn = refusing_prune(&fx);
    let first = delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!(first.state, DeleteState::CleanupPending, "{first:?}");
    assert!(!fx.lib.meeting_exists(&m.id).unwrap(), "the folder moved");
    assert!(meeting_deleted(&fx.lib, &m.id));

    let plan = delete_plan(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!((plan.state, plan.dry_run), (DeleteState::Planned, true));
    let retry = delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!(retry.state, DeleteState::CleanupPending, "{retry:?}");
    assert!(!retry.cleanup_failures.is_empty());
    assert!(meeting_deleted(&fx.lib, &m.id), "never claimed complete");

    conn.execute_batch("DROP TRIGGER refuse_prune;").unwrap();
    drop(conn);
    let done = delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!(done.state, DeleteState::Deleted, "{done:?}");
    assert!(done.cleanup_failures.is_empty());
    assert!(!meeting_deleted(&fx.lib, &m.id));
    assert!(!staged_path(fx.lib.root(), &m.id).exists());
    assert!(fx.search("Synthetic").is_empty());
    let left = fx.lib.load_voiceprints(&person).unwrap();
    let ids: Vec<_> = left.embeddings.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["vp-p-a1a1a1-3", "vp-p-a1a1a1-4"]);
    // Finished for good: nothing comes back and another delete is not_found.
    assert!(!fx.lib.meeting_exists(&m.id).unwrap());
    assert_eq!(
        code(&delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap_err()),
        "not_found"
    );
}

/// A crash after the commit's rename, before any physical cleanup: the
/// user's next delete finishes it on its own, without maintenance.
#[test]
fn deleting_again_finishes_a_deletion_interrupted_after_the_rename() {
    let fx = Fx::new();
    let (m, _) = populated(&fx, Handling::Copy);
    let opts = fx.opts();
    ops::with_index(&fx.lib, |tx, index| {
        let plan = plan(tx.library(), &fx.probe, &opts, &m.id, Mode::User)?;
        commit(tx, index, &fx.probe, &opts, plan, true)
    })
    .unwrap();
    assert!(staged_path(fx.lib.root(), &m.id).exists());
    let report = delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!(report.state, DeleteState::Deleted, "{report:?}");
    assert_eq!(report.playback_caches, 1);
    assert!(!staged_path(fx.lib.root(), &m.id).exists());
    assert!(!marker_path(fx.lib.root(), &m.id).exists());
    let cache = fx.cache().join(PLAYBACK_CACHE_DIR);
    assert!(!cache.join(format!("{SHA}.wav")).exists());
    assert!(cache.join("other.wav").exists());
}

/// A discard refused by another kind of live job on its provisional meeting
/// changes nothing first: the unpublished attempt, its samples and its job
/// stay, and no identity recheck is requested. Once the job ends it goes
/// through.
#[test]
fn a_refused_discard_changes_nothing_and_requests_no_recheck() {
    let fx = Fx::new();
    fx.journal(SESSION);
    let m = recording("m-000001", ProcessingStatus::Provisional);
    fx.save(&m);
    let record = fx.enqueue(
        "record-final",
        SESSION,
        serde_json::json!({ "session_id": SESSION }),
    );
    fs::write(
        fx.queue().job_dir(&record).join(RECORD_MEETING_ID_FILE),
        "m-00abcd\n",
    )
    .unwrap();
    let owned = fx
        .lib
        .root()
        .join("2026")
        .join("11")
        .join("2026-11-16-1000-untitled-00abcd");
    fs::create_dir_all(&owned).unwrap();
    fs::write(owned.join("audio.m4a"), b"aac!").unwrap();
    let pid = PersonId::parse("p-a1a1a1").unwrap();
    fx.person(
        "p-a1a1a1",
        "Ana Example",
        vec![print(&pid, VoiceprintKind::Auto, Some("m-00abcd"), 1)],
    );
    let enrich = fx.enqueue(
        "enrich",
        m.id.as_str(),
        serde_json::json!({ "meeting_id": m.id }),
    );
    let lease = fx.claim(&enrich);
    write_discard_intent(&fx.journal_root(), SESSION).unwrap();

    let err = discard_session(&fx.lib, &fx.probe, &fx.opts(), SESSION, AGENT_IDLE).unwrap_err();
    assert_eq!(code(&err), JOB_RUNNING);
    assert!(owned.join("audio.m4a").exists());
    assert_eq!(fx.lib.load_voiceprints(&pid).unwrap().embeddings.len(), 1);
    assert!(fx.queue().get(&learning::job_id(MODEL)).is_err());
    assert_ne!(
        fx.queue().get(&record).unwrap().error.as_deref(),
        Some(REVOKED)
    );
    assert!(fx.lib.meeting_exists(&m.id).unwrap());
    assert!(!marker_path(fx.lib.root(), &m.id).exists());
    assert!(has_discard_intent(&fx.journal_root(), SESSION));

    lease.release(&fx.lib).unwrap();
    let report = discard_session(&fx.lib, &fx.probe, &fx.opts(), SESSION, AGENT_IDLE).unwrap();
    assert!(report.complete, "{report:?}");
    assert_eq!(report.unpublished_removed, 1);
    assert!(!owned.join("audio.m4a").exists());
    assert!(fx.lib.load_voiceprints(&pid).unwrap().embeddings.is_empty());
    assert!(fx.queue().get(&learning::job_id(MODEL)).is_ok());
    assert!(!fx.lib.meeting_exists(&m.id).unwrap());
}

/// A record attempt that failed before publishing left a folder named with
/// the title of that moment; after a title change it is still found by the
/// meeting's stable ID (and start minute), never by the new title's path.
#[test]
fn unpublished_folders_are_found_by_meeting_id_after_a_title_change() {
    let fx = Fx::new();
    let id = mid("m-00abcd");
    let started = at("2026-11-16T10:00:00Z");
    let rel = |title: &str| crate::paths::meeting_rel_dir(&started, Some(title), &id);
    let earlier = fx.lib.root().join(rel("Old synthetic title"));
    fs::create_dir_all(&earlier).unwrap();
    fs::write(earlier.join("audio.m4a"), b"aac!").unwrap();
    let renamed = fx.lib.root().join(rel("New synthetic title"));
    assert_ne!(earlier, renamed);
    assert!(!renamed.exists());

    let minute = started.format("%Y-%m-%d-%H%M").to_string();
    let root = fx.lib.root();
    assert_eq!(
        unpublished_meeting_dirs(root, &id, Some(minute.as_str())).unwrap(),
        vec![earlier.clone()]
    );
    assert_eq!(
        unpublished_meeting_dirs(root, &id, None).unwrap(),
        vec![earlier.clone()]
    );
    assert!(
        unpublished_meeting_dirs(root, &id, Some("2026-11-16-0900"))
            .unwrap()
            .is_empty()
    );
    assert!(
        unpublished_meeting_dirs(root, &mid("m-00abce"), None)
            .unwrap()
            .is_empty()
    );
    // Published: no longer an unpublished folder.
    fs::write(earlier.join(crate::paths::MEETING_JSON), b"{}").unwrap();
    assert!(
        unpublished_meeting_dirs(root, &id, None)
            .unwrap()
            .is_empty()
    );
}

/// The diagnosis names pending deletions, discards, unpublished folders and
/// dead writers' temporaries by identifier or count only, and repairs
/// nothing.
#[test]
fn diagnosis_reports_pending_fences_by_identifier_and_changes_nothing() {
    let fx = Fx::new();
    let (m, _) = populated(&fx, Handling::Copy);
    let opts = fx.opts();
    ops::with_index(&fx.lib, |tx, index| {
        let plan = plan(tx.library(), &fx.probe, &opts, &m.id, Mode::User)?;
        commit(tx, index, &fx.probe, &opts, plan, true)
    })
    .unwrap();
    let root = fx.lib.root();
    let deleting = root.join(DELETING_DIR);
    let unreadable = deleting.join("m-000002.json");
    fs::write(&unreadable, b"not a marker").unwrap();
    fs::create_dir_all(root.join(DELETED_DIR).join("m-000003")).unwrap();
    write_discard_intent(&fx.journal_root(), SESSION).unwrap();
    let dead = deleting.join(".m-000001.json.tmp-999-0000abcd");
    let mine = deleting.join(".m-000001.json.tmp-4242-0000abcd");
    for path in [&dead, &mine] {
        fs::write(path, b"{").unwrap();
        age(path);
    }
    let month = root.join("2026").join("11");
    fs::create_dir_all(month.join("2026-11-16-1000-old-title-00abcd")).unwrap();
    fs::create_dir_all(month.join("holiday photos")).unwrap();

    let d = diagnose(&fx.lib, &fx.data_dir(), &fx.probe).unwrap();
    assert_eq!(d.pending_deletions.len(), 1, "{d:?}");
    let pending = &d.pending_deletions[0];
    assert_eq!(pending.meeting_id, m.id);
    assert_eq!(pending.stage, DeletionStage::Staged);
    assert_eq!(d.unreadable_deletion_markers, 1);
    assert_eq!(d.unaccounted_staged, 1);
    assert_eq!(d.pending_discards, vec![SESSION.to_owned()]);
    assert_eq!(d.unpublished_meetings, vec![mid("m-00abcd")]);
    assert_eq!(d.unidentified_folders, 1);
    assert_eq!(d.orphan_temp_files, 1);
    let text = serde_json::to_string(&d).unwrap();
    for private in [
        "Synthetic",
        "Ana Example",
        "old-title",
        "holiday",
        fx.tmp.path().to_str().unwrap(),
    ] {
        assert!(!text.contains(private), "{private} in {text}");
    }
    // Nothing was repaired.
    assert!(dead.exists() && mine.exists() && unreadable.exists());
    assert!(staged_path(root, &m.id).exists());
    assert!(marker_path(root, &m.id).exists());
    assert!(has_discard_intent(&fx.journal_root(), SESSION));
}

/// Maintenance removes only atomic-write temporaries proven orphaned (the
/// exact pattern, a dead writer, old enough) among the deletion markers; an
/// unreadable marker is reported, never repaired.
#[test]
fn maintenance_removes_only_aged_temporaries_of_dead_writers() {
    use std::time::SystemTime;
    let fx = Fx::new();
    let deleting = fx.lib.root().join(DELETING_DIR);
    fs::create_dir_all(&deleting).unwrap();
    let aged_dead = deleting.join(".m-000001.json.tmp-999-0000abcd");
    let aged_mine = deleting.join(".m-000001.json.tmp-4242-0000abcd");
    let fresh_dead = deleting.join(".m-000001.json.tmp-998-0000abce");
    let lookalike = deleting.join(".m-000001.json.tmp-999-not-hex");
    let unreadable = deleting.join("m-000002.json");
    for path in [&aged_dead, &aged_mine, &fresh_dead, &lookalike, &unreadable] {
        fs::write(path, b"{").unwrap();
    }
    let old = SystemTime::now() - Duration::from_secs(60 * 60);
    for path in [&aged_dead, &aged_mine, &lookalike] {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    assert_eq!(finish_pending_deletions(&fx.lib, &fx.probe).unwrap(), 0);
    assert!(!aged_dead.exists());
    assert!(aged_mine.exists() && fresh_dead.exists() && lookalike.exists());
    assert!(unreadable.exists());
}

/// Sets `path`'s modification time an hour back: older than the minimum
/// age orphaned temporaries must reach before they count or go.
fn age(path: &Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(60 * 60);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(old)
        .unwrap();
}

/// Every folder the library, the queue and the data folder's templates are
/// written atomically into is diagnosed and swept by one rule: an aged
/// temporary of a dead writer goes from the library root, a meeting
/// folder, its templates, a person's folder, the forget tombstones, the
/// live sidecars, the index, the queue, a job folder, the deletion markers,
/// the discard intents and the user's templates; a live or this process's
/// writer, a recent file, a lookalike name and the files themselves stay.
/// Counted before, none left after.
#[test]
fn orphaned_temporaries_are_counted_and_swept_everywhere_the_library_writes() {
    let fx = Fx::new();
    let m = meeting("m-000001", Handling::Copy, "audio.m4a");
    fx.save(&m);
    fx.person("p-a1a1a1", "Ana Example", Vec::new());
    let job = fx.enqueue("enrich", "m-000001", serde_json::json!({}));
    write_discard_intent(&fx.journal_root(), SESSION).unwrap();
    let template = crate::templates::save_user(
        &fx.data_dir(),
        "my-notes",
        "My notes",
        "",
        "List what was agreed.",
    )
    .unwrap();
    let root = fx.lib.root();
    let templates = fx.dir(&m.id).join(crate::paths::MEETING_TEMPLATES_DIR);
    fs::create_dir_all(&templates).unwrap();
    let deleting = root.join(DELETING_DIR);
    fs::create_dir_all(&deleting).unwrap();
    let forgotten = root.join(ops::FORGOTTEN_DIR);
    fs::create_dir_all(&forgotten).unwrap();
    let live = root.join(crate::live_identity::LIVE_DIR);
    fs::create_dir_all(&live).unwrap();
    let folders = [
        root.to_path_buf(),
        fx.dir(&m.id),
        templates,
        fx.lib.person_dir(&PersonId::parse("p-a1a1a1").unwrap()),
        forgotten,
        live,
        root.join(crate::paths::INDEX_DIR),
        root.join(crate::paths::QUEUE_DIR),
        fx.queue().job_dir(&job),
        deleting,
        fx.journal_root().join(DISCARD_DIR),
        crate::templates::dir(&fx.data_dir()),
    ];
    fx.probe.alive.lock().unwrap().insert(77);
    let mut orphans = Vec::new();
    let mut kept = Vec::new();
    for dir in &folders {
        let orphan = dir.join(".meeting.json.tmp-999-0000abcd");
        let live = dir.join(".meeting.json.tmp-77-0000abcd");
        let mine = dir.join(".meeting.json.tmp-4242-0000abcd");
        let fresh = dir.join(".meeting.json.tmp-998-0000abcd");
        let lookalike = dir.join(".meeting.json.tmp-999-not-hex");
        for path in [&orphan, &live, &mine, &fresh, &lookalike] {
            fs::write(path, b"{").unwrap();
        }
        for path in [&orphan, &live, &mine, &lookalike] {
            age(path);
        }
        orphans.push(orphan);
        kept.extend([live, mine, fresh, lookalike]);
    }

    let before = diagnose(&fx.lib, &fx.data_dir(), &fx.probe).unwrap();
    assert_eq!(before.orphan_temp_files, folders.len());
    assert!(
        orphans.iter().all(|p| p.exists()),
        "diagnosis removes nothing"
    );
    assert_eq!(
        sweep_orphan_temps(&fx.lib, &fx.data_dir(), &fx.probe).unwrap(),
        folders.len()
    );
    for path in &orphans {
        assert!(!path.exists(), "{}", path.display());
    }
    for path in &kept {
        assert!(path.exists(), "{}", path.display());
    }
    let after = diagnose(&fx.lib, &fx.data_dir(), &fx.probe).unwrap();
    assert_eq!(after.orphan_temp_files, 0);
    assert_eq!(fx.lib.load_meeting(&m.id).unwrap().id, m.id);
    assert!(fx.queue().get(&job).is_ok());
    assert_eq!(
        crate::templates::get(&fx.data_dir(), "my-notes").unwrap(),
        template
    );
}

/// A deletion resumed after a crash revokes every job of the meeting, but a
/// revoked job folder holding the user's only copy of a moved file is never
/// removed: the deletion stays pending (marker kept) instead of losing it.
#[test]
fn a_resumed_deletion_never_removes_a_job_folder_holding_a_moved_original() {
    let fx = Fx::new();
    let (m, _) = populated(&fx, Handling::Move);
    let marker = DeletionMarker {
        meeting_id: m.id.clone(),
        session_id: None,
        owns_journal: false,
        recording_sha256: vec![SHA.into()],
        revoked: Vec::new(),
        finished: Vec::new(),
        cache_dir: Some(fx.cache()),
        journal_root: fx.journal_root(),
        created_at: schema::now(),
    };
    fs::create_dir_all(fx.lib.root().join(DELETING_DIR)).unwrap();
    fsutil::write_json_atomic(&marker_path(fx.lib.root(), &m.id), &marker).unwrap();
    let import = fx.enqueue("import", SHA, serde_json::json!({ "sha256": SHA }));
    let journal = fx.queue().job_dir(&import).join(crate::queue::MOVE_JOURNAL);
    fs::write(&journal, b"{}").unwrap();

    let report = delete_meeting(&fx.lib, &fx.probe, &fx.opts(), &m.id).unwrap();
    assert_eq!(report.state, DeleteState::CleanupPending, "{report:?}");
    assert!(
        report
            .cleanup_failures
            .iter()
            .any(|f| f.contains("only copy")),
        "{report:?}"
    );
    assert!(journal.exists(), "the moved original's job folder stays");
    assert!(!fx.lib.meeting_exists(&m.id).unwrap());
    assert!(meeting_deleted(&fx.lib, &m.id));
}
