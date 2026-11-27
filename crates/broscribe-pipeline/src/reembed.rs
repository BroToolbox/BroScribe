//! Re-embedding after a speaker-embedding model switch (plan §13.9, §16.6).
//!
//! - Never runs diarization again: clean turns come from the stored segments
//!   (each speaker's runs of consecutive segments ≥ 1.5 s, the 20 longest);
//!   speaker IDs stay.
//! - Only speakers whose `centroid` is `Some` and from another model are
//!   recomputed; a `None` centroid (a forgotten voice) is never filled.
//! - Voiceprints are recomputed from their exact stored clean-turn ranges
//!   (legacy samples fall back to their envelope), including SHA-addressed
//!   enrollment WAVs. Old-model data stays; matching remains per model.
//!
//! Inference runs outside the lock; each meeting is committed on its own.

use std::collections::{BTreeMap, BTreeSet};

use broscribe_core::index::Index;
use broscribe_core::schema::{self, Voiceprint, VoiceprintOrigin};
use broscribe_core::{Library, MeetingId, PersonId, Result, SpeakerId, ops, paths};
use broscribe_models::SpeakerEmbedder;
use broscribe_platform::DecoderChain;
use serde::Serialize;

use crate::analyze::{centroid_of, stored_clean_turns};
use crate::audio::{decode_16k, decode_meeting_track, meeting_tracks};

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ReembedReport {
    /// The new embedding model.
    pub model: String,
    /// Meetings whose files changed.
    pub meetings_updated: usize,
    pub speakers_recomputed: usize,
    /// Speakers left with their old-model centroid (no audio, or no clean
    /// turn the model could embed).
    pub speakers_kept: usize,
    pub voiceprints_added: usize,
    /// Old-model voiceprints that could not be recomputed (no audio or no
    /// source meeting).
    pub voiceprints_kept: usize,
    /// Meetings that needed work but whose recording is not available.
    pub meetings_without_audio: usize,
}

