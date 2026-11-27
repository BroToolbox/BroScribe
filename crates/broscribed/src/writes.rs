//! Library writes requested over IPC, through `core::ops` in one short
//! [`Library::write`] each (design §3.1: when the daemon runs, it is the
//! writer). Nothing here waits on a child process or runs inference:
//! naming uses the centroid fallback and queues the `confirm-samples` job,
//! as the app does, and core requests the speaker recheck (`reidentify`)
//! the scheduler runs after the caller's kick; retention for the touched
//! meetings is started by the caller afterwards, outside the lock.

use std::path::Path;

use broscribe_core::config::Config;
use broscribe_core::ops;
use broscribe_core::queue::{CONFIRM_KIND, JobId, Queue, VECTORS_KIND};
use broscribe_core::{Error, Library, MeetingId, Result, SpeakerId};
use serde_json::{Value, json};

/// Queues the meeting's `refresh-vectors` job, in the identity write's
/// transaction, when that write separated speakers from a group: their
/// lines now show as another speaker, so the search windows changed. The
/// daemon kicks the worker after every write.
fn queue_vector_refresh(
    tx: &mut broscribe_core::WriteTx<'_>,
    meeting: &MeetingId,
    separated: &[ops::Separation],
) -> Result<Option<JobId>> {
    if separated.is_empty() {
        return Ok(None);
    }
    Queue::open(tx.root())?
        .enqueue(
            tx,
            VECTORS_KIND,
            meeting.as_str(),
            json!({ "meeting_id": meeting }),
        )
        .map(Some)
}

/// A write's JSON result and the meetings whose retention may have changed.
pub struct WriteOutcome {
    pub value: Value,
    pub touched: Vec<MeetingId>,
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| Error::Internal(format!("serialising: {e}")))
}

/// Tag changes as `tag` takes them: `+a` or `a` adds, `-b` removes. Tags
/// are trimmed; an empty one is refused.
pub fn tag_changes(changes: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut add = Vec::new();
    let mut remove = Vec::new();
    for change in changes {
        let (list, tag) = match change.trim() {
            c if c.starts_with('-') => (&mut remove, &c[1..]),
            c => (&mut add, c.strip_prefix('+').unwrap_or(c)),
        };
        let tag = tag.trim();
        if tag.is_empty() {
            return Err(Error::InvalidInput(format!("empty tag in {change:?}")));
        }
        list.push(tag.to_owned());
    }
    Ok((add, remove))
}

/// `tag <meeting> +a -b`.
pub fn meeting_tag(
    lib: &Library,
    meeting: &str,
    add: &[String],
    remove: &[String],
) -> Result<WriteOutcome> {
    let id = MeetingId::parse(meeting)?;
    let tags = ops::with_index(lib, |tx, index| ops::tag(tx, index, &id, add, remove))?;
    Ok(WriteOutcome {
        value: json!({ "meeting_id": id, "tags": tags }),
        touched: Vec::new(),
    })
}

/// `speakers name`: the centroid naming now and the `confirm-samples` job in
/// the same transaction; core requests the speaker recheck (`learning` in
/// the response) that the scheduler runs once the caller kicks it. A
/// grouped member names its group's kept speaker (`ops::name_speaker`): the
/// job and the response use the speaker actually named, and say which
/// member was asked for.
pub fn speaker_name(
    lib: &Library,
    config: &Config,
    meeting: &str,
    speaker: &str,
    person: &str,
) -> Result<WriteOutcome> {
    let mid = MeetingId::parse(meeting)?;
    let sid = SpeakerId::parse(speaker)?;
    let model = config.profile.speaker_embedding.clone();
    let (outcome, job, refresh) = ops::with_index(lib, |tx, index| {
        let outcome = ops::name_speaker(tx, index, &mid, sid, person, Vec::new(), &model)?;
        // The speaker actually named: the group's kept speaker for a member.
        let named = outcome.speaker_id;
        let key = format!("{mid}/{named}/{}/{model}", outcome.person_id);
        let job = Queue::open(tx.root())?.enqueue(
            tx,
            CONFIRM_KIND,
            &key,
            json!({
                "meeting_id": mid,
                "speaker_id": named.to_string(),
                "person_id": outcome.person_id,
                "model": model,
            }),
        )?;
        let refresh = queue_vector_refresh(tx, &mid, &outcome.separated)?;
        Ok((outcome, job, refresh))
    })?;
    let people = lib.people_lookup()?;
    let mut value = to_value(&outcome)?;
    value["meeting_id"] = json!(mid);
    value["speaker_id"] = json!(outcome.speaker_id.to_string());
    value["person_name"] = json!(people.display_name(&outcome.person_id));
    value["confirm_job_id"] = json!(job);
    if let Some(refresh) = refresh {
        value["refresh_vectors_job_id"] = json!(refresh);
    }
    Ok(WriteOutcome {
        value,
        touched: vec![mid],
    })
}

