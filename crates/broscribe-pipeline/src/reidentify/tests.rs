//! Synthetic regressions for identity reevaluation: invented names, unit
//! vectors as voices, no audio.

use std::path::PathBuf;
use std::sync::Arc;

use broscribe_core::config::{Config, Thresholds};
use broscribe_core::learning::{self, InFlight, LearningStatus, Pass, ReidentifyInput};
use broscribe_core::ops::{self, SpeakerChange, SpeakerState};
use broscribe_core::queue::{CONFIRM_KIND, JOB_JSON, JobState, Queue};
use broscribe_core::schema::{
    self, Assignment, Centroid, Meeting, Person, ProcessingStatus, Segment, Source, Speaker,
    Strength, Track, TrackRole, VoiceprintKind,
};
use broscribe_core::{Library, MeetingId, PersonId, SpeakerId, fsutil, paths};

use super::*;
use crate::testutil::Fixture;

const MODEL: &str = "synthetic-embedder";

/// The configured shape these tests check the gate against (synthetic).
fn usual() -> Thresholds {
    Thresholds {
        auto_name: Some(0.90),
        suggest: Some(0.85),
        margin: Some(0.10),
        merge: None,
        auto_merge: None,
    }
}

fn sid(n: u32) -> SpeakerId {
    SpeakerId::new(n).unwrap()
}

/// A unit voice along axis `i`.
fn e(i: usize) -> Vec<f32> {
    let mut v = vec![0.0; 8];
    v[i] = 1.0;
    v
}

/// A unit voice with cosine `c` to axis `i`, leaning towards axis `j`.
fn tilt(i: usize, j: usize, c: f32) -> Vec<f32> {
    let mut v = vec![0.0; 8];
    v[i] = c;
    v[j] = (1.0 - c * c).sqrt();
    v
}

/// A synthetic library with the configuration the worker reads.
struct Lab {
    fx: Fixture,
    data: PathBuf,
    model: String,
}

impl Lab {
    fn new(model: &str, thresholds: Thresholds) -> Self {
        let fx = Fixture::new();
        let data = fx._tmp.path().join("data");
        let mut config = Config::default();
        config.profile.speaker_embedding = model.to_owned();
        config.identity.insert(model.to_owned(), thresholds);
        config.save(&data).unwrap();
        Self {
            fx,
            data,
            model: model.to_owned(),
        }
    }

    fn lib(&self) -> &Library {
        &self.fx.lib
    }

    fn configure(&self, f: impl FnOnce(&mut Config)) {
        let mut config = Config::load(&self.data).unwrap();
        f(&mut config);
        config.save(&self.data).unwrap();
    }

