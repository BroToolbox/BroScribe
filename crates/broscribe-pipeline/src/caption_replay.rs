//! Re-applies a recording's saved meeting-app evidence (`roster.json` and
//! `captions.jsonl`, copied into the meeting folder when the recording was
//! published) after its transcript or speakers change, e.g. on reprocessing.
//!
//! Captions are re-aligned against the current ASR spans and named exactly as
//! the record path does. The names never come from this meeting's own caption
//! voiceprints: the resolver excludes same-meeting weak samples, so without
//! this replay a person learned only from this meeting's captions would lose
//! the speaker. A caption name keeps the person this meeting's captions
//! already linked (even renamed), and identities forgotten since the meeting
//! (`people forget`, `forget-voice`) are never named or re-enrolled again.

use std::borrow::Cow;
use std::collections::BTreeMap;

use broscribe_core::index::Index;
use broscribe_core::schema::{Meeting, Person, SpeakerGroup};
use broscribe_core::{Library, MeetingId, Result, SpeakerId, WriteTx, ops};
use broscribe_models::SpeakerEmbedder;
use broscribe_readers::RosterMember;
use broscribe_readers::alignment::{self, CaptionNaming, RangeEmbeddings};
use broscribe_readers::captions::folded_name;

use crate::Env;
use crate::record::CaptionWrites;

/// Runs after identity resolution (`finish_meeting`) inside the same write
/// transaction, because the resolver replaces unconfirmed names. Confirmed
/// speakers, grouped members (frozen) and remembered rejections are left
/// alone. Caption samples come from `embeddings`, computed outside the lock
/// by [`embed_saved_captions`] (empty: names only, no new samples).
/// `provisional` are speaker groups this same publish formed but has not
/// stored yet: captions align with them folded (one voice, one naming) and
/// their members are not frozen. Returns the number of speakers whose
/// assignment the captions changed.
pub(crate) fn realign(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    embeddings: &RangeEmbeddings,
    provisional: &[SpeakerGroup],
) -> Result<usize> {
    let mut meeting = tx.library().load_meeting(meeting_id)?;
    let participants = meeting.participants.clone();
    let before_notes = meeting.review_notes.clone();
    let before = speaker_states(&meeting);
    if !apply_saved_captions(tx, index, &mut meeting, embeddings, provisional)? {
        return Ok(0);
    }
    let changes = speaker_changes(&meeting, before);
    ops::refresh_identity_conflict_notes(tx.library(), &mut meeting)?;
    // A caption link to an already correctly named speaker changes no
    // assignment but must persist, so a later rename or forget is honoured.
    if changes.is_empty()
        && meeting.participants == participants
        && meeting.review_notes == before_notes
    {
        return Ok(0);
    }
    ops::sync_weak_samples(tx, &meeting, &changes)?;
    meeting.updated_at = broscribe_core::schema::now();
    tx.save_meeting(&meeting)?;
    index.upsert_meeting(tx, &meeting)?;
    Ok(changes.len())
}

/// Names `meeting`'s speakers (in memory; people and voiceprints are written)
/// from the meeting folder's saved captions, as the record path does:
/// reader evidence, minus forgotten identities, then caption names
/// ([`saved_namings`]). Returns whether any naming survived. The caller
/// saves the meeting and syncs weak samples in the same write.
pub(crate) fn apply_saved_captions(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting: &mut Meeting,
    embeddings: &RangeEmbeddings,
    provisional: &[SpeakerGroup],
) -> Result<bool> {
    let mut people = tx.library().list_people()?;
    let mut writes = CaptionWrites::default();
    let dir = tx.library().meeting_dir(&meeting.id)?;
    let named = plan_saved_captions(
        tx.library(),
        &dir,
        &mut people,
        meeting,
        embeddings,
        provisional,
        &mut writes,
    )?;
    crate::record::write_caption_names(tx, index, &people, &writes)?;
    Ok(named)
}

/// [`apply_saved_captions`] without writing: names `meeting`'s speakers in
/// memory and adds the people and samples to store to `writes`
/// ([`crate::record::write_caption_names`]). `people` is the library's
/// people, extended with those this plan creates.
pub(crate) fn plan_saved_captions(
    lib: &Library,
    dir: &std::path::Path,
    people: &mut Vec<Person>,
    meeting: &mut Meeting,
    embeddings: &RangeEmbeddings,
    provisional: &[SpeakerGroup],
    writes: &mut CaptionWrites,
) -> Result<bool> {
    if !dir.join("captions.jsonl").exists() {
        return Ok(false);
    }
    let saved = saved_namings(dir, meeting, provisional)?;
    let raw = drop_forgotten(lib, people, meeting, &saved.roster, &saved.raw)?;
    let folded = drop_forgotten(lib, people, meeting, &saved.roster, &saved.folded)?;
    if raw.is_empty() && folded.is_empty() {
        return Ok(false);
    }
    let roster = &saved.roster;
    crate::record::plan_caption_names(
        lib, people, meeting, None, roster, &raw, embeddings, writes,
    )?;
    if !folded.is_empty() {
        crate::record::plan_caption_names(
            lib,
            people,
            meeting,
            saved.view.as_ref(),
            roster,
            &folded,
            embeddings,
            writes,
        )?;
    }
    Ok(true)
}

