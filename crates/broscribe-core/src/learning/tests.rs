use std::time::Duration;

use chrono::TimeDelta;

use super::*;
use crate::ops::{SpeakerChange, SpeakerState};
use crate::process::ProcessProbe;
use crate::queue::{CONFIRM_KIND, JobState, Next, Queue};
use crate::schema::{Centroid, Meeting, Person, Source, Speaker};
use crate::{PersonId, SpeakerId};

const MODEL: &str = "test-embedder";
const A: &str = "m-0000a1";
const B: &str = "m-0000b2";
const C: &str = "m-0000c3";
const ANA: &str = "p-a1a1a1";
const BIA: &str = "p-b2b2b2";

/// This process, alive.
struct Me;

impl ProcessProbe for Me {
    fn is_alive(&self, pid: u32) -> bool {
        pid == std::process::id()
    }
    fn current_pid(&self) -> u32 {
        std::process::id()
    }
}

fn mid(s: &str) -> MeetingId {
    MeetingId::parse(s).unwrap()
}

fn pid(s: &str) -> PersonId {
    PersonId::parse(s).unwrap()
}

fn sid(n: u32) -> SpeakerId {
    SpeakerId::new(n).unwrap()
}

fn setup() -> (tempfile::TempDir, Library) {
    let tmp = tempfile::tempdir().unwrap();
    let lib = Library::open(tmp.path()).unwrap();
    (tmp, lib)
}

fn stored(lib: &Library) -> ReidentifyInput {
    let job = Queue::open(lib.root())
        .unwrap()
        .get(&job_id(MODEL))
        .unwrap();
    ReidentifyInput::of(&job).unwrap()
}

#[test]
fn requests_coalesce_per_model_with_the_latest_source_first() {
    let (_tmp, lib) = setup();
    // `Library::open` creates the queue folder; a library without one shows
    // that reading the status creates none.
    let queue_dir = lib.root().join(paths::QUEUE_DIR);
    std::fs::remove_dir(&queue_dir).unwrap();
    assert!(status(&lib, MODEL).unwrap().is_none());
    assert!(!queue_dir.exists(), "reading the status creates nothing");
    let a = lib.write(|tx| request(tx, MODEL, Some(&mid(A)))).unwrap();
    let b = lib.write(|tx| request(tx, MODEL, Some(&mid(B)))).unwrap();
    let again = lib.write(|tx| request(tx, MODEL, Some(&mid(A)))).unwrap();
    assert_eq!((a.generation, b.generation, again.generation), (1, 2, 3));
    assert_eq!(a.job_id, again.job_id);
    assert_eq!(again.source_meeting, Some(mid(A)));
    let s = status(&lib, MODEL).unwrap().unwrap();
    assert_eq!(s.state, JobState::Queued);
    assert!(!s.running);
    assert_eq!((s.requested, s.processed), (3, 0));
    assert_eq!(s.source_meetings, vec![mid(A), mid(B)]);
    // The job names no single meeting: other writers never wait for it.
    let job = Queue::open(lib.root()).unwrap().get(&a.job_id).unwrap();
    assert!(job.input.get("meeting_id").is_none());
    assert_eq!(job.key, MODEL);
    // Each model has its own job.
    let other = lib.write(|tx| request(tx, "other-embedder", None)).unwrap();
    assert_ne!(other.job_id, a.job_id);
    assert_eq!(other.generation, 1);
    assert!(
        lib.write(|tx| request(tx, " ", None))
            .unwrap_err()
            .to_string()
            .contains("model")
    );
}

#[test]
fn quick_requests_during_a_running_pass_are_never_overwritten() {
    let (_tmp, lib) = setup();
    lib.write(|tx| request(tx, MODEL, Some(&mid(A)))).unwrap();
    let queue = Queue::open(lib.root()).unwrap();
    let lease = queue.claim(&lib, &job_id(MODEL), &Me).unwrap().unwrap();
    // The worker's copy of the input when it started.
    let stale = stored(&lib);
    // Two confirmations while the job runs.
    lib.write(|tx| request(tx, MODEL, Some(&mid(B)))).unwrap();
    lib.write(|tx| request(tx, MODEL, Some(&mid(C)))).unwrap();
    let job = queue.get(&job_id(MODEL)).unwrap();
    assert_eq!(
        job.state,
        JobState::Running,
        "the running job keeps running"
    );
    assert_eq!(
        job.lease.as_ref().map(|l| l.token.as_str()),
        Some(lease.token())
    );
    assert!(status(&lib, MODEL).unwrap().unwrap().running);
    // The lease's update reads the stored input, with both requests.
    lib.write(|tx| {
        lease.update_in(tx, None, |v| {
            let mut current = ReidentifyInput::of_value(v).unwrap();
            assert_eq!(current.requested, 3);
            current.processed = stale.requested;
            *v = current.to_value()?;
            Ok(())
        })
    })
    .unwrap();
    let after = stored(&lib);
    assert_eq!((after.requested, after.processed), (3, 1));
    assert_eq!(after.source_meetings, vec![mid(C), mid(B), mid(A)]);

    // A continuation goes to the back of the queue: jobs queued meanwhile
    // run first.
    let created = queue.get(&job_id(MODEL)).unwrap().created_at;
    std::thread::sleep(Duration::from_millis(5));
    lib.write(|tx| lease.update_in(tx, Some(Next::Requeue { not_before: None }), |_| Ok(())))
        .unwrap();
    lease.release(&lib).unwrap();
    let job = queue.get(&job_id(MODEL)).unwrap();
    assert_eq!(job.state, JobState::Queued);
    assert!(job.created_at > created);
    assert!(job.is_claimable());
    assert_eq!(job.attempts, 0);
}