    /// A `done` meeting with one speaker per voice (S1, S2…), each with one
    /// line; `f` adjusts the speakers before the lines are written.
    fn meeting_with(&self, voices: &[Vec<f32>], f: impl FnOnce(&mut Meeting)) -> MeetingId {
        let id = self.lib().new_meeting_id().unwrap();
        let mut m = Meeting::new(id.clone(), Source::Import, schema::now());
        m.processing.status = ProcessingStatus::Done;
        m.duration_s = 60.0;
        for (i, v) in voices.iter().enumerate() {
            let mut s = Speaker::new(sid(i as u32 + 1), "t0");
            s.talk_s = 3.0;
            s.centroid = Some(Centroid::new(&self.model, v, 1));
            m.speakers.push(s);
        }
        f(&mut m);
        m.segments = m
            .speakers
            .iter()
            .map(|s| {
                let n = s.id.number();
                Segment {
                    speech_overlap: Vec::new(),
                    id: n,
                    track: s.track.clone(),
                    speaker: Some(s.id),
                    start: f64::from(n) * 5.0,
                    end: f64::from(n) * 5.0 + 3.0,
                    text: format!("synthetic line {n}"),
                    words: Vec::new(),
                }
            })
            .collect();
        ops::with_index(self.lib(), |tx, index| {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)?;
            index.upsert_meeting(tx, &m)
        })
        .unwrap();
        id
    }

    fn meeting(&self, voices: &[Vec<f32>]) -> MeetingId {
        self.meeting_with(voices, |_| {})
    }

    /// A person with stored samples not tied to any meeting speaker.
    fn person(&self, name: &str, confirmed: &[Vec<f32>], weak: &[Vec<f32>]) -> PersonId {
        let person = Person::new(self.lib().new_person_id(), name);
        let all: Vec<(&Vec<f32>, Strength)> = confirmed
            .iter()
            .map(|v| (v, Strength::Confirmed))
            .chain(weak.iter().map(|v| (v, Strength::Weak)))
            .collect();
        // A template meeting (never saved) gives each sample its own range.
        let mut t = Meeting::new(
            MeetingId::parse("m-fffff0").unwrap(),
            Source::Import,
            schema::now(),
        );
        for (i, (v, _)) in all.iter().enumerate() {
            let n = i as u32 + 1;
            let mut s = Speaker::new(sid(n), "t0");
            s.centroid = Some(Centroid::new(&self.model, v, 1));
            t.speakers.push(s);
            t.segments.push(Segment {
                speech_overlap: Vec::new(),
                id: n,
                track: "t0".into(),
                speaker: Some(sid(n)),
                start: f64::from(n) * 5.0,
                end: f64::from(n) * 5.0 + 3.0,
                text: String::new(),
                words: Vec::new(),
            });
        }
        let prints = all
            .iter()
            .enumerate()
            .map(|(i, (_, strength))| {
                let mut vp = ops::weak_sample(&t, sid(i as u32 + 1), &person.id)
                    .unwrap()
                    .unwrap();
                vp.strength = *strength;
                vp.source.kind = match strength {
                    Strength::Confirmed => VoiceprintKind::Hint,
                    Strength::Weak => VoiceprintKind::Captions,
                };
                vp.source.meeting_id = None;
                vp
            })
            .collect();
        ops::with_index(self.lib(), |tx, index| {
            tx.save_person(&person)?;
            index.upsert_person(tx, &person)?;
            tx.upsert_voiceprints(&person.id, prints)
        })
        .unwrap();
        person.id
    }

    /// Names a speaker as the app does without audio (centroid fallback).
    fn name(&self, m: &MeetingId, s: u32, who: &str) -> ops::NameOutcome {
        ops::with_index(self.lib(), |tx, index| {
            ops::name_speaker(tx, index, m, sid(s), who, Vec::new(), &self.model)
        })
        .unwrap()
    }

    fn edit(&self, m: &MeetingId, f: impl FnOnce(&mut Meeting)) {
        ops::with_index(self.lib(), |tx, index| {
            let mut meeting = tx.library().load_meeting(m)?;
            f(&mut meeting);
            tx.save_meeting(&meeting)?;
            index.upsert_meeting(tx, &meeting)
        })
        .unwrap();
    }

    fn request(&self, source: Option<&MeetingId>) {
        self.lib()
            .write(|tx| learning::request(tx, &self.model, source))
            .unwrap();
    }

    fn speaker(&self, m: &MeetingId, s: u32) -> Speaker {
        self.lib()
            .load_meeting(m)
            .unwrap()
            .speaker(sid(s))
            .unwrap()
            .clone()
    }

    fn state(&self, m: &MeetingId, s: u32) -> (Assignment, Option<PersonId>) {
        let st = SpeakerState::of(&self.speaker(m, s));
        (st.assignment, st.person_id)
    }

    fn status(&self) -> LearningStatus {
        learning::status(self.lib(), &self.model).unwrap().unwrap()
    }

    fn queue(&self) -> Queue {
        Queue::open(self.lib().root()).unwrap()
    }

    /// One worker run of at most `budget` meetings.
    fn run_with(&self, budget: usize, progress: &mut dyn FnMut(&str)) -> RunOutcome {
        let lease = self
            .queue()
            .claim(self.lib(), &learning::job_id(&self.model), &self.fx.probe)
            .unwrap()
            .expect("the job is claimable");
        let out = run(self.lib(), &lease, &self.data, budget, progress).unwrap();
        lease.release(self.lib()).unwrap();
        out
    }

    fn run(&self, budget: usize) -> RunOutcome {
        self.run_with(budget, &mut |_| {})
    }

    /// Runs until the job is done or waits for busy meetings.
    fn finish(&self) -> Vec<RunOutcome> {
        let mut outs = Vec::new();
        for _ in 0..50 {
            let job = self.queue().get(&learning::job_id(&self.model)).unwrap();
            if !job.is_claimable() {
                return outs;
            }
            outs.push(self.run(MEETINGS_PER_RUN));
        }
        panic!("the reevaluation never ended: {outs:?}");
    }

    fn weak_samples(&self, person: &PersonId) -> Vec<(Option<MeetingId>, Option<SpeakerId>)> {
        self.lib()
            .load_voiceprints(person)
            .unwrap()
            .embeddings
            .into_iter()
            .filter(|v| v.strength == Strength::Weak && v.model == self.model)
            .map(|v| (v.source.meeting_id, v.source.speaker_id))
            .collect()
    }

    fn in_review(&self, m: &MeetingId, s: u32) -> bool {
        ops::review_queue(self.lib(), None)
            .unwrap()
            .iter()
            .any(|r| r.item.meeting_id == *m && r.item.speaker_id == sid(s))
    }
}