/// The namings of a meeting folder's saved captions.
pub(crate) struct SavedNamings {
    pub roster: Vec<RosterMember>,
    /// Aligned against the speakers as they are.
    pub raw: Vec<CaptionNaming>,
    /// The meeting with the `provisional` groups folded, when there are any.
    pub view: Option<Meeting>,
    /// Aligned against `view`: namings of a provisional group's kept speaker
    /// when the raw alignment named none of the group's speakers.
    pub folded: Vec<CaptionNaming>,
}

/// Captions align with the speakers as they are first, so a cluster the
/// captions prove to be someone else keeps that proof (and is never grouped
/// against it). Only a `provisional` group (formed in this same publish)
/// the raw alignment left entirely unnamed is aligned again with its
/// clusters folded: a voice over-split into clusters too short to agree
/// with a caption alone is then named once, with all its ranges.
pub(crate) fn saved_namings(
    dir: &std::path::Path,
    meeting: &Meeting,
    provisional: &[SpeakerGroup],
) -> Result<SavedNamings> {
    let setup = meeting_setup(meeting);
    let (roster, raw) = crate::record::reader_evidence(dir, meeting, setup)?;
    if provisional.is_empty() {
        return Ok(SavedNamings {
            roster,
            raw,
            view: None,
            folded: Vec::new(),
        });
    }
    let view = caption_view(meeting, provisional)?;
    let (_, names) = crate::record::reader_evidence(dir, &view, setup)?;
    let named: std::collections::BTreeSet<SpeakerId> = raw.iter().map(|n| n.speaker_id).collect();
    let folded = names
        .into_iter()
        .filter(|n| {
            provisional
                .iter()
                .any(|g| g.kept == n.speaker_id && !g.speakers().any(|s| named.contains(&s)))
        })
        .collect();
    Ok(SavedNamings {
        roster,
        raw,
        view: Some(view),
        folded,
    })
}

/// `meeting` with `groups` folded (one speaker per voice, with all its
/// clean ranges), as caption alignment and its embeddings see it.
pub(crate) fn caption_view(meeting: &Meeting, groups: &[SpeakerGroup]) -> Result<Meeting> {
    let model = meeting.models.speaker_embedding.clone().unwrap_or_default();
    crate::finish::fold_view(meeting, &model, groups)
}

/// Embeds, outside the library lock, the agreed clean ranges of every
/// speaker the saved captions name in `meeting` ([`saved_namings`] with the
/// `provisional` groups): a snapshot of the speakers and transcript about
/// to be published (the folder's saved captions and the recording's tracks
/// are read). Empty without saved captions.
pub(crate) fn embed_saved_captions(
    env: &Env<'_>,
    meeting: &Meeting,
    provisional: &[SpeakerGroup],
    embedder: &dyn SpeakerEmbedder,
) -> Result<RangeEmbeddings> {
    let dir = env.lib.meeting_dir(&meeting.id)?;
    if !dir.join("captions.jsonl").exists() {
        return Ok(RangeEmbeddings::new(embedder.model_id()));
    }
    let saved = saved_namings(&dir, meeting, provisional)?;
    let named = saved
        .raw
        .iter()
        .filter_map(|n| meeting.speaker(n.speaker_id).map(|s| (s, n)))
        .chain(saved.view.iter().flat_map(|view| {
            saved
                .folded
                .iter()
                .filter_map(|n| view.speaker(n.speaker_id).map(|s| (s, n)))
        }));
    let sources = crate::audio::meeting_tracks(meeting);
    embed_named_ranges(named, embedder, |track| {
        // As re-embedding: a single source stands for any track label.
        let source = sources
            .iter()
            .find(|s| s.track == track)
            .or_else(|| (sources.len() == 1).then(|| &sources[0]));
        let Some(source) = source else {
            return Ok(None);
        };
        let samples = crate::audio::decode_meeting_track(env.lib, env.decoders, meeting, source)?;
        Ok(Some((Cow::Owned(samples), source.offset_s)))
    })
}

/// Embeds each of the [`alignment::agreed_clean_ranges`] of the speakers
/// `names` name, one range at a time, from `track_audio(track)`: that track's
/// 16 kHz samples and its offset in meeting time (`None`: unavailable).
/// Ranges the embedder finds too short are left out.
pub(crate) fn embed_caption_ranges<'a>(
    meeting: &Meeting,
    names: &[CaptionNaming],
    embedder: &dyn SpeakerEmbedder,
    track_audio: impl FnMut(&str) -> Result<Option<(Cow<'a, [f32]>, f64)>>,
) -> Result<RangeEmbeddings> {
    embed_named_ranges(
        names
            .iter()
            .filter_map(|n| meeting.speaker(n.speaker_id).map(|s| (s, n))),
        embedder,
        track_audio,
    )
}