#[test]
fn a_job_waiting_for_busy_meetings_is_not_claimable_until_its_time_or_a_request() {
    let (_tmp, lib) = setup();
    lib.write(|tx| request(tx, MODEL, None)).unwrap();
    let queue = Queue::open(lib.root()).unwrap();
    let lease = queue.claim(&lib, &job_id(MODEL), &Me).unwrap().unwrap();
    let later = crate::schema::now() + TimeDelta::hours(1);
    lib.write(|tx| {
        lease.update_in(
            tx,
            Some(Next::Requeue {
                not_before: Some(later),
            }),
            |_| Ok(()),
        )
    })
    .unwrap();
    lease.release(&lib).unwrap();
    let job = queue.get(&job_id(MODEL)).unwrap();
    assert!(!job.is_claimable(), "no hot loop over busy meetings");
    assert!(queue.claim(&lib, &job_id(MODEL), &Me).unwrap().is_none());
    assert_eq!(
        status(&lib, MODEL).unwrap().unwrap().not_before,
        Some(later)
    );
    // A new request runs at once.
    lib.write(|tx| request(tx, MODEL, Some(&mid(A)))).unwrap();
    let job = queue.get(&job_id(MODEL)).unwrap();
    assert_eq!(job.not_before, None);
    assert!(job.is_claimable());
}

#[test]
fn a_done_job_keeps_its_report_until_the_next_request() {
    let (_tmp, lib) = setup();
    lib.write(|tx| request(tx, MODEL, Some(&mid(A)))).unwrap();
    let queue = Queue::open(lib.root()).unwrap();
    let lease = queue.claim(&lib, &job_id(MODEL), &Me).unwrap().unwrap();
    lib.write(|tx| {
        lease.update_in(tx, Some(Next::Done), |v| {
            let mut input = ReidentifyInput::of_value(v).unwrap();
            input.processed = input.requested;
            input.source_meetings.clear();
            let mut report = LearningReport {
                generation: input.requested,
                ..LearningReport::default()
            };
            report.record(
                &mid(A),
                true,
                &[change(A, 2, state(Assignment::None, None), auto(ANA))],
            );
            input.report = Some(report);
            *v = input.to_value()?;
            Ok(())
        })
    })
    .unwrap();
    lease.release(&lib).unwrap();
    let done = status(&lib, MODEL).unwrap().unwrap();
    assert_eq!(done.state, JobState::Done);
    let report = done.report.unwrap();
    assert_eq!((report.generation, report.newly_named), (1, 1));
    assert_eq!(report.meetings[0].meeting_id, mid(A));

    let next = lib.write(|tx| request(tx, MODEL, Some(&mid(B)))).unwrap();
    assert_eq!(next.generation, 2);
    let s = status(&lib, MODEL).unwrap().unwrap();
    assert_eq!(s.state, JobState::Queued);
    assert_eq!((s.requested, s.processed), (2, 1));
    assert_eq!(
        s.report, None,
        "a new request after completion starts afresh"
    );
}

fn state(assignment: Assignment, person: Option<&str>) -> SpeakerState {
    SpeakerState {
        assignment,
        person_id: person.map(pid),
        score: None,
    }
}

fn auto(person: &str) -> SpeakerState {
    state(Assignment::Auto, Some(person))
}

fn change(meeting: &str, speaker: u32, before: SpeakerState, after: SpeakerState) -> SpeakerChange {
    SpeakerChange {
        meeting_id: mid(meeting),
        speaker_id: sid(speaker),
        before,
        after,
    }
}