/// The reproduced gap: naming a speaker left its identical siblings, here
/// and in another meeting, unassigned until an explicit re-resolve. Now the
/// worker names the source meeting's sibling first (even while the app's
/// confirmation job for that meeting is still queued), then the other
/// meeting, and leaves meetings that are not `done` alone.
#[test]
fn naming_reaches_siblings_in_its_meeting_first_then_the_library() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0), e(1)]);
    let b = lab.meeting(&[e(0)]);
    let pending = lab.meeting_with(&[e(0)], |m| {
        m.processing.status = ProcessingStatus::Analyzed;
    });

    let named = lab.name(&a, 1, "Ana Invented");
    let ana = named.person_id.clone();
    assert_eq!(named.learning.source_meeting, Some(a.clone()));
    // Naming stores neither sibling by itself.
    assert_eq!(lab.state(&a, 2), (Assignment::None, None));
    assert_eq!(lab.state(&b, 1), (Assignment::None, None));
    assert!(lab.in_review(&a, 2) && lab.in_review(&b, 1));

    // The app's confirmation-samples job for this meeting is still queued.
    let confirm = crate::worker::ConfirmInput {
        meeting_id: a.clone(),
        speaker_id: sid(1),
        person_id: ana.clone(),
        model: MODEL.into(),
    };
    let queue = lab.queue();
    let confirm_id = lab
        .lib()
        .write(|tx| queue.enqueue(tx, CONFIRM_KIND, &confirm.key(), confirm.to_json()))
        .unwrap();

    let first = lab.run(1);
    assert!(first.continues, "{first:?}");
    assert_eq!(first.changed, vec![a.clone()]);
    assert_eq!(lab.state(&a, 2), (Assignment::Auto, Some(ana.clone())));
    assert_eq!(lab.state(&b, 1), (Assignment::None, None), "source first");
    assert!(!lab.in_review(&a, 2));
    let status = lab.status();
    assert!(status.source_meetings.is_empty());
    assert_eq!(status.report.as_ref().unwrap().newly_named, 1);
    // The continuation waits behind the confirmation job queued before it.
    let job = queue.get(&learning::job_id(MODEL)).unwrap();
    assert!(job.created_at > queue.get(&confirm_id).unwrap().created_at);

    lab.finish();
    assert_eq!(lab.state(&b, 1), (Assignment::Auto, Some(ana.clone())));
    assert_eq!(lab.state(&a, 3), (Assignment::None, None));
    assert_eq!(lab.state(&pending, 1), (Assignment::None, None));
    let status = lab.status();
    assert_eq!(status.state, JobState::Done);
    assert_eq!((status.requested, status.processed), (1, 1));
    assert!(status.deferred.is_empty(), "not-done meetings are skipped");
    let report = status.report.unwrap();
    assert_eq!((report.newly_named, report.generation), (2, 1));
    assert_eq!(report.meetings[0].meeting_id, a);
    assert!(report.meetings[0].source);
    assert_eq!(report.meetings[1].meeting_id, b);
    let mut weak = lab.weak_samples(&ana);
    weak.sort();
    let mut expected = vec![
        (Some(a.clone()), Some(sid(2))),
        (Some(b.clone()), Some(sid(1))),
    ];
    expected.sort();
    assert_eq!(weak, expected);
    // Its own weak samples requested nothing more.
    assert_eq!(lab.status().requested, 1);
    // No names in the stored job.
    let stored =
        std::fs::read_to_string(queue.job_dir(&learning::job_id(MODEL)).join(JOB_JSON)).unwrap();
    assert!(!stored.contains("Ana Invented"));
}

/// High score without the lead stays a suggestion; correcting the
/// competitor's mislabelled sample (Bia → Ana) removes it before
/// reassessment, and the strong match is then named.
#[test]
fn competitor_within_margin_keeps_it_pending_until_a_correction() {
    let lab = Lab::new(MODEL, usual());
    let z = lab.meeting(&[e(0)]);
    let y = lab.meeting(&[tilt(0, 1, 0.95)]);
    let x = lab.meeting(&[e(0)]);
    let ana = lab.name(&z, 1, "Ana Invented").person_id;
    let bia = lab.name(&y, 1, "Bia Invented").person_id;
    lab.finish();
    let s = lab.speaker(&x, 1);
    assert_eq!(s.assignment, Assignment::Suggested);
    assert_eq!(s.suggestion.as_ref().unwrap().person_id, ana);
    assert!(lab.in_review(&x, 1));

    let corrected = lab.name(&y, 1, "Ana Invented");
    assert_eq!(corrected.previous_person_id, Some(bia.clone()));
    lab.finish();
    assert_eq!(lab.state(&x, 1), (Assignment::Auto, Some(ana)));
    assert!(
        lab.lib()
            .load_voiceprints(&bia)
            .unwrap()
            .embeddings
            .is_empty()
    );
    let report = lab.status().report.unwrap();
    assert_eq!(report.newly_named, 1, "{report:?}");
}

/// Ordinary-pool Auto without confirmed-only agreement is no new Auto:
/// voices known only from weak (caption) samples stay suggestions, teach
/// nothing, and with `suggest` above the score are left unassigned. An
/// existing automatic name the ordinary resolution still gives is kept.
#[test]
fn all_weak_evidence_never_propagates() {
    let lab = Lab::new(MODEL, usual());
    let ana = lab.person("Ana Invented", &[], &[e(0), e(0)]);
    // Score 0.95 against the weak samples: an ordinary Auto (≥ 0.90, no
    // competitor).
    let x = lab.meeting(&[tilt(0, 1, 0.95)]);
    let w = lab.meeting(&[e(0)]);
    lab.edit(&w, |m| {
        let s = m.speaker_mut(sid(1)).unwrap();
        s.assignment = Assignment::Auto;
        s.person_id = Some(ana.clone());
        s.score = Some(1.0);
    });
    lab.request(None);
    lab.finish();
    let s = lab.speaker(&x, 1);
    assert_eq!(s.assignment, Assignment::Suggested);
    assert_eq!(s.suggestion.unwrap().person_id, ana);
    assert_eq!(lab.state(&w, 1), (Assignment::Auto, Some(ana.clone())));
    assert!(
        lab.weak_samples(&ana)
            .iter()
            .all(|(m, _)| m.as_ref() != Some(&x)),
        "an unconfirmed voice teaches nothing"
    );

    // A `suggest` above the score yields no suggestion below it.
    lab.configure(|c| {
        c.identity.insert(
            MODEL.into(),
            Thresholds {
                suggest: Some(0.97),
                ..usual()
            },
        );
    });
    lab.request(None);
    lab.finish();
    assert_eq!(lab.state(&x, 1), (Assignment::None, None));
}