/// `speakers reject`.
pub fn speaker_reject(
    lib: &Library,
    config: &Config,
    meeting: &str,
    speaker: &str,
    person: Option<&str>,
) -> Result<WriteOutcome> {
    let mid = MeetingId::parse(meeting)?;
    let sid = SpeakerId::parse(speaker)?;
    let model = config.profile.speaker_embedding.clone();
    let thresholds = config.thresholds(&model).copied();
    let (outcome, refresh) = ops::with_index(lib, |tx, index| {
        let outcome =
            ops::reject_suggestion(tx, index, &mid, sid, person, &model, thresholds.as_ref())?;
        let refresh = queue_vector_refresh(tx, &mid, &outcome.separated)?;
        Ok((outcome, refresh))
    })?;
    let mut value = to_value(&outcome)?;
    value["meeting_id"] = json!(mid);
    value["speaker_id"] = json!(outcome.speaker_id.to_string());
    if let Some(refresh) = refresh {
        value["refresh_vectors_job_id"] = json!(refresh);
    }
    Ok(WriteOutcome {
        value,
        touched: vec![mid],
    })
}

/// `people forget` / `people forget-voice` (the kept enrollment passage
/// goes with the voice). Once the forget committed, the directory photo
/// purge never fails it: as in the CLI and app, `photos` reports the purge
/// (`complete`, or `partial` with sanitized per-integration failures) and
/// `photos_error` a purge that could not run, beside the success.
pub fn person_forget(
    lib: &Library,
    data_dir: &Path,
    person: &str,
    voice_only: bool,
) -> Result<WriteOutcome> {
    let outcome = ops::with_index(lib, |tx, index| {
        if voice_only {
            ops::forget_voice(tx, index, person)
        } else {
            ops::forget_person(tx, index, person)
        }
    })?;
    let mut value = to_value(&outcome)?;
    if !voice_only {
        match broscribe_integrations::forget_person_photos(data_dir, outcome.person_id.as_str()) {
            Ok(purge) => value["photos"] = to_value(&purge)?,
            Err(error) => {
                tracing::warn!(
                    code = error.code(),
                    "cached photos of a forgotten person remain"
                );
                value["photos"] = json!({
                    "status": broscribe_integrations::PhotoPurgeStatus::Partial
                });
                value["photos_error"] = to_value(&error.public())?;
            }
        }
    }
    Ok(WriteOutcome {
        value,
        touched: Vec::new(),
    })
}