/// Recomputes centroids and voiceprints with `embedder`'s model.
pub fn reembed(
    lib: &Library,
    decoders: &DecoderChain,
    embedder: &dyn SpeakerEmbedder,
) -> Result<ReembedReport> {
    let model = embedder.model_id().to_owned();
    let mut report = ReembedReport {
        model: model.clone(),
        ..ReembedReport::default()
    };

    // Old-model prints of people with a voiceprints file, by source meeting.
    let mut prints: BTreeMap<MeetingId, Vec<(PersonId, Voiceprint)>> = BTreeMap::new();
    let mut enrollments = Vec::new();
    for person in lib.list_people()? {
        if !lib
            .person_dir(&person.id)
            .join(paths::VOICEPRINTS_JSON)
            .is_file()
        {
            continue;
        }
        let vps = lib.load_voiceprints(&person.id)?;
        let have: BTreeSet<&str> = vps
            .embeddings
            .iter()
            .filter(|e| e.model == model)
            .map(|e| e.id.as_str())
            .collect();
        for vp in vps.embeddings.iter().filter(|e| e.model != model) {
            let Some(origin) = VoiceprintOrigin::of_source(&vp.source) else {
                report.voiceprints_kept += 1;
                continue;
            };
            if have.contains(new_id(&person.id, &model, vp, origin).as_str()) {
                continue;
            }
            match &vp.source.meeting_id {
                Some(m) => prints
                    .entry(m.clone())
                    .or_default()
                    .push((person.id.clone(), vp.clone())),
                None if vp.source.kind == schema::VoiceprintKind::Enrollment => {
                    enrollments.push((person.id.clone(), vp.clone()));
                }
                None => report.voiceprints_kept += 1,
            }
        }
    }

    for dir in lib.list_meeting_dirs()? {
        let meeting = lib.load_meeting_at(&dir)?;
        let speakers: Vec<SpeakerId> = meeting
            .speakers
            .iter()
            .filter(|s| s.centroid.as_ref().is_some_and(|c| c.model != model))
            .map(|s| s.id)
            .collect();
        let todo = prints.remove(&meeting.id).unwrap_or_default();
        if speakers.is_empty() && todo.is_empty() {
            continue;
        }
        let mut audio = BTreeMap::new();
        for source in meeting_tracks(&meeting) {
            let path = if source.path.is_absolute() {
                source.path.clone()
            } else {
                dir.join(&source.path)
            };
            if path.is_file() {
                let samples = decode_meeting_track(lib, decoders, &meeting, &source)?;
                audio.insert(source.track.clone(), (source, samples));
            }
        }
        if audio.is_empty() {
            report.meetings_without_audio += 1;
            report.speakers_kept += speakers.len();
            report.voiceprints_kept += todo.len();
            continue;
        }
        let mut centroids = Vec::new();
        for id in speakers {
            let track = meeting.speaker(id).map(|s| s.track.as_str()).unwrap_or("");
            let Some((source, samples)) = audio.get(track) else {
                report.speakers_kept += 1;
                continue;
            };
            let turns: Vec<_> = stored_clean_turns(&meeting, id)
                .into_iter()
                .map(|(a, b)| ((a - source.offset_s).max(0.0), b - source.offset_s))
                .filter(|(a, b)| b > a)
                .collect();
            match centroid_of(samples, &turns, embedder)? {
                Some(mut c) => {
                    // Provenance is meeting time, as `multitrack::append` keeps it.
                    for range in &mut c.ranges {
                        *range = range.map(|t| (f64::from(t) + source.offset_s) as f32);
                    }
                    centroids.push((id, c));
                }
                None => report.speakers_kept += 1,
            }
        }
        // Each new print with the old print it was computed from.
        let mut new_prints: BTreeMap<PersonId, Vec<(Voiceprint, Voiceprint)>> = BTreeMap::new();
        let user = ops::UserSpeakerEvidence::of(&meeting);
        for (person, vp) in todo {
            if !user.allows_voiceprint(&meeting, &vp, &person) {
                report.voiceprints_kept += 1;
                continue;
            }
            let old_vector = vp.vector()?;
            let track = meeting
                .speakers
                .iter()
                .filter(|speaker| {
                    meeting.segments.iter().any(|s| {
                        s.speaker == Some(speaker.id)
                            && s.end > vp.source.start
                            && s.start < vp.source.end
                    })
                })
                .filter_map(|speaker| {
                    speaker
                        .centroid
                        .as_ref()
                        .filter(|c| c.model == vp.model)
                        .and_then(|c| c.vector().ok())
                        .map(|v| (speaker, broscribe_core::identity::cosine(&old_vector, &v)))
                })
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(s, _)| s.track.as_str());
            let selected = track
                .and_then(|track| audio.get(track))
                .or_else(|| (audio.len() == 1).then(|| audio.values().next()).flatten());
            let Some((source, samples)) = selected else {
                report.voiceprints_kept += 1;
                continue;
            };
            let turns: Vec<_> = vp
                .source
                .audio_ranges()
                .map(|[a, b]| {
                    (
                        f64::from(a) - source.offset_s,
                        f64::from(b) - source.offset_s,
                    )
                })
                .collect();
            match centroid_of(samples, &turns, embedder)?
                .map(|c| c.vector())
                .transpose()?
            {
                Some(v) => {
                    // The audio is known now: key by the recording.
                    let mut source = vp.source.clone();
                    if source.sha256.is_none() {
                        source.sha256 = meeting.recording.as_ref().map(|r| r.sha256.clone());
                    }
                    let origin = VoiceprintOrigin::new(source.sha256.as_deref(), &meeting.id);
                    let id = new_id(&person, &model, &vp, origin);
                    let fresh = Voiceprint {
                        id,
                        model: model.clone(),
                        dim: v.len(),
                        vector_b64: schema::encode_vector(&v),
                        source,
                        device: vp.device.clone(),
                        strength: vp.strength,
                        created_at: schema::now(),
                    };
                    new_prints
                        .entry(person.clone())
                        .or_default()
                        .push((vp, fresh));
                }
                None => report.voiceprints_kept += 1,
            }
        }

        let (changed, recomputed, added) = ops::with_index(lib, |tx, index: &mut Index| {
            let mut recomputed = 0;
            let snapshot = &meeting;
            let mut meeting = tx.library().load_meeting(&snapshot.id)?;
            // Speaker IDs, turns and audio were read from `snapshot`: after a
            // reanalysis meanwhile (another speaker namespace or recording)
            // they belong to another analysis and nothing computed here
            // applies. The next run starts from the new analysis.
            let same_analysis = meeting.speaker_namespace == snapshot.speaker_namespace
                && meeting.recording.as_ref().map(|r| &r.sha256)
                    == snapshot.recording.as_ref().map(|r| &r.sha256);
            for (id, c) in centroids.iter().filter(|_| same_analysis) {
                // Re-read under the lock: a voice forgotten (or recomputed, or
                // whose turns changed) meanwhile keeps what it has now.
                let unchanged = meeting.speaker(*id).map(|s| &s.centroid)
                    == snapshot.speaker(*id).map(|s| &s.centroid)
                    && stored_clean_turns(&meeting, *id) == stored_clean_turns(snapshot, *id);
                if let Some(s) = meeting.speaker_mut(*id)
                    && unchanged
                    && s.centroid.as_ref().is_some_and(|old| old.model != model)
                {
                    s.centroid = Some(c.clone());
                    recomputed += 1;
                }
            }
            if recomputed > 0 {
                meeting.updated_at = schema::now();
                tx.save_meeting(&meeting)?;
                index.upsert_meeting(tx, &meeting)?;
            }
            let mut added = 0;
            let user = ops::UserSpeakerEvidence::of(&meeting);
            for (person, list) in new_prints.iter().filter(|_| same_analysis) {
                let lib = tx.library().clone();
                if !lib
                    .person_dir(person)
                    .join(paths::VOICEPRINTS_JSON)
                    .is_file()
                {
                    continue;
                }
                let mut vps = lib.load_voiceprints(person)?;
                let known: BTreeSet<String> = vps.embeddings.iter().map(|e| e.id.clone()).collect();
                // A print corrected away (or changed) during inference must
                // not come back under the new model.
                let fresh: Vec<Voiceprint> = list
                    .iter()
                    .filter(|(old, new)| {
                        !known.contains(&new.id)
                            && vps.embeddings.iter().any(|e| e == old)
                            && user.allows_voiceprint(&meeting, old, person)
                    })
                    .map(|(_, new)| new.clone())
                    .collect();
                if !fresh.is_empty() {
                    added += fresh.len();
                    vps.upsert(fresh);
                    crate::batch::cap_confirmed(&mut vps, &model)?;
                    tx.save_voiceprints(&vps)?;
                }
            }
            Ok((recomputed > 0 || added > 0, recomputed, added))
        })?;
        report.meetings_updated += usize::from(changed);
        report.speakers_recomputed += recomputed;
        report.speakers_kept += centroids.len() - recomputed;
        report.voiceprints_added += added;
    }
    // Prints whose source meeting no longer exists.
    report.voiceprints_kept += prints.values().map(Vec::len).sum::<usize>();
    for (person, vp) in enrollments {
        let Some(sha) = vp
            .source
            .sha256
            .as_deref()
            .filter(|sha| sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        else {
            report.voiceprints_kept += 1;
            continue;
        };
        let path = lib
            .person_dir(&person)
            .join("enrollment")
            .join(format!("{sha}.wav"));
        if !path.is_file() {
            report.voiceprints_kept += 1;
            continue;
        }
        let samples = decode_16k(decoders, &path)?.samples;
        let turns: Vec<_> = vp
            .source
            .audio_ranges()
            .map(|[a, b]| (f64::from(a), f64::from(b)))
            .collect();
        let Some(centroid) = centroid_of(&samples, &turns, embedder)? else {
            report.voiceprints_kept += 1;
            continue;
        };
        let vector = centroid.vector()?;
        let fresh = Voiceprint {
            id: new_id(
                &person,
                &model,
                &vp,
                VoiceprintOrigin::Recording { sha256: sha },
            ),
            model: model.clone(),
            dim: vector.len(),
            vector_b64: schema::encode_vector(&vector),
            source: vp.source.clone(),
            device: vp.device.clone(),
            strength: vp.strength,
            created_at: schema::now(),
        };
        report.voiceprints_added += lib.write(|tx| {
            let mut prints = tx.library().load_voiceprints(&person)?;
            // Redo/forget during inference must not resurrect the old passage.
            if !prints.embeddings.iter().any(|v| v == &vp)
                || prints.embeddings.iter().any(|v| v.id == fresh.id)
            {
                return Ok(0);
            }
            prints.upsert(vec![fresh]);
            crate::batch::cap_confirmed(&mut prints, &model)?;
            tx.save_voiceprints(&prints)?;
            Ok(1)
        })?;
    }
    Ok(report)
}