/// Few confirmed samples plus weak ones: both pools agree, so the name is
/// published.
#[test]
fn few_confirmed_samples_with_weak_ones_still_name() {
    let lab = Lab::new(MODEL, usual());
    let ana = lab.person("Ana Invented", &[tilt(0, 1, 0.93)], &[e(0), e(0)]);
    let x = lab.meeting(&[e(0)]);
    lab.request(None);
    lab.finish();
    assert_eq!(lab.state(&x, 1), (Assignment::Auto, Some(ana)));
}

/// The paired microphone's speakers, rejections, confirmations and review
/// dismissals keep their protections.
#[test]
fn mic_rejection_confirmation_and_dismissal_are_protected() {
    let lab = Lab::new(MODEL, usual());
    let ana = lab.person("Ana Invented", &[e(0)], &[]);
    let bia = lab.person("Bia Invented", &[e(1)], &[]);
    let dismissed_at = schema::now();
    let m = lab.meeting_with(&[e(0), e(0), e(0), e(0), e(0)], |m| {
        m.setup = Some("headphones".into());
        m.tracks = vec![
            Track {
                id: "t0".into(),
                role: TrackRole::Mic,
            },
            Track {
                id: "t1".into(),
                role: TrackRole::System,
            },
        ];
        for s in m.speakers.iter_mut().skip(1) {
            s.track = "t1".into();
        }
        m.speaker_mut(sid(3)).unwrap().rejected_person_ids = vec![ana.clone()];
        m.speaker_mut(sid(4)).unwrap().review_dismissed_at = Some(dismissed_at);
        let s5 = m.speaker_mut(sid(5)).unwrap();
        s5.assignment = Assignment::Confirmed;
        s5.person_id = Some(bia.clone());
    });
    lab.request(Some(&m));
    lab.finish();
    // Unknown mic speech stays unassigned.
    assert_eq!(lab.state(&m, 1), (Assignment::None, None));
    assert_eq!(lab.state(&m, 2), (Assignment::Auto, Some(ana.clone())));
    assert_ne!(lab.state(&m, 3).1, Some(ana.clone()));
    assert_eq!(lab.speaker(&m, 3).rejected_person_ids, vec![ana.clone()]);
    let s4 = lab.speaker(&m, 4);
    assert_eq!(s4.assignment, Assignment::Auto);
    assert_eq!(s4.review_dismissed_at, Some(dismissed_at));
    assert_eq!(lab.state(&m, 5), (Assignment::Confirmed, Some(bia)));
    assert!(
        !lab.weak_samples(&ana)
            .contains(&(Some(m.clone()), Some(sid(1))))
    );
}

/// Names a meeting's captions proved survive reevaluation under strict
/// thresholds, with their caption sample unchanged.
#[test]
fn caption_proven_names_survive_reevaluation() {
    use crate::caption_replay::tests::{caption_models, caption_named_meeting, niko, second_voice};
    let model = caption_models().embedder.model_id().to_owned();
    let lab = Lab::new(
        &model,
        Thresholds {
            auto_name: Some(0.99),
            suggest: Some(0.98),
            margin: Some(0.5),
            merge: None,
            auto_merge: None,
        },
    );
    let id = caption_named_meeting(&lab.fx);
    let person = niko(&lab.fx);
    let samples = || {
        lab.lib()
            .load_voiceprints(&person.id)
            .unwrap()
            .embeddings
            .into_iter()
            .map(|v| (v.id, v.vector_b64, v.strength))
            .collect::<Vec<_>>()
    };
    let before = samples();
    lab.request(Some(&id));
    lab.finish();
    let s = second_voice(&lab.fx, &id);
    assert_eq!(
        (s.assignment, s.person_id),
        (Assignment::Auto, Some(person.id.clone()))
    );
    assert_eq!(samples(), before);
}