/// Enqueues a job (existing `(kind, key)` jobs are returned unchanged).
pub fn enqueue(lib: &Library, kind: &str, key: &str, input: Value) -> Result<JobId> {
    lib.write(|tx| Queue::open(tx.root())?.enqueue(tx, kind, key, input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_tag_changes_take_plus_minus_and_refuse_empty_tags() {
        let changes =
            |c: &[&str]| tag_changes(&c.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let (add, remove) = changes(&["+a", "b ", "-c", " - d"]).unwrap();
        assert_eq!(
            (add, remove),
            (vec!["a".into(), "b".into()], vec!["c".into(), "d".into()])
        );
        for bad in ["", "  ", "+", "-", "- "] {
            assert_eq!(
                changes(&[bad]).unwrap_err().code(),
                "invalid_input",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn enqueue_is_idempotent_and_tag_needs_a_meeting() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let a = enqueue(&lib, broscribe_core::queue::PLAYBACK_KIND, "k", json!({})).unwrap();
        let b = enqueue(
            &lib,
            broscribe_core::queue::PLAYBACK_KIND,
            "k",
            json!({"other": 1}),
        )
        .unwrap();
        assert_eq!(a, b);
        let err = meeting_tag(&lib, "m-000001", &["x".into()], &[])
            .err()
            .unwrap();
        assert_eq!(err.code(), "not_found");
    }

    #[test]
    fn record_forget_person_purges_integration_photos_but_forget_voice_keeps_them() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let lib = Library::open(tmp.path().join("library")).unwrap();
        let person = broscribe_core::schema::Person::new(lib.new_person_id(), "Alex Example");
        lib.write(|tx| tx.save_person(&person)).unwrap();
        let mut photos = Vec::new();
        for integration in broscribe_integrations::IntegrationId::ALL {
            let dir = broscribe_integrations::account_dir(&data, integration).join("photos");
            std::fs::create_dir_all(&dir).unwrap();
            let photo = dir.join(format!("{}.img", person.id));
            std::fs::write(&photo, b"synthetic image").unwrap();
            photos.push(photo);
        }
        person_forget(&lib, &data, person.id.as_str(), true).unwrap();
        assert!(photos.iter().all(|path| path.exists()));
        person_forget(&lib, &data, person.id.as_str(), false).unwrap();
        assert!(photos.iter().all(|path| !path.exists()));
    }

    /// IN14: a photo store that cannot be purged after the forget committed is
    /// reported beside the success, sanitized, and never undoes the forget.
    #[test]
    fn a_partial_photo_purge_after_forget_is_reported_beside_the_success() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let lib = Library::open(tmp.path().join("library")).unwrap();
        let person = broscribe_core::schema::Person::new(lib.new_person_id(), "Alex Example");
        lib.write(|tx| tx.save_person(&person)).unwrap();
        let google = broscribe_integrations::account_dir(
            &data,
            broscribe_integrations::IntegrationId::Google,
        );
        // Synthetic obstruction: a directory where a photo file would be.
        let blocked = google
            .join("photos")
            .join("2")
            .join(format!("{}.img", person.id));
        std::fs::create_dir_all(&blocked).unwrap();
        let out = person_forget(&lib, &data, person.id.as_str(), false).unwrap();
        assert_eq!(out.value["person_id"], json!(person.id));
        assert_eq!(out.value["photos"]["status"], "partial");
        assert_eq!(out.value["photos"]["failures"][0]["integration"], "google");
        let text = out.value.to_string();
        assert!(
            !text.contains(tmp.path().to_string_lossy().as_ref()),
            "{text}"
        );
        assert!(
            lib.load_person(&person.id).is_err(),
            "the forget stays committed"
        );
    }

    /// Naming a grouped member over IPC names the group's kept speaker: the
    /// `confirm-samples` job is keyed and filled with the kept speaker, the
    /// response reports it (and the member asked for), and the sample stored
    /// now carries the kept speaker's provenance.
    #[test]
    fn naming_a_grouped_member_queues_and_reports_the_kept_speaker() {
        use broscribe_core::schema::{
            Centroid, GroupOrigin, Meeting, ProcessingStatus, Segment, Source, Speaker,
            SpeakerGroup, VoiceprintKind,
        };
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let mut m = Meeting::new(
            lib.new_meeting_id().unwrap(),
            Source::Import,
            broscribe_core::schema::now(),
        );
        m.processing.status = ProcessingStatus::Done;
        let config = Config::default();
        let model = config.profile.speaker_embedding.clone();
        for (n, talk) in [(1u32, 30.0), (2, 10.0)] {
            let id = SpeakerId::new(n).unwrap();
            let mut s = Speaker::new(id, "t0");
            s.talk_s = talk;
            s.centroid = Some(Centroid::new(&model, &[1.0, 0.0], 2));
            m.speakers.push(s);
            m.segments.push(Segment {
                speech_overlap: Vec::new(),
                id: n,
                track: "t0".into(),
                speaker: Some(id),
                start: f64::from(n) * 5.0,
                end: f64::from(n) * 5.0 + 3.0,
                text: "synthetic words".into(),
                words: Vec::new(),
            });
        }
        let (kept, member) = (SpeakerId::new(1).unwrap(), SpeakerId::new(2).unwrap());
        m.speaker_groups = vec![SpeakerGroup::new(kept, [member], GroupOrigin::User)];
        lib.write(|tx| {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)?;
            broscribe_core::index::Index::rebuild(tx).map(|_| ())
        })
        .unwrap();

        let out = speaker_name(
            &lib,
            &config,
            m.id.as_str(),
            &member.to_string(),
            "Ana Example",
        )
        .unwrap();
        assert_eq!(out.value["speaker_id"], json!(kept.to_string()));
        assert_eq!(out.value["redirected_from"], json!(member.to_string()));
        let person = out.value["person_id"].as_str().unwrap().to_owned();
        let jobs = Queue::open(lib.root()).unwrap().list(CONFIRM_KIND).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].key, format!("{}/{kept}/{person}/{model}", m.id));
        assert_eq!(jobs[0].input["speaker_id"], json!(kept.to_string()));
        assert_eq!(out.value["confirm_job_id"], json!(jobs[0].id));
        let refreshes = || Queue::open(lib.root()).unwrap().list(VECTORS_KIND).unwrap();
        assert!(refreshes().is_empty(), "no separation, no refresh");
        assert!(out.value.get("refresh_vectors_job_id").is_none());
        // Core requested the speaker recheck, keyed by model, with this
        // meeting first and no names in the stored job.
        let recheck = Queue::open(lib.root())
            .unwrap()
            .get(&broscribe_core::learning::job_id(&model))
            .unwrap();
        assert_eq!(recheck.kind, broscribe_core::learning::REIDENTIFY_KIND);
        assert_eq!(out.value["learning"]["job_id"], json!(recheck.id));
        assert_eq!(out.value["learning"]["source_meeting"], json!(m.id));
        assert_eq!(recheck.input["source_meetings"], json!([m.id]));
        let stored = recheck.input.to_string();
        assert!(!stored.contains("Ana Example"), "{stored}");
        assert!(out.value.get("retro_candidates").is_none());
        assert!(out.touched == vec![m.id.clone()]);

        let saved = lib.load_meeting(&m.id).unwrap();
        let named = saved.speaker(kept).unwrap();
        assert_eq!(
            named.person_id.as_ref().map(|p| p.as_str()),
            Some(person.as_str())
        );
        assert!(saved.speaker(member).unwrap().person_id.is_none());
        let prints = lib
            .load_voiceprints(&broscribe_core::PersonId::parse(&person).unwrap())
            .unwrap();
        let from_meeting: Vec<_> = prints
            .embeddings
            .iter()
            .filter(|v| v.source.meeting_id.as_ref() == Some(&m.id))
            .collect();
        assert_eq!(from_meeting.len(), 1);
        assert_eq!(from_meeting[0].source.kind, VoiceprintKind::Confirmation);
        assert_eq!(from_meeting[0].source.speaker_id, Some(kept));

        // A rejection through the member also reports the kept speaker.
        let bob = broscribe_core::schema::Person::new(lib.new_person_id(), "Bob Example");
        lib.write(|tx| tx.save_person(&bob)).unwrap();
        let out = speaker_reject(
            &lib,
            &config,
            m.id.as_str(),
            &member.to_string(),
            Some(bob.id.as_str()),
        )
        .unwrap();
        assert_eq!(out.value["speaker_id"], json!(kept.to_string()));
        assert_eq!(out.value["redirected_from"], json!(member.to_string()));
        let saved = lib.load_meeting(&m.id).unwrap();
        assert_eq!(
            saved.speaker(kept).unwrap().rejected_person_ids,
            vec![bob.id]
        );
        assert!(
            saved
                .speaker(member)
                .unwrap()
                .rejected_person_ids
                .is_empty()
        );
        assert!(refreshes().is_empty(), "no separation, no refresh");

        // Naming the group as someone the member rejected separates it: its
        // lines show as another speaker, so the meeting's vectors refresh.
        let cai = broscribe_core::schema::Person::new(lib.new_person_id(), "Cai Example");
        lib.write(|tx| {
            tx.save_person(&cai)?;
            tx.update_meeting(&m.id, |m| {
                m.speaker_mut(member).unwrap().rejected_person_ids = vec![cai.id.clone()];
                Ok(())
            })
            .map(|_| ())
        })
        .unwrap();
        let out = speaker_name(
            &lib,
            &config,
            m.id.as_str(),
            &member.to_string(),
            cai.id.as_str(),
        )
        .unwrap();
        assert_eq!(
            out.value["separated"][0]["speaker_id"],
            json!(member.to_string())
        );
        let jobs = refreshes();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].key, m.id.as_str());
        assert_eq!(jobs[0].input, json!({ "meeting_id": m.id }));
        assert_eq!(out.value["refresh_vectors_job_id"], json!(jobs[0].id));
    }
}