#[test]
fn reports_count_each_speaker_once_by_its_net_change() {
    let mut r = LearningReport::default();
    let none = || state(Assignment::None, None);
    r.record(
        &mid(B),
        false,
        &[change(
            B,
            1,
            state(Assignment::Suggested, Some(ANA)),
            none(),
        )],
    );
    r.record(
        &mid(A),
        true,
        &[
            change(A, 1, none(), auto(ANA)),
            change(A, 2, auto(ANA), auto(BIA)),
            change(A, 3, auto(ANA), state(Assignment::Suggested, Some(ANA))),
        ],
    );
    assert_eq!(
        (r.newly_named, r.replaced, r.demoted, r.suggested, r.cleared),
        (1, 1, 1, 0, 1)
    );
    // Source meetings first.
    assert_eq!(r.meetings[0].meeting_id, mid(A));
    // S1 changes again: it counts from its first state.
    r.record(
        &mid(A),
        true,
        &[change(
            A,
            1,
            auto(ANA),
            state(Assignment::Suggested, Some(BIA)),
        )],
    );
    assert_eq!((r.newly_named, r.suggested), (0, 1));
    // S2 back where it started drops out.
    r.record(&mid(A), true, &[change(A, 2, auto(BIA), auto(ANA))]);
    assert_eq!(r.replaced, 0);
    assert_eq!(r.meetings[0].changes.len(), 2);
    // Counts only: no IDs in the summary.
    let line = r.summary();
    assert!(!line.contains("p-") && !line.contains("m-"), "{line}");
    // A meeting whose every change was undone leaves the report.
    r.record(
        &mid(B),
        false,
        &[change(
            B,
            1,
            none(),
            state(Assignment::Suggested, Some(ANA)),
        )],
    );
    assert!(r.meetings.iter().all(|m| m.meeting_id != mid(B)));
}

#[test]
fn evidence_fingerprint_follows_every_voiceprint_change() {
    let (_tmp, lib) = setup();
    let empty = evidence_fingerprint(&lib).unwrap();
    let mut meeting = Meeting::new(mid(A), Source::Import, crate::schema::now());
    let mut s = Speaker::new(sid(1), "t0");
    s.centroid = Some(Centroid::new(MODEL, &[1.0, 0.0], 1));
    meeting.speakers.push(s);
    let sample = crate::ops::weak_sample(&meeting, sid(1), &pid(ANA))
        .unwrap()
        .unwrap();
    lib.write(|tx| tx.save_person(&Person::new(pid(ANA), "Ana Example")))
        .unwrap();
    let person = evidence_fingerprint(&lib).unwrap();
    assert_ne!(person, empty);
    lib.write(|tx| tx.upsert_voiceprints(&pid(ANA), vec![sample.clone()]))
        .unwrap();
    let one = evidence_fingerprint(&lib).unwrap();
    assert_ne!(one, person);
    assert_eq!(evidence_fingerprint(&lib).unwrap(), one, "stable");
    let mut confirmed = sample;
    confirmed.strength = crate::schema::Strength::Confirmed;
    lib.write(|tx| {
        let mut vps = tx.library().load_voiceprints(&pid(ANA))?;
        vps.embeddings = vec![confirmed];
        tx.save_voiceprints(&vps)
    })
    .unwrap();
    assert_ne!(
        evidence_fingerprint(&lib).unwrap(),
        one,
        "same size, new strength"
    );
    lib.write(|tx| tx.delete_person_dir(&pid(ANA))).unwrap();
    assert_eq!(evidence_fingerprint(&lib).unwrap(), empty);
}

#[test]
fn reevaluation_ignores_confirmation_jobs_when_checking_busy_meetings() {
    let (_tmp, lib) = setup();
    let meeting = Meeting::new(mid(A), Source::Import, crate::schema::now());
    let queue = Queue::open(lib.root()).unwrap();
    lib.write(|tx| {
        queue.enqueue(
            tx,
            CONFIRM_KIND,
            "m-0000a1/S1/p-a1a1a1/test-embedder",
            serde_json::json!({ "meeting_id": A }),
        )
    })
    .unwrap();
    assert!(queue.check_meeting_idle(&meeting).is_err());
    queue
        .check_meeting_idle_except(&meeting, &[CONFIRM_KIND])
        .unwrap();
    Queue::check_meeting_idle_readonly(lib.root(), &meeting, &[CONFIRM_KIND]).unwrap();
    // Reevaluation's own job never makes a meeting busy.
    lib.write(|tx| request(tx, MODEL, Some(&mid(A)))).unwrap();
    queue
        .check_meeting_idle_except(&meeting, &[CONFIRM_KIND])
        .unwrap();
}

#[test]
fn evidence_fingerprint_is_order_independent_and_streams_files() {
    // The same evidence written in two orders hashes the same.
    let (_a_tmp, a) = setup();
    let (_b_tmp, b) = setup();
    for (lib, order) in [(&a, [ANA, BIA]), (&b, [BIA, ANA])] {
        for p in order {
            lib.write(|tx| tx.save_person(&Person::new(pid(p), "Synthetic Voice")))
                .unwrap();
        }
    }
    assert_eq!(
        evidence_fingerprint(&a).unwrap(),
        evidence_fingerprint(&b).unwrap()
    );
    // A file spanning several read blocks hashes as its whole content.
    let path = a.root().join("synthetic-blocks.bin");
    let bytes: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        crate::schema::hex(&hash_file(&path).unwrap().unwrap()),
        crate::schema::sha256_hex(&bytes)
    );
    assert_eq!(hash_file(&a.root().join("absent.bin")).unwrap(), None);
}