/// [`embed_caption_ranges`] of `(speaker, naming)` pairs.
fn embed_named_ranges<'a, 'm>(
    named: impl IntoIterator<Item = (&'m broscribe_core::schema::Speaker, &'m CaptionNaming)>,
    embedder: &dyn SpeakerEmbedder,
    mut track_audio: impl FnMut(&str) -> Result<Option<(Cow<'a, [f32]>, f64)>>,
) -> Result<RangeEmbeddings> {
    let mut wanted: BTreeMap<&str, Vec<[f32; 2]>> = BTreeMap::new();
    for (speaker, naming) in named {
        wanted
            .entry(speaker.track.as_str())
            .or_default()
            .extend(alignment::agreed_clean_ranges(speaker, naming));
    }
    let mut out = RangeEmbeddings::new(embedder.model_id());
    for (track, mut ranges) in wanted {
        ranges.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
        ranges.dedup();
        let Some((samples, offset)) = track_audio(track)? else {
            continue;
        };
        for range in ranges {
            let shifted = (f64::from(range[0]) - offset, f64::from(range[1]) - offset);
            let embedded = crate::analyze::embed_ranges(&samples, &[shifted], embedder)?;
            let vectors: Vec<&[f32]> = embedded
                .iter()
                .map(|(_, vector)| vector.as_slice())
                .collect();
            if let Some(vector) = broscribe_core::identity::mean_normalized(&vectors) {
                out.insert(track, range, vector);
            }
        }
    }
    Ok(out)
}

fn meeting_setup(meeting: &Meeting) -> broscribe_dsp::Setup {
    meeting
        .setup
        .as_deref()
        .and_then(|s| s.parse::<broscribe_dsp::Setup>().ok())
        .unwrap_or_else(|| broscribe_dsp::SetupRules::builtin().fallback)
}

/// Every speaker's assignment, before a change [`speaker_changes`] compares.
pub(crate) fn speaker_states(meeting: &Meeting) -> Vec<(SpeakerId, ops::SpeakerState)> {
    meeting
        .speakers
        .iter()
        .map(|s| (s.id, ops::SpeakerState::of(s)))
        .collect()
}

/// The speakers of `meeting` whose assignment or person differs from `before`.
pub(crate) fn speaker_changes(
    meeting: &Meeting,
    before: Vec<(SpeakerId, ops::SpeakerState)>,
) -> Vec<ops::SpeakerChange> {
    before
        .into_iter()
        .filter_map(|(speaker_id, before)| {
            let after = ops::SpeakerState::of(meeting.speaker(speaker_id)?);
            (before.assignment != after.assignment || before.person_id != after.person_id).then(
                || ops::SpeakerChange {
                    meeting_id: meeting.id.clone(),
                    speaker_id,
                    before,
                    after,
                },
            )
        })
        .collect()
}

/// The namings of `names` whose identity was not forgotten since `meeting`
/// started; recording and replay both filter through it. A naming is dropped
/// when the caption participant an earlier pass linked lost its person, or a
/// forget tombstone matches the linked person (ID, names, e-mails) or,
/// without a link, the caption name (normalised as caption matching does),
/// the roster's e-mail for it or the person it matches. `people` is the
/// library's people.
pub(crate) fn drop_forgotten(
    lib: &Library,
    people: &[Person],
    meeting: &Meeting,
    roster: &[RosterMember],
    names: &[CaptionNaming],
) -> Result<Vec<CaptionNaming>> {
    let mut kept = Vec::with_capacity(names.len());
    'naming: for naming in names {
        let mut keys: Vec<&str> = Vec::new();
        let mut name_keys: Vec<&str> = Vec::new();
        let person = match crate::record::caption_participant(meeting, naming) {
            Some(participant) => match &participant.person_id {
                Some(id) => match people.iter().find(|p| p.id == *id) {
                    Some(p) => Some(p),
                    None => {
                        tracing::warn!(
                            meeting = %meeting.id,
                            person = %id,
                            "caption participant links a person missing from the library; \
                             skipping its caption naming"
                        );
                        continue;
                    }
                },
                // Forgetting the person cleared the link.
                None => continue,
            },
            None => {
                let name = folded_name(&naming.name);
                name_keys.push(&naming.name);
                keys.extend(naming.is_self.then_some("self"));
                keys.extend(
                    roster
                        .iter()
                        .filter(|r| folded_name(&r.name) == name)
                        .filter_map(|r| r.email.as_deref()),
                );
                alignment::match_person(naming, people, roster)
            }
        };
        if let Some(p) = person {
            keys.push(p.id.as_str());
            keys.extend(p.emails.iter().map(String::as_str));
            name_keys.extend(
                std::iter::once(&p.name)
                    .chain(&p.aliases)
                    .map(String::as_str),
            );
        }
        let forgotten = |at: Option<_>| at.is_some_and(|at| at >= meeting.started_at);
        for key in keys {
            if forgotten(ops::forgotten_since(lib, key)?) {
                continue 'naming;
            }
        }
        for name in name_keys {
            if forgotten(ops::name_forgotten_since(lib, name)?) {
                continue 'naming;
            }
        }
        kept.push(naming.clone());
    }
    Ok(kept)
}

#[cfg(test)]
pub(crate) mod tests {
    use broscribe_core::schema::Person;
    use broscribe_core::schema::{Assignment, Strength, VoiceprintKind};
    use broscribe_core::{MeetingId, SpeakerId, ops};
    use broscribe_models::ModelSet;
    use broscribe_models::fakes::FakeAsr;

    use super::*;
    use crate::import::{ImportOptions, import_file};
    use crate::reprocess::{ReprocessOptions, reprocess};
    use crate::testutil::{Fixture, models};

    const WORDS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

    pub(crate) fn caption_models() -> ModelSet {
        let mut set = models();
        set.asr = Box::new(FakeAsr::new(&WORDS));
        set
    }

    /// Voice 0 for 4 s, a pause, then voice 1 for 4 s; the saved captions
    /// name the second voice (an invented person) while it speaks.
    pub(crate) fn caption_named_meeting(fx: &Fixture) -> MeetingId {
        caption_meeting(fx, "Niko Example")
    }