/// Two confirmations while a pass is running: both are processed (their
/// meetings first), none is lost, and the generation catches up.
#[test]
fn quick_confirmations_during_a_running_pass_are_all_processed() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0)]);
    let b = lab.meeting(&[e(1), e(1)]);
    let c = lab.meeting(&[e(2), e(2)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    let mut asked = false;
    let mut later = Vec::new();
    let out = lab.run_with(MEETINGS_PER_RUN, &mut |line| {
        assert!(!line.contains("Invented"), "{line}");
        if !asked {
            asked = true;
            later.push(lab.name(&b, 1, "Bia Invented").person_id);
            later.push(lab.name(&c, 1, "Cai Invented").person_id);
        }
    });
    assert!(asked, "{out:?}");
    lab.finish();
    assert_eq!(lab.state(&a, 2), (Assignment::Auto, Some(ana)));
    assert_eq!(lab.state(&b, 2), (Assignment::Auto, Some(later[0].clone())));
    assert_eq!(lab.state(&c, 2), (Assignment::Auto, Some(later[1].clone())));
    let status = lab.status();
    assert_eq!((status.requested, status.processed), (3, 3));
    assert_eq!(status.state, JobState::Done);
    assert_eq!(status.report.unwrap().newly_named, 3);
}

/// A `done` meeting another job works on is deferred, not declared
/// complete; the job then waits instead of looping, and retries it later.
#[test]
fn busy_meetings_are_deferred_and_retried_without_a_hot_loop() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0)]);
    let b = lab.meeting(&[e(0)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    let queue = lab.queue();
    let reprocess = lab
        .lib()
        .write(|tx| {
            queue.enqueue(
                tx,
                "reprocess",
                b.as_str(),
                serde_json::json!({ "meeting_id": b }),
            )
        })
        .unwrap();
    let outs = lab.finish();
    assert!(
        outs.last().unwrap().message.contains("1 waiting"),
        "{outs:?}"
    );
    assert_eq!(lab.state(&a, 2), (Assignment::Auto, Some(ana.clone())));
    assert_eq!(lab.state(&b, 1), (Assignment::None, None));
    let status = lab.status();
    assert_eq!(status.deferred, vec![b.clone()]);
    assert_eq!(status.report.as_ref().unwrap().deferred, 1);
    assert_eq!(status.state, JobState::Queued);
    assert!(status.not_before.is_some_and(|t| t > schema::now()));
    let id = learning::job_id(MODEL);
    assert!(!queue.get(&id).unwrap().is_claimable(), "no hot loop");

    // The other job ends; the wait elapses.
    lab.lib()
        .write(|tx| queue.set_state(tx, &reprocess, JobState::Done, None))
        .unwrap();
    let mut job = queue.get(&id).unwrap();
    job.not_before = Some(schema::now() - chrono::TimeDelta::seconds(1));
    fsutil::write_json_atomic(&queue.job_dir(&id).join(JOB_JSON), &job).unwrap();
    lab.finish();
    assert_eq!(lab.state(&b, 1), (Assignment::Auto, Some(ana)));
    let status = lab.status();
    assert_eq!(status.state, JobState::Done);
    assert!(status.deferred.is_empty());
    assert_eq!(status.report.unwrap().deferred, 0);
}

fn auto_as(person: &PersonId) -> SpeakerState {
    SpeakerState {
        assignment: Assignment::Auto,
        person_id: Some(person.clone()),
        score: None,
    }
}

fn unassigned() -> SpeakerState {
    SpeakerState {
        assignment: Assignment::None,
        person_id: None,
        score: None,
    }
}

/// What a run that crashed after saving `meeting.json` (S2 named) but
/// before its index row, weak sample and checkpoint leaves; S3's planned
/// change never reached the file.
fn crash_after_meeting_file(lab: &Lab, a: &MeetingId, ana: &PersonId) {
    let lease = lab
        .queue()
        .claim(lab.lib(), &learning::job_id(MODEL), &lab.fx.probe)
        .unwrap()
        .unwrap();
    let change = |s: u32| SpeakerChange {
        meeting_id: a.clone(),
        speaker_id: sid(s),
        before: unassigned(),
        after: auto_as(ana),
    };
    lab.lib()
        .write(|tx| {
            lease.update_in(tx, None, |v| {
                let mut input = ReidentifyInput::of_value(v).unwrap();
                input.pass = Some(Pass::new(input.requested, Some(usual())));
                input.in_flight = Some(InFlight {
                    meeting_id: a.clone(),
                    source: true,
                    changes: vec![change(2), change(3)],
                });
                *v = input.to_value()?;
                Ok(())
            })?;
            let mut m = tx.library().load_meeting(a)?;
            let s = m.speaker_mut(sid(2)).unwrap();
            s.assignment = Assignment::Auto;
            s.person_id = Some(ana.clone());
            s.score = Some(1.0);
            tx.save_meeting(&m)
        })
        .unwrap();
    lease.release(lab.lib()).unwrap();
}

/// Restart after a crash between the meeting file and its derived state:
/// the index and weak sample are repaired, the change counted once, and the
/// unwritten change planned again.
#[test]
fn restart_repairs_a_partial_publication_and_counts_it_once() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0), e(0)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    crash_after_meeting_file(&lab, &a, &ana);
    assert!(lab.in_review(&a, 2), "the index missed the saved name");
    assert!(lab.weak_samples(&ana).is_empty());

    lab.finish();
    assert!(!lab.in_review(&a, 2));
    assert_eq!(lab.state(&a, 3), (Assignment::Auto, Some(ana.clone())));
    let mut weak = lab.weak_samples(&ana);
    weak.sort();
    assert_eq!(
        weak,
        vec![
            (Some(a.clone()), Some(sid(2))),
            (Some(a.clone()), Some(sid(3)))
        ]
    );
    let status = lab.status();
    assert_eq!(status.state, JobState::Done);
    let report = status.report.unwrap();
    assert_eq!(report.newly_named, 2, "{report:?}");
    // Idempotent: another pass changes nothing.
    lab.request(None);
    lab.finish();
    assert_eq!(lab.weak_samples(&ana).len(), 2);
}