/// The deterministic ID of `vp`'s new-model counterpart from `origin`.
fn new_id(person: &PersonId, model: &str, vp: &Voiceprint, origin: VoiceprintOrigin<'_>) -> String {
    schema::voiceprint_id(
        person,
        model,
        origin,
        vp.source.start,
        vp.source.end,
        vp.source.kind,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Env;
    use crate::import::{ImportOptions, import_file};
    use crate::testutil::{conversation, models, write_wav};
    use broscribe_core::config::Config;
    use broscribe_core::schema::Person;
    use broscribe_core::schema::{
        Strength, VoiceprintKind, VoiceprintSource, Voiceprints, encode_vector,
    };
    use broscribe_models::fakes::ToneEmbedder;
    use broscribe_platform::fakes::FakeProcessProbe;
    use broscribe_platform::symphonia_decoder::SymphoniaDecoder;

    #[test]
    fn reembed_recomputes_centroids_and_prints_but_never_fills_forgotten_voices() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path().join("lib")).unwrap();
        let chain = DecoderChain::new(vec![Box::new(SymphoniaDecoder::new())]);
        let probe = FakeProcessProbe::new(std::process::id());
        let config = Config::default();
        let execution = crate::testutil::execution();
        let env = Env {
            lib: &lib,
            config: &config,
            decoders: &chain,
            probe: &probe,
            execution: &execution,
            progress: None,
        };
        let file = tmp.path().join("talk.wav");
        write_wav(
            &file,
            16_000,
            &conversation(&[(Some(0), 4.0), (Some(1), 4.0)]),
        );
        let out = import_file(&env, &mut models(), &file, &ImportOptions::default()).unwrap();
        let m = lib.load_meeting(&out.meeting_id).unwrap();
        let (s1, s2) = (m.speakers[0].id, m.speakers[1].id);
        let sha = m.recording.as_ref().unwrap().sha256.clone();

        // A person with one old-model hint print from this meeting, and a
        // forgotten voice (centroid None) on S2.
        let ana = Person::new(lib.new_person_id(), "Ana Example");
        let old = Voiceprint {
            id: schema::voiceprint_id(
                &ana.id,
                "nemo-titanet-large",
                VoiceprintOrigin::Recording { sha256: &sha },
                0.5,
                3.5,
                VoiceprintKind::Hint,
            ),
            model: "nemo-titanet-large".into(),
            dim: 2,
            vector_b64: encode_vector(&[1.0, 0.0]),
            source: VoiceprintSource {
                kind: VoiceprintKind::Hint,
                meeting_id: Some(m.id.clone()),
                sha256: Some(sha.clone()),
                start: 0.5,
                end: 3.5,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: schema::now(),
        };
        lib.write(|tx| {
            tx.save_person(&ana)?;
            let mut vps = Voiceprints::empty(ana.id.clone());
            vps.upsert(vec![old.clone()]);
            tx.save_voiceprints(&vps)?;
            tx.update_meeting(&m.id, |m| {
                m.speaker_mut(s2).unwrap().centroid = None;
                Ok(())
            })
            .map(|_| ())
        })
        .unwrap();

        let mut small = ToneEmbedder::new("nemo-titanet-small", 8);
        small.max_samples = Some(2 * 16_000);
        let r = reembed(&lib, &chain, &small).unwrap();
        assert_eq!(r.speakers_recomputed, 1);
        assert_eq!(r.voiceprints_added, 1);
        let after = lib.load_meeting(&m.id).unwrap();
        assert_eq!(
            after.speaker(s1).unwrap().centroid.as_ref().unwrap().model,
            "nemo-titanet-small"
        );
        assert!(after.speaker(s2).unwrap().centroid.is_none());
        let vps = lib.load_voiceprints(&ana.id).unwrap();
        assert_eq!(vps.embeddings.len(), 2);
        let new = vps
            .embeddings
            .iter()
            .find(|e| e.model == "nemo-titanet-small")
            .unwrap();
        assert_eq!((new.source.start, new.source.end), (0.5, 3.5));
        assert_eq!(new.dim, 8);

        // Running again changes nothing.
        let r2 = reembed(&lib, &chain, &small).unwrap();
        assert_eq!(r2.speakers_recomputed + r2.voiceprints_added, 0);
    }

    /// Runs `hook` once, on the first embedding, then defers to `inner`.
    struct Hooked<'a> {
        inner: ToneEmbedder,
        hook: std::cell::Cell<Option<Box<dyn FnOnce() + 'a>>>,
    }

    impl SpeakerEmbedder for Hooked<'_> {
        fn model_id(&self) -> &str {
            self.inner.model_id()
        }
        fn dim(&self) -> usize {
            self.inner.dim()
        }
        fn embed(&self, samples: &[f32]) -> Result<Option<Vec<f32>>> {
            if let Some(hook) = self.hook.take() {
                hook();
            }
            self.inner.embed(samples)
        }
    }

    /// A print corrected away while the new model runs must not come back
    /// as its new-model counterpart.
    #[test]
    fn reembed_skips_a_print_removed_during_inference() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path().join("lib")).unwrap();
        let chain = DecoderChain::new(vec![Box::new(SymphoniaDecoder::new())]);
        let probe = FakeProcessProbe::new(std::process::id());
        let config = Config::default();
        let execution = crate::testutil::execution();
        let env = Env {
            lib: &lib,
            config: &config,
            decoders: &chain,
            probe: &probe,
            execution: &execution,
            progress: None,
        };
        let file = tmp.path().join("talk.wav");
        write_wav(
            &file,
            16_000,
            &conversation(&[(Some(0), 4.0), (Some(1), 4.0)]),
        );
        let out = import_file(&env, &mut models(), &file, &ImportOptions::default()).unwrap();
        let m = lib.load_meeting(&out.meeting_id).unwrap();
        let sha = m.recording.as_ref().unwrap().sha256.clone();
        let ana = Person::new(lib.new_person_id(), "Ana Example");
        let old = Voiceprint {
            id: schema::voiceprint_id(
                &ana.id,
                "nemo-titanet-large",
                VoiceprintOrigin::Recording { sha256: &sha },
                0.5,
                3.5,
                VoiceprintKind::Hint,
            ),
            model: "nemo-titanet-large".into(),
            dim: 2,
            vector_b64: encode_vector(&[1.0, 0.0]),
            source: VoiceprintSource {
                kind: VoiceprintKind::Hint,
                meeting_id: Some(m.id.clone()),
                sha256: Some(sha.clone()),
                start: 0.5,
                end: 3.5,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: schema::now(),
        };
        lib.write(|tx| {
            tx.save_person(&ana)?;
            let mut vps = Voiceprints::empty(ana.id.clone());
            vps.upsert(vec![old.clone()]);
            tx.save_voiceprints(&vps)
        })
        .unwrap();

        let embedder = Hooked {
            inner: ToneEmbedder::new("nemo-titanet-small", 8),
            hook: std::cell::Cell::new(Some(Box::new(|| {
                // The user corrects the print away after the snapshot.
                lib.write(|tx| {
                    let mut vps = tx.library().load_voiceprints(&ana.id)?;
                    vps.embeddings.retain(|e| e.id != old.id);
                    tx.save_voiceprints(&vps)
                })
                .unwrap();
            }))),
        };
        let r = reembed(&lib, &chain, &embedder).unwrap();
        assert_eq!(r.voiceprints_added, 0);
        assert!(lib.load_voiceprints(&ana.id).unwrap().embeddings.is_empty());
    }

    /// A reanalysis that renumbers the speakers while the new model runs
    /// makes this pass' speaker IDs and turns another analysis': no centroid
    /// or print computed from them lands on the new one.
    #[test]
    fn reembed_never_applies_to_a_meeting_reanalysed_during_inference() {
        let fx = crate::testutil::Fixture::new();
        let file = fx.wav("renumbered.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
        let out = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
        let m = fx.lib.load_meeting(&out.meeting_id).unwrap();
        let sha = m.recording.as_ref().unwrap().sha256.clone();
        let ana = Person::new(fx.lib.new_person_id(), "Ana Example");
        let old = Voiceprint {
            id: schema::voiceprint_id(
                &ana.id,
                "nemo-titanet-large",
                VoiceprintOrigin::Recording { sha256: &sha },
                0.5,
                3.5,
                VoiceprintKind::Hint,
            ),
            model: "nemo-titanet-large".into(),
            dim: 2,
            vector_b64: encode_vector(&[1.0, 0.0]),
            source: VoiceprintSource {
                kind: VoiceprintKind::Hint,
                meeting_id: Some(m.id.clone()),
                sha256: Some(sha.clone()),
                start: 0.5,
                end: 3.5,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: schema::now(),
        };
        fx.lib
            .write(|tx| {
                tx.save_person(&ana)?;
                let mut vps = Voiceprints::empty(ana.id.clone());
                vps.upsert(vec![old.clone()]);
                tx.save_voiceprints(&vps)
            })
            .unwrap();
        let embedder = Hooked {
            inner: ToneEmbedder::new("nemo-titanet-small", 8),
            hook: std::cell::Cell::new(Some(Box::new(|| {
                // A reanalysis publishes another speaker numbering.
                fx.lib
                    .write(|tx| {
                        tx.update_meeting(&m.id, |m| {
                            m.speaker_namespace = "after-reanalysis".into();
                            Ok(())
                        })
                        .map(|_| ())
                    })
                    .unwrap();
            }))),
        };
        let r = reembed(&fx.lib, &fx.chain, &embedder).unwrap();
        assert_eq!((r.speakers_recomputed, r.voiceprints_added), (0, 0));
        let after = fx.lib.load_meeting(&m.id).unwrap();
        assert!(after.speakers.iter().all(|s| {
            s.centroid
                .as_ref()
                .is_none_or(|c| c.model == "nemo-titanet-large")
        }));
        assert_eq!(
            fx.lib.load_voiceprints(&ana.id).unwrap().embeddings,
            vec![old]
        );
    }

    #[test]
    fn enrollment_prints_reembed_from_sha_addressed_wav_ranges() {
        let fx = crate::testutil::Fixture::new();
        let person = Person::new(fx.lib.new_person_id(), "Ari Example");
        let sha = "a".repeat(64);
        let dir = fx.lib.person_dir(&person.id).join("enrollment");
        std::fs::create_dir_all(&dir).unwrap();
        write_wav(
            &dir.join(format!("{sha}.wav")),
            16_000,
            &conversation(&[(Some(0), 3.0), (Some(1), 3.0)]),
        );
        let vp = Voiceprint {
            id: "passage".into(),
            model: "old".into(),
            dim: 2,
            vector_b64: encode_vector(&[1.0, 0.0]),
            source: VoiceprintSource {
                kind: VoiceprintKind::Enrollment,
                meeting_id: None,
                sha256: Some(sha),
                start: 0.0,
                end: 3.0,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: schema::now(),
        };
        fx.lib
            .write(|tx| {
                tx.save_person(&person)?;
                let mut vps = Voiceprints::empty(person.id.clone());
                vps.upsert(vec![vp]);
                tx.save_voiceprints(&vps)
            })
            .unwrap();
        let small = ToneEmbedder::new("new", 8);
        assert_eq!(
            reembed(&fx.lib, &fx.chain, &small)
                .unwrap()
                .voiceprints_added,
            1
        );
        let vps = fx.lib.load_voiceprints(&person.id).unwrap();
        let new = vps.embeddings.iter().find(|v| v.model == "new").unwrap();
        let expected = small
            .embed(&conversation(&[(Some(0), 3.0)]))
            .unwrap()
            .unwrap();
        assert!(broscribe_core::identity::cosine(&new.vector().unwrap(), &expected) > 0.999);
        assert_eq!(
            reembed(&fx.lib, &fx.chain, &small)
                .unwrap()
                .voiceprints_added,
            0
        );
    }

    #[test]
    fn weak_prints_reembed_only_stored_clean_turn_ranges() {
        let fx = crate::testutil::Fixture::new();
        let file = fx.wav(
            "gapped.wav",
            &[(Some(0), 2.0), (Some(1), 8.0), (Some(0), 2.0)],
        );
        let out = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
        let person = Person::new(fx.lib.new_person_id(), "Bea Example");
        let vp = Voiceprint {
            id: "weak".into(),
            model: "nemo-titanet-large".into(),
            dim: 2,
            vector_b64: encode_vector(&[1.0, 0.0]),
            source: VoiceprintSource {
                kind: VoiceprintKind::Auto,
                meeting_id: Some(out.meeting_id),
                sha256: None,
                start: 0.0,
                end: 12.0,
                ranges: vec![[0.0, 2.0], [10.0, 12.0]],
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: Strength::Weak,
            created_at: schema::now(),
        };
        fx.lib
            .write(|tx| {
                tx.save_person(&person)?;
                let mut vps = Voiceprints::empty(person.id.clone());
                vps.upsert(vec![vp]);
                tx.save_voiceprints(&vps)
            })
            .unwrap();
        let small = ToneEmbedder::new("new", 8);
        assert_eq!(
            reembed(&fx.lib, &fx.chain, &small)
                .unwrap()
                .voiceprints_added,
            1
        );
        let vps = fx.lib.load_voiceprints(&person.id).unwrap();
        let new = vps.embeddings.iter().find(|v| v.model == "new").unwrap();
        let expected = small
            .embed(&conversation(&[(Some(0), 2.0)]))
            .unwrap()
            .unwrap();
        assert!(broscribe_core::identity::cosine(&new.vector().unwrap(), &expected) > 0.999);
        assert_eq!(new.source.ranges, [[0.0, 2.0], [10.0, 12.0]]);
    }

    #[test]
    fn reembedded_centroid_ranges_stay_in_meeting_time_for_offset_sources() {
        let fx = crate::testutil::Fixture::new();
        let file = fx.wav("late.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
        let out = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
        // The same audio as a referenced source starting 5 s into the meeting.
        let before = fx
            .lib
            .write(|tx| {
                tx.update_meeting(&out.meeting_id, |m| {
                    let rec = m.recording.as_mut().unwrap();
                    rec.sources = vec![schema::RecordingSource {
                        path: rec.path.clone(),
                        track_index: 0,
                        offset_s: 5.0,
                        sha256: rec.sha256.clone(),
                    }];
                    for s in &mut m.segments {
                        s.start += 5.0;
                        s.end += 5.0;
                    }
                    for s in &mut m.speakers {
                        let c = s.centroid.as_mut().unwrap();
                        for r in &mut c.ranges {
                            *r = r.map(|t| t + 5.0);
                        }
                    }
                    Ok(())
                })
            })
            .unwrap();
        let small = ToneEmbedder::new("new", 8);
        assert_eq!(
            reembed(&fx.lib, &fx.chain, &small)
                .unwrap()
                .speakers_recomputed,
            2
        );
        let after = fx.lib.load_meeting(&out.meeting_id).unwrap();
        assert_eq!(before.speakers.len(), after.speakers.len());
        for s in &after.speakers {
            let c = s.centroid.as_ref().unwrap();
            assert_eq!(c.model, "new");
            assert!(!c.ranges.is_empty());
            // Every range lies in this speaker's speech on the meeting timeline.
            let own = after
                .segments
                .iter()
                .filter(|seg| seg.speaker == Some(s.id));
            let (from, to) = own.fold((f64::MAX, f64::MIN), |(a, b), seg| {
                (a.min(seg.start), b.max(seg.end))
            });
            assert!(
                c.ranges
                    .iter()
                    .all(|r| f64::from(r[0]) >= from - 1e-3 && f64::from(r[1]) <= to + 1e-3),
                "{:?} outside {from}..{to}",
                c.ranges
            );
        }
    }
}