    /// [`caption_named_meeting`] with captions reading `speaker_name`.
    fn caption_meeting(fx: &Fixture, speaker_name: &str) -> MeetingId {
        let id = import_captioned(fx, speaker_name);
        // The record path's naming, on the imported transcript.
        let embeddings = caption_embeddings(fx, &id);
        let named = ops::with_index(&fx.lib, |tx, index| {
            realign(tx, index, &id, &embeddings, &[])
        })
        .unwrap();
        assert_eq!(named, 1);
        id
    }

    /// What reprocessing embeds outside the lock, for the stored analysis.
    pub(crate) fn caption_embeddings(fx: &Fixture, id: &MeetingId) -> RangeEmbeddings {
        let meeting = fx.lib.load_meeting(id).unwrap();
        embed_saved_captions(&fx.env(), &meeting, &[], caption_models().embedder.as_ref()).unwrap()
    }

    /// Imports the two-voice recording and saves captions reading
    /// `speaker_name` while the second voice speaks, without naming anyone.
    fn import_captioned(fx: &Fixture, speaker_name: &str) -> MeetingId {
        import_captioned_turns(
            fx,
            speaker_name,
            &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0), (None, 1.0)],
        )
    }

    /// [`import_captioned`] of a recording of `turns`; the captions still
    /// cover 5.5–8.5 s only.
    fn import_captioned_turns(
        fx: &Fixture,
        speaker_name: &str,
        turns: &[(Option<usize>, f64)],
    ) -> MeetingId {
        let wav = fx.wav("captions.wav", turns);
        let id = import_file(
            &fx.env(),
            &mut caption_models(),
            &wav,
            &ImportOptions::default(),
        )
        .unwrap()
        .meeting_id;
        let event = broscribe_readers::ReaderEvent::Caption(broscribe_readers::CaptionEvent {
            caption_id: "synthetic-caption".into(),
            revision: 1,
            speaker_name: speaker_name.into(),
            is_self: false,
            text: WORDS.join(" "),
            observed_s: 8.5,
            start_s: 5.5,
            end_s: 8.5,
        });
        let dir = fx.lib.meeting_dir(&id).unwrap();
        std::fs::write(
            dir.join("captions.jsonl"),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        id
    }

    pub(crate) fn niko(fx: &Fixture) -> Person {
        fx.lib
            .list_people()
            .unwrap()
            .into_iter()
            .find(|p| p.name == "Niko Example")
            .unwrap()
    }

    pub(crate) fn second_voice(fx: &Fixture, id: &MeetingId) -> broscribe_core::schema::Speaker {
        let meeting = fx.lib.load_meeting(id).unwrap();
        let speaker = meeting
            .segments
            .iter()
            .find(|s| s.start <= 7.0 && s.end >= 7.0)
            .and_then(|s| s.speaker)
            .unwrap();
        meeting.speaker(speaker).unwrap().clone()
    }

    #[test]
    fn caption_sample_embeds_only_agreed_clean_speech_and_reembeds_alike() {
        let fx = Fixture::new();
        // The second voice speaks twice; the captions agree with its first turn only.
        let id = import_captioned_turns(
            &fx,
            "Niko Example",
            &[
                (Some(0), 4.0),
                (None, 1.0),
                (Some(1), 4.0),
                (None, 1.0),
                (Some(1), 4.0),
                (None, 1.0),
            ],
        );
        let embeddings = caption_embeddings(&fx, &id);
        let named = ops::with_index(&fx.lib, |tx, index| {
            realign(tx, index, &id, &embeddings, &[])
        })
        .unwrap();
        assert_eq!(named, 1);
        let person = niko(&fx);
        let speaker = second_voice(&fx, &id);
        assert_eq!(speaker.person_id.as_ref(), Some(&person.id));
        let clean = speaker.centroid.as_ref().unwrap().ranges.clone();
        assert!(
            clean.iter().any(|r| r[0] >= 9.5),
            "the uncaptioned turn is clean speech of the speaker: {clean:?}"
        );
        let sample = fx
            .lib
            .load_voiceprints(&person.id)
            .unwrap()
            .embeddings
            .into_iter()
            .find(|v| v.source.kind == VoiceprintKind::Captions)
            .unwrap();
        let ranges = sample.source.ranges.clone();
        assert!(!ranges.is_empty());
        assert!(
            ranges.iter().all(|r| r[0] >= 4.5 && r[1] <= 9.5),
            "only the agreed turn: {ranges:?}"
        );
        // The vector is the embedding of exactly the recorded ranges.
        let meeting = fx.lib.load_meeting(&id).unwrap();
        let source = &crate::audio::meeting_tracks(&meeting)[0];
        let audio =
            crate::audio::decode_meeting_track(&fx.lib, fx.env().decoders, &meeting, source)
                .unwrap();
        let turns: Vec<_> = ranges
            .iter()
            .map(|&[a, b]| (f64::from(a), f64::from(b)))
            .collect();
        let models = caption_models();
        let expected = crate::analyze::centroid_of(&audio, &turns, models.embedder.as_ref())
            .unwrap()
            .unwrap()
            .vector()
            .unwrap();
        let close = |a: &[f32], b: &[f32]| {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
        };
        assert!(close(&sample.vector().unwrap(), &expected));
        // Re-embedding the sample (as after a model switch) rebuilds it.
        fx.lib
            .write(|tx| {
                let mut prints = tx.library().load_voiceprints(&person.id)?;
                for print in &mut prints.embeddings {
                    if print.id == sample.id {
                        print.model = "older-model".into();
                    }
                }
                tx.save_voiceprints(&prints)
            })
            .unwrap();
        crate::reembed(&fx.lib, fx.env().decoders, models.embedder.as_ref()).unwrap();
        let rebuilt = fx
            .lib
            .load_voiceprints(&person.id)
            .unwrap()
            .embeddings
            .into_iter()
            .find(|v| {
                v.source.kind == VoiceprintKind::Captions
                    && v.model == models.embedder.model_id()
                    && v.source.ranges == ranges
            })
            .expect("re-embedded caption sample");
        assert!(close(&rebuilt.vector().unwrap(), &expected));
    }

    #[test]
    fn caption_replay_renames_after_resolution_without_self_matching() {
        let fx = Fixture::new();
        let id = caption_named_meeting(&fx);
        let person = niko(&fx);
        assert_eq!(second_voice(&fx, &id).person_id.as_ref(), Some(&person.id));
        let prints = fx.lib.load_voiceprints(&person.id).unwrap();
        assert!(prints.embeddings.iter().all(|v| {
            v.source.meeting_id.as_ref() == Some(&id) && v.strength == Strength::Weak
        }));
        assert!(
            prints
                .embeddings
                .iter()
                .any(|v| v.source.kind == VoiceprintKind::Captions)
        );
        let model = caption_models().embedder.model_id().to_owned();
        let embeddings = caption_embeddings(&fx, &id);
        ops::with_index(&fx.lib, |tx, index| {
            // A fresh analysis: unassigned speakers, then identity resolution.
            let mut meeting = tx.library().load_meeting(&id)?;
            for speaker in &mut meeting.speakers {
                speaker.person_id = None;
                speaker.assignment = Assignment::None;
                speaker.score = None;
                speaker.suggestion = None;
            }
            tx.save_meeting(&meeting)?;
            crate::finish::finish_meeting(tx, index, &id, &model, None)?;
            // Only this meeting's own samples exist: they must not name it.
            let resolved = tx.library().load_meeting(&id)?;
            assert!(resolved.speakers.iter().all(|s| s.person_id.is_none()));
            assert_eq!(realign(tx, index, &id, &embeddings, &[])?, 1);
            // Idempotent once applied.
            assert_eq!(realign(tx, index, &id, &embeddings, &[])?, 0);
            Ok(())
        })
        .unwrap();
        let speaker = second_voice(&fx, &id);
        assert_eq!(speaker.person_id.as_ref(), Some(&person.id));
        assert_eq!(speaker.assignment, Assignment::Auto);
        let people = fx.lib.list_people().unwrap();
        assert_eq!(
            people.iter().filter(|p| p.name == "Niko Example").count(),
            1
        );
    }

    #[test]
    fn caption_replay_respects_confirmed_speakers_and_rejections() {
        let fx = Fixture::new();
        let id = caption_named_meeting(&fx);
        let person = niko(&fx);
        let speaker_id: SpeakerId = second_voice(&fx, &id).id;
        let embeddings = caption_embeddings(&fx, &id);
        ops::with_index(&fx.lib, |tx, index| {
            let mut meeting = tx.library().load_meeting(&id)?;
            let speaker = meeting.speaker_mut(speaker_id).unwrap();
            speaker.person_id = None;
            speaker.assignment = Assignment::None;
            speaker.rejected_person_ids = vec![person.id.clone()];
            tx.save_meeting(&meeting)?;
            assert_eq!(realign(tx, index, &id, &embeddings, &[])?, 0);
            Ok(())
        })
        .unwrap();
        assert!(second_voice(&fx, &id).person_id.is_none());
    }

    #[test]
    fn caption_replay_holds_a_speaker_with_a_different_user_line_name() {
        let fx = Fixture::new();
        let id = caption_named_meeting(&fx);
        let caption_person = niko(&fx);
        let speaker_id = second_voice(&fx, &id).id;
        let person = Person::new(fx.lib.new_person_id(), "Synthetic Blythe");
        let embeddings = caption_embeddings(&fx, &id);
        ops::with_index(&fx.lib, |tx, index| {
            tx.save_person(&person)?;
            index.upsert_person(tx, &person)?;
            let mut meeting = tx.library().load_meeting(&id)?;
            let line = meeting
                .segments
                .iter()
                .find(|s| s.speaker == Some(speaker_id))
                .map(ops::segment_fingerprint)
                .unwrap();
            meeting
                .utterance_people
                .push(broscribe_core::schema::UtterancePerson {
                    track: line.track,
                    start: line.start,
                    end: line.end,
                    text_sha256: line.text_sha256,
                    person_id: person.id.clone(),
                    at: broscribe_core::schema::now(),
                    live_decision: None,
                });
            let speaker = meeting.speaker_mut(speaker_id).unwrap();
            speaker.assignment = Assignment::None;
            speaker.person_id = None;
            tx.save_meeting(&meeting)?;
            tx.save_voiceprints(&broscribe_core::schema::Voiceprints::empty(
                caption_person.id.clone(),
            ))?;
            assert_eq!(realign(tx, index, &id, &embeddings, &[])?, 0);
            Ok(())
        })
        .unwrap();
        let saved = fx.lib.load_meeting(&id).unwrap();
        assert_eq!(
            saved.speaker(speaker_id).unwrap().assignment,
            Assignment::None
        );
        assert!(saved.review_notes.iter().any(
            |note| note.text.contains(&caption_person.name) && note.text.contains(&person.name)
        ));
        assert!(
            fx.lib
                .load_voiceprints(&caption_person.id)
                .unwrap()
                .embeddings
                .is_empty()
        );
        reprocess_captions(&fx, &id);
        let saved = fx.lib.load_meeting(&id).unwrap();
        assert!(
            saved
                .speakers
                .iter()
                .all(|s| s.named_person() != Some(&caption_person.id))
        );
        assert!(saved.review_notes.iter().any(
            |note| note.text.contains(&caption_person.name) && note.text.contains(&person.name)
        ));
    }

    #[test]
    fn reprocess_keeps_caption_name_when_asr_still_matches() {
        let fx = Fixture::new();
        let id = caption_named_meeting(&fx);
        let person = niko(&fx);
        let out = reprocess(
            &fx.env(),
            &mut caption_models(),
            &id,
            ReprocessOptions::default(),
        )
        .unwrap();
        assert!(out.skipped.is_none());
        let speaker = second_voice(&fx, &id);
        assert_eq!(speaker.person_id.as_ref(), Some(&person.id));
        assert_eq!(speaker.assignment, Assignment::Auto);
        // Replay sees the samples under the reprocessed speaker IDs.
        let prints = fx.lib.load_voiceprints(&person.id).unwrap();
        assert!(
            prints
                .embeddings
                .iter()
                .filter(|v| v.source.kind == VoiceprintKind::Captions)
                .all(|v| v.source.speaker_id.is_none_or(|s| s == speaker.id))
        );
    }

    fn reprocess_captions(fx: &Fixture, id: &MeetingId) {
        let out = reprocess(
            &fx.env(),
            &mut caption_models(),
            id,
            ReprocessOptions::default(),
        )
        .unwrap();
        assert!(out.skipped.is_none());
    }

    #[test]
    fn reprocess_never_recreates_a_forgotten_caption_person() {
        let fx = Fixture::new();
        let id = caption_named_meeting(&fx);
        let person = niko(&fx);
        ops::with_index(&fx.lib, |tx, index| {
            ops::forget_person(tx, index, person.id.as_str())
        })
        .unwrap();
        reprocess_captions(&fx, &id);
        let people = fx.lib.list_people().unwrap();
        assert!(
            people.iter().all(|p| p.name != "Niko Example"),
            "{people:?}"
        );
        assert!(!fx.lib.person_dir(&person.id).exists());
        for p in &people {
            assert!(
                fx.lib
                    .load_voiceprints(&p.id)
                    .unwrap()
                    .embeddings
                    .iter()
                    .all(|v| v.source.meeting_id.as_ref() != Some(&id))
            );
        }
        assert!(second_voice(&fx, &id).person_id.is_none());
    }

    #[test]
    fn reprocess_never_relearns_a_forgotten_caption_voice() {
        let fx = Fixture::new();
        let id = caption_named_meeting(&fx);
        let person = niko(&fx);
        ops::with_index(&fx.lib, |tx, index| {
            ops::forget_voice(tx, index, person.id.as_str())
        })
        .unwrap();
        reprocess_captions(&fx, &id);
        assert!(
            fx.lib
                .load_voiceprints(&person.id)
                .unwrap()
                .embeddings
                .is_empty()
        );
        assert_ne!(second_voice(&fx, &id).person_id.as_ref(), Some(&person.id));
    }

    #[test]
    fn reprocess_keeps_a_renamed_caption_person_without_duplicating_it() {
        let fx = Fixture::new();
        let id = caption_named_meeting(&fx);
        let mut person = niko(&fx);
        person.name = "Nikolai Renamed".into();
        ops::with_index(&fx.lib, |tx, index| {
            tx.save_person(&person)?;
            index.upsert_person(tx, &person)
        })
        .unwrap();
        reprocess_captions(&fx, &id);
        let people = fx.lib.list_people().unwrap();
        assert!(
            people.iter().all(|p| p.name != "Niko Example"),
            "{people:?}"
        );
        assert_eq!(second_voice(&fx, &id).person_id.as_ref(), Some(&person.id));
    }

    /// Existing person `name`, named by captions reading `spelling` (no
    /// roster e-mail), is forgotten; reprocessing neither recreates them nor
    /// learns their voice. `legacy` strips the caption link first, as in a
    /// meeting named before links were recorded for existing people, so only
    /// the normalised name tombstones can catch it.
    fn assert_forgotten_existing_person_stays_forgotten(name: &str, spelling: &str, legacy: bool) {
        let fx = Fixture::new();
        let daria = Person::new(fx.lib.new_person_id(), name);
        ops::with_index(&fx.lib, |tx, index| {
            tx.save_person(&daria)?;
            index.upsert_person(tx, &daria)
        })
        .unwrap();
        let id = caption_meeting(&fx, spelling);
        assert_eq!(second_voice(&fx, &id).person_id.as_ref(), Some(&daria.id));
        let linked = |m: &Meeting| {
            m.participants.iter().any(|p| {
                p.person_id.as_ref() == Some(&daria.id)
                    && p.evidence.iter().any(|e| e == "captions")
            })
        };
        assert!(linked(&fx.lib.load_meeting(&id).unwrap()), "{spelling}");
        if legacy {
            ops::with_index(&fx.lib, |tx, index| {
                let mut meeting = tx.library().load_meeting(&id)?;
                meeting
                    .participants
                    .retain(|p| !p.evidence.iter().any(|e| e == "captions"));
                tx.save_meeting(&meeting)?;
                index.upsert_meeting(tx, &meeting)
            })
            .unwrap();
        }
        ops::with_index(&fx.lib, |tx, index| {
            ops::forget_person(tx, index, daria.id.as_str())
        })
        .unwrap();
        reprocess_captions(&fx, &id);
        let people = fx.lib.list_people().unwrap();
        assert!(people.is_empty(), "{spelling} legacy={legacy}: {people:?}");
        assert!(!fx.lib.person_dir(&daria.id).exists());
        assert!(second_voice(&fx, &id).person_id.is_none());
    }

    /// (person name, caption spelling): accents, name order and suffixes,
    /// on either side.
    const SPELLINGS: [(&str, &str); 3] = [
        ("Dária Solís", "Daria Solis"),
        ("Dária Solís", "Solis, Daria"),
        ("Daria Solis", "Solís, Dária (External)"),
    ];

    #[test]
    fn reprocess_never_recreates_a_forgotten_existing_person_named_by_another_spelling() {
        for (name, spelling) in SPELLINGS {
            assert_forgotten_existing_person_stays_forgotten(name, spelling, false);
        }
    }

    #[test]
    fn reprocess_honours_normalised_name_tombstones_without_a_caption_link() {
        for (name, spelling) in SPELLINGS {
            assert_forgotten_existing_person_stays_forgotten(name, spelling, true);
        }
    }

    /// The captions name a speaker that other-meeting evidence already named
    /// correctly: replay changes no assignment but must still save the
    /// caption link, so a later rename and forget keeps the name forgotten.
    #[test]
    fn reprocess_saves_the_caption_link_of_an_already_named_speaker() {
        let mut fx = Fixture::new();
        fx.config.identity.insert(
            fx.config.profile.speaker_embedding.clone(),
            broscribe_core::config::Thresholds {
                auto_name: Some(0.8),
                suggest: Some(0.6),
                margin: Some(0.1),
                merge: Some(0.95),
                auto_merge: Some(0.95),
            },
        );
        let daria = Person::new(fx.lib.new_person_id(), "Daria Solis");
        ops::with_index(&fx.lib, |tx, index| {
            tx.save_person(&daria)?;
            index.upsert_person(tx, &daria)
        })
        .unwrap();
        // Another meeting confirms the second voice as Daria.
        let wav = fx.wav("other.wav", &[(Some(1), 4.0), (None, 1.0)]);
        let other = import_file(
            &fx.env(),
            &mut caption_models(),
            &wav,
            &ImportOptions::default(),
        )
        .unwrap()
        .meeting_id;
        let other_speaker = fx.lib.load_meeting(&other).unwrap().segments[0]
            .speaker
            .unwrap();
        let model = caption_models().embedder.model_id().to_owned();
        ops::with_index(&fx.lib, |tx, index| {
            ops::name_speaker(
                tx,
                index,
                &other,
                other_speaker,
                daria.id.as_str(),
                Vec::new(),
                &model,
            )
        })
        .unwrap();
        let id = import_captioned(&fx, "Daria Solis");
        let speaker = second_voice(&fx, &id);
        assert_eq!(speaker.person_id.as_ref(), Some(&daria.id));
        assert_eq!(speaker.assignment, Assignment::Auto);
        let linked = |m: &Meeting| {
            m.participants.iter().any(|p| {
                p.person_id.as_ref() == Some(&daria.id)
                    && p.evidence.iter().any(|e| e == "captions")
            })
        };
        assert!(!linked(&fx.lib.load_meeting(&id).unwrap()));

        reprocess_captions(&fx, &id);
        assert_eq!(second_voice(&fx, &id).person_id.as_ref(), Some(&daria.id));
        assert!(linked(&fx.lib.load_meeting(&id).unwrap()));

        // Renamed, the caption name no longer matches Daria's tombstones;
        // only the saved link marks the caption naming as forgotten.
        let mut renamed = daria.clone();
        renamed.name = "Dana Renamed".into();
        ops::with_index(&fx.lib, |tx, index| {
            tx.save_person(&renamed)?;
            index.upsert_person(tx, &renamed)?;
            ops::forget_person(tx, index, daria.id.as_str()).map(drop)
        })
        .unwrap();
        reprocess_captions(&fx, &id);
        let people = fx.lib.list_people().unwrap();
        assert!(people.is_empty(), "{people:?}");
        assert!(!fx.lib.person_dir(&daria.id).exists());
        assert!(second_voice(&fx, &id).person_id.is_none());
    }

    /// Diarizes like [`crate::testutil::ToneDiarizer`], but labels the second
    /// voice's turns from 9.5 s on as another cluster: one over-split voice.
    struct SplitDiarizer;

    impl broscribe_models::Diarizer for SplitDiarizer {
        fn diarize(
            &self,
            samples_16k: &[f32],
        ) -> broscribe_core::Result<Vec<broscribe_models::DiarSegment>> {
            let mut turns = crate::testutil::ToneDiarizer.diarize(samples_16k)?;
            for turn in &mut turns {
                if turn.speaker == 1 && turn.start_s >= 9.5 {
                    turn.speaker = 3;
                }
            }
            Ok(turns)
        }
    }

    /// Two short captions, one per turn of the over-split voice, name it
    /// only once reprocessing groups the two clusters (a two-word caption
    /// needs a second agreeing span; captions align with this publish's
    /// groups folded). The caption sample must still be embedded for that
    /// voice, both clusters stay as diarized and the group survives.
    #[test]
    fn reprocess_embeds_caption_samples_for_namings_that_need_the_merge() {
        let mut fx = Fixture::new();
        fx.config.identity.insert(
            fx.config.profile.speaker_embedding.clone(),
            broscribe_core::config::Thresholds {
                auto_name: Some(0.8),
                suggest: Some(0.6),
                margin: Some(0.1),
                merge: Some(0.95),
                auto_merge: Some(0.95),
            },
        );
        let split = || {
            let mut set = caption_models();
            set.diarizer = Box::new(SplitDiarizer);
            set
        };
        let wav = fx.wav(
            "split.wav",
            &[
                (Some(0), 4.0),
                (None, 1.0),
                (Some(1), 4.0),
                (None, 1.0),
                (Some(1), 4.0),
                (None, 1.0),
            ],
        );
        let id = import_file(&fx.env(), &mut split(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let captions: String = [
            ("c1", "alpha beta", 5.5, 8.5),
            ("c2", "gamma delta", 10.5, 13.5),
        ]
        .into_iter()
        .map(|(caption_id, text, start_s, end_s)| {
            let event = broscribe_readers::ReaderEvent::Caption(broscribe_readers::CaptionEvent {
                caption_id: caption_id.into(),
                revision: 1,
                speaker_name: "Niko Example".into(),
                is_self: false,
                text: text.into(),
                observed_s: end_s,
                start_s,
                end_s,
            });
            format!("{}\n", serde_json::to_string(&event).unwrap())
        })
        .collect();
        let dir = fx.lib.meeting_dir(&id).unwrap();
        std::fs::write(dir.join("captions.jsonl"), captions).unwrap();

        let out = reprocess(&fx.env(), &mut split(), &id, ReprocessOptions::default()).unwrap();
        assert!(out.skipped.is_none());
        let meeting = fx.lib.load_meeting(&id).unwrap();
        let at = |t: f64| {
            meeting
                .segments
                .iter()
                .find(|s| s.start <= t && s.end >= t)
                .and_then(|s| s.speaker)
                .unwrap()
        };
        assert_ne!(at(7.0), at(12.0), "clusters are never folded");
        assert_eq!(
            ops::effective_speaker(&meeting, at(7.0)),
            ops::effective_speaker(&meeting, at(12.0)),
            "the over-split clusters were grouped"
        );
        let person = niko(&fx);
        let speaker = second_voice(&fx, &id);
        assert_eq!(speaker.person_id.as_ref(), Some(&person.id));
        let samples: Vec<_> = fx
            .lib
            .load_voiceprints(&person.id)
            .unwrap()
            .embeddings
            .into_iter()
            .filter(|v| v.source.kind == VoiceprintKind::Captions)
            .collect();
        assert_eq!(samples.len(), 1, "{samples:?}");
        let ranges = &samples[0].source.ranges;
        assert!(
            ranges.iter().any(|r| r[1] <= 9.5) && ranges.iter().any(|r| r[0] >= 9.5),
            "both captioned turns: {ranges:?}"
        );
    }
}

#[cfg(test)]
mod bounded_caption_tests {
    use super::*;
    use broscribe_core::schema::{Centroid, Source, Speaker};
    use broscribe_core::{MeetingId, SpeakerId};

    struct TwoParts;
    impl SpeakerEmbedder for TwoParts {
        fn model_id(&self) -> &str {
            "two-parts"
        }
        fn dim(&self) -> usize {
            2
        }
        fn max_input_samples(&self) -> Option<usize> {
            Some(2 * 16_000)
        }
        fn embed(&self, input: &[f32]) -> Result<Option<Vec<f32>>> {
            if input.len() > 2 * 16_000 {
                return Err(broscribe_core::Error::Inference(
                    "input exceeds capacity".into(),
                ));
            }
            Ok(Some(if input[0] > 0.0 {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            }))
        }
    }

    #[test]
    fn caption_range_lookup_combines_every_native_piece() {
        let mut meeting = Meeting::new(
            MeetingId::random(),
            Source::Import,
            broscribe_core::schema::now(),
        );
        let id = SpeakerId::new(1).unwrap();
        let mut speaker = Speaker::new(id, "t0");
        let mut centroid = Centroid::new("two-parts", &[1.0, 0.0], 1);
        centroid.ranges = vec![[0.0, 4.0]];
        speaker.centroid = Some(centroid);
        meeting.speakers.push(speaker);
        let naming = CaptionNaming {
            speaker_id: id,
            name: "Alex Example".into(),
            is_self: false,
            distinct_spans: 3,
            purity: 1.0,
            start_s: 0.0,
            end_s: 4.0,
            ranges: vec![[0.0, 4.0]],
        };
        let mut audio = vec![0.2; 2 * 16_000];
        audio.extend(std::iter::repeat_n(-0.2, 2 * 16_000));
        let result = embed_caption_ranges(&meeting, &[naming], &TwoParts, |_| {
            Ok(Some((Cow::Borrowed(&audio), 0.0)))
        })
        .unwrap();
        let vector = result
            .get("t0", [0.0, 4.0])
            .expect("original agreed-range key");
        assert!((vector[0] - 0.70710677).abs() < 1e-5);
        assert!(
            (vector[1] - 0.70710677).abs() < 1e-5,
            "must not use only the last part"
        );
    }
}