#[test]
fn unreadable_partial_publication_keeps_its_repair_checkpoint() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0), e(0)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    crash_after_meeting_file(&lab, &a, &ana);
    let path = lab.lib().meeting_dir(&a).unwrap().join(paths::MEETING_JSON);
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"{broken synthetic meeting").unwrap();
    let lease = lab
        .queue()
        .claim(lab.lib(), &learning::job_id(MODEL), &lab.fx.probe)
        .unwrap()
        .unwrap();
    let error = run(lab.lib(), &lease, &lab.data, MEETINGS_PER_RUN, &mut |_| {}).unwrap_err();
    assert!(error.to_string().contains("identity repair pending"));
    let stored = lab.queue().get(&learning::job_id(MODEL)).unwrap();
    assert!(ReidentifyInput::of(&stored).unwrap().in_flight.is_some());
    lease.release(lab.lib()).unwrap();
    std::fs::write(path, original).unwrap();
    lab.finish();
    assert!(!lab.in_review(&a, 2));
    assert_eq!(lab.state(&a, 3), (Assignment::Auto, Some(ana.clone())));
    assert_eq!(lab.weak_samples(&ana).len(), 2);
    assert_eq!(lab.status().report.unwrap().newly_named, 2);
}

/// Recovery never restores a forgotten voice nor overrides a decision the
/// user made after the crash.
#[test]
fn recovery_keeps_forgets_and_newer_decisions() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0), e(0)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    crash_after_meeting_file(&lab, &a, &ana);
    // After the crash: the user names S2 Bia, and forgets Ana's voice.
    let bia = lab.name(&a, 2, "Bia Invented").person_id;
    ops::with_index(lab.lib(), |tx, index| {
        ops::forget_voice(tx, index, ana.as_str())
    })
    .unwrap();
    lab.finish();
    assert_eq!(lab.state(&a, 2), (Assignment::Confirmed, Some(bia)));
    assert!(
        !lab.lib()
            .person_dir(&ana)
            .join(paths::VOICEPRINTS_JSON)
            .exists(),
        "the forgotten voice is not restored"
    );
    let report = lab.status().report.unwrap();
    assert!(
        report
            .meetings
            .iter()
            .flat_map(|m| &m.changes)
            .all(|c| c.speaker_id != sid(2)),
        "{report:?}"
    );
}

/// Thresholds changed during a pass: the pass restarts, and no meeting is
/// published with the old ones after the change.
#[test]
fn a_threshold_change_during_a_pass_restarts_it() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0)]);
    let b = lab.meeting(&[tilt(0, 1, 0.95)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    let mut changed = false;
    lab.run_with(MEETINGS_PER_RUN, &mut |_| {
        if !changed {
            changed = true;
            lab.configure(|c| {
                c.identity.insert(
                    MODEL.into(),
                    Thresholds {
                        auto_name: Some(0.99),
                        suggest: Some(0.90),
                        ..usual()
                    },
                );
            });
        }
    });
    assert!(changed);
    lab.finish();
    assert_eq!(lab.state(&a, 2), (Assignment::Auto, Some(ana.clone())));
    let s = lab.speaker(&b, 1);
    assert_eq!(s.assignment, Assignment::Suggested);
    assert_eq!(s.suggestion.unwrap().person_id, ana);
    assert_eq!(lab.status().state, JobState::Done);
}

/// A model switch during a pass ends it: nothing more is published for the
/// old model.
#[test]
fn a_model_switch_during_a_pass_ends_it() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0)]);
    let b = lab.meeting(&[e(0)]);
    lab.name(&a, 1, "Ana Invented");
    let mut switched = false;
    let out = lab.run_with(MEETINGS_PER_RUN, &mut |_| {
        if !switched {
            switched = true;
            lab.configure(|c| c.profile.speaker_embedding = "another-embedder".into());
        }
    });
    assert!(switched);
    assert_eq!(
        out.message,
        "skipped: the active speaker-embedding model changed"
    );
    assert_eq!(lab.state(&b, 1), (Assignment::None, None));
    assert_eq!(lab.status().state, JobState::Done);
}

/// A crash left a publication half done, then the active model changed:
/// the run still repairs what reached `meeting.json` (index row, weak
/// sample, counted once) before the switch ends the job, and plans
/// nothing more for the old model.
#[test]
fn a_model_switch_still_repairs_an_interrupted_publication_first() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0), e(0)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    crash_after_meeting_file(&lab, &a, &ana);
    assert!(lab.in_review(&a, 2));
    lab.configure(|c| c.profile.speaker_embedding = "another-embedder".into());
    let outs = lab.finish();
    assert_eq!(
        outs.last().unwrap().message,
        "skipped: the active speaker-embedding model changed"
    );
    assert!(outs.iter().any(|o| o.changed.contains(&a)), "{outs:?}");
    assert!(
        !lab.in_review(&a, 2),
        "the saved name's index row is repaired"
    );
    assert_eq!(
        lab.weak_samples(&ana),
        vec![(Some(a.clone()), Some(sid(2)))]
    );
    // S3's unwritten change is not planned again for the old model.
    assert_eq!(lab.state(&a, 3), (Assignment::None, None));
    let status = lab.status();
    assert_eq!(status.state, JobState::Done);
    let stored = lab.queue().get(&learning::job_id(MODEL)).unwrap();
    assert!(ReidentifyInput::of(&stored).unwrap().in_flight.is_none());
}

/// Forgetting a person during a pass invalidates it: the pass restarts on
/// the remaining evidence and nothing names or teaches the forgotten
/// person again.
#[test]
fn forgetting_during_a_pass_invalidates_it() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0)]);
    let b = lab.meeting(&[e(0)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    let mut forgot = false;
    lab.run_with(MEETINGS_PER_RUN, &mut |_| {
        if !forgot {
            forgot = true;
            ops::with_index(lab.lib(), |tx, index| {
                ops::forget_person(tx, index, ana.as_str())
            })
            .unwrap();
        }
    });
    assert!(forgot);
    lab.finish();
    for (m, s) in [(&a, 1), (&a, 2), (&b, 1)] {
        let st = lab.speaker(m, s);
        assert_ne!(st.person_id, Some(ana.clone()));
        assert!(st.suggestion.is_none_or(|x| x.person_id != ana));
    }
    assert!(!lab.lib().person_dir(&ana).exists());
    let status = lab.status();
    assert_eq!(status.state, JobState::Done);
    assert_eq!(status.processed, status.requested);
}

/// The queue worker runs reevaluation like any other kind and reports the
/// meetings it changed for retention and search.
#[test]
fn the_queue_worker_runs_reevaluation() {
    use crate::worker::{WORKER_KINDS, WorkerEventKind, WorkerOptions, work_queue};
    assert!(WORKER_KINDS.contains(&REIDENTIFY_KIND));
    assert_eq!(
        broscribe_core::queue::kind_label(REIDENTIFY_KIND, &serde_json::json!({})),
        "Checking other speakers"
    );
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0)]);
    let b = lab.meeting(&[e(0)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    let opts = WorkerOptions::new(
        lab.fx._tmp.path().join("cache"),
        Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
        lab.data.clone(),
        Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
        Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
    );
    let mut events = Vec::new();
    let summary = work_queue(
        &lab.fx.env(),
        &broscribe_models::fakes::FakeModelProvider::default(),
        &opts,
        &mut |e| events.push(e.clone()),
    )
    .unwrap();
    assert_eq!(summary.failed, 0, "{events:?}");
    assert!(
        events
            .iter()
            .any(|e| e.event == WorkerEventKind::JobDone
                && e.kind.as_deref() == Some(REIDENTIFY_KIND)),
        "{events:?}"
    );
    let mut changed = summary.reidentified.clone();
    changed.sort();
    let mut expected = vec![a.clone(), b.clone()];
    expected.sort();
    assert_eq!(changed, expected);
    assert_eq!(lab.state(&b, 1), (Assignment::Auto, Some(ana)));
    let progress: Vec<_> = events
        .iter()
        .filter(|e| e.event == WorkerEventKind::Progress)
        .filter_map(|e| e.message.as_deref())
        .collect();
    assert!(!progress.is_empty());
    assert!(
        progress.iter().all(|m| !m.contains("Invented")),
        "{progress:?}"
    );
    assert_eq!(lab.status().state, JobState::Done);
}

fn reprocessing(lab: &Lab, m: &MeetingId) -> broscribe_core::queue::JobId {
    let queue = lab.queue();
    lab.lib()
        .write(|tx| {
            queue.enqueue(
                tx,
                "reprocess",
                m.as_str(),
                serde_json::json!({ "meeting_id": m }),
            )
        })
        .unwrap()
}

/// Changes committed for a source meeting show in the report under the
/// request generation they answer at once, before the pass ends (the app
/// shows only a report of its own request's generation), also while
/// another meeting waits for its job and a later request arrives.
#[test]
fn committed_source_results_show_their_generation_before_the_pass_ends() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0)]);
    let b = lab.meeting(&[e(0)]);
    let c = lab.meeting(&[e(1), e(1)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    reprocessing(&lab, &b);

    // One meeting: the source, before the sweep.
    assert!(lab.run(1).continues);
    assert_eq!(lab.state(&a, 2), (Assignment::Auto, Some(ana.clone())));
    let status = lab.status();
    assert_eq!((status.requested, status.processed), (1, 0));
    let report = status.report.unwrap();
    assert_eq!(report.generation, 1, "{report:?}");
    assert_eq!(report.meetings.len(), 1);
    assert_eq!(report.meetings[0].meeting_id, a);
    assert!(report.meetings[0].source);

    // The pass ends with b waiting; the job is not idle.
    lab.finish();
    let status = lab.status();
    assert_eq!(status.deferred, vec![b.clone()]);
    assert_eq!(status.state, JobState::Queued);

    // A second naming while b waits: its source's changes show under the
    // new generation as soon as they are written, earlier ones kept.
    let bia = lab.name(&c, 1, "Bia Invented").person_id;
    assert!(lab.run(1).continues);
    assert_eq!(lab.state(&c, 2), (Assignment::Auto, Some(bia)));
    let status = lab.status();
    assert_eq!((status.requested, status.processed), (2, 1));
    let report = status.report.unwrap();
    assert_eq!(report.generation, 2, "{report:?}");
    assert_eq!(report.newly_named, 2);
    assert!(
        report
            .meetings
            .iter()
            .any(|m| m.meeting_id == c && m.source)
    );
}

/// A search-vector refresh queued for the source meeting (as a speaker
/// separation queues one) or a playback cache of its recording (as opening
/// its player queues one) never defers it: neither writes the meeting.
#[test]
fn a_queued_search_refresh_or_playback_cache_never_defers_the_source() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    let queue = lab.queue();
    let (key, input) = crate::worker::vectors_job_input(&a);
    let (refresh, playback) = lab
        .lib()
        .write(|tx| {
            let refresh = queue.enqueue(tx, broscribe_core::queue::VECTORS_KIND, &key, input)?;
            let playback = queue.enqueue(
                tx,
                broscribe_core::queue::PLAYBACK_KIND,
                a.as_str(),
                serde_json::json!({ "meeting_id": a }),
            )?;
            Ok((refresh, playback))
        })
        .unwrap();

    let first = lab.run(1);
    assert_eq!(first.changed, vec![a.clone()]);
    assert_eq!(lab.state(&a, 2), (Assignment::Auto, Some(ana)));
    assert!(lab.status().deferred.is_empty());
    lab.finish();
    let status = lab.status();
    assert_eq!(status.state, JobState::Done);
    assert!(status.deferred.is_empty());
    for job in [refresh, playback] {
        assert_eq!(
            queue.get(&job).unwrap().state,
            JobState::Queued,
            "the other job is left to run"
        );
    }
}

/// A source meeting deferred while busy and then reached free by the sweep
/// of the same pass waits no more: the job ends instead of retrying it.
#[test]
fn a_deferred_meeting_the_sweep_processes_waits_no_more() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    let reprocess = reprocessing(&lab, &a);

    assert!(lab.run(1).continues);
    assert_eq!(lab.status().deferred, vec![a.clone()]);
    assert_eq!(lab.state(&a, 2), (Assignment::None, None));

    let queue = lab.queue();
    lab.lib()
        .write(|tx| queue.set_state(tx, &reprocess, JobState::Done, None))
        .unwrap();
    lab.finish();
    assert_eq!(lab.state(&a, 2), (Assignment::Auto, Some(ana)));
    let status = lab.status();
    assert!(status.deferred.is_empty());
    assert_eq!(status.state, JobState::Done, "nothing waits");
    let report = status.report.unwrap();
    assert_eq!((report.deferred, report.newly_named), (0, 1));
}

/// A meeting whose file cannot be read is reported by ID and error code
/// and skipped; every other meeting is still reassessed, the job ends, and
/// nothing of the meeting's folder (its title) reaches the queue. The next
/// request tries it again.
#[test]
fn an_unreadable_meeting_is_reported_and_never_stops_the_rest() {
    let lab = Lab::new(MODEL, usual());
    let a = lab.meeting(&[e(0), e(0)]);
    let broken = lab.meeting_with(&[e(0)], |m| m.title = Some("Private Invented Title".into()));
    let b = lab.meeting(&[e(0)]);
    let c = lab.meeting(&[e(0)]);
    let ana = lab.name(&a, 1, "Ana Invented").person_id;
    let dir = lab.lib().meeting_dir(&broken).unwrap();
    let file = dir.join(paths::MEETING_JSON);
    let original = std::fs::read(&file).unwrap();
    std::fs::write(&file, b"{ damaged").unwrap();

    lab.finish();
    assert_eq!(lab.state(&a, 2), (Assignment::Auto, Some(ana.clone())));
    assert_eq!(lab.state(&b, 1), (Assignment::Auto, Some(ana.clone())));
    assert_eq!(lab.state(&c, 1), (Assignment::Auto, Some(ana.clone())));
    let status = lab.status();
    assert_eq!(status.state, JobState::Done);
    assert!(status.error.is_none());
    assert_eq!((status.requested, status.processed), (1, 1));
    let report = status.report.unwrap();
    assert_eq!(
        report.unreadable,
        vec![learning::UnreadableMeeting {
            meeting_id: broken.clone(),
            code: "schema".into(),
        }]
    );
    assert!(
        report.summary().ends_with("; 1 unreadable"),
        "{}",
        report.summary()
    );
    let stored =
        std::fs::read_to_string(lab.queue().job_dir(&learning::job_id(MODEL)).join(JOB_JSON))
            .unwrap();
    let folder = dir.file_name().unwrap().to_str().unwrap();
    for private in [
        folder,
        "private-invented-title",
        "Private Invented Title",
        "Ana Invented",
    ] {
        assert!(!stored.contains(private), "{private} reached the queue");
    }

    // Repaired, the next request reassesses it.
    std::fs::write(&file, original).unwrap();
    lab.request(None);
    lab.finish();
    assert_eq!(lab.state(&broken, 1), (Assignment::Auto, Some(ana)));
    let report = lab.status().report.unwrap();
    assert!(report.unreadable.is_empty(), "{report:?}");
}
