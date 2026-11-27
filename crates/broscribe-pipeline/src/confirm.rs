//! Embeddings for confirming a speaker (`speakers name`, the worker's
//! `confirm-samples` jobs) and for enrolling the user's voice: from a file
//! range, or from the read-aloud passage (`setup voice`), with its audio
//! checks, VAD chunks and the confirmation-phrase check (plan §8 items 11,
//! 13, 18). Inference only: call these before taking the library lock.

use std::path::Path;

use broscribe_core::identity;
use broscribe_core::ops::ConfirmationSample;
use broscribe_core::{Library, MeetingId, PersonId, Result, SpeakerId};
use broscribe_models::SpeakerEmbedder;
use broscribe_platform::DecoderChain;
use serde::Serialize;

use crate::analyze::{MIN_CLEAN_S, embed_ranges, stored_clean_turns};
use crate::audio::{TARGET_RATE, decode_16k, slice_s};
use crate::ranges::Range;

/// Shortest enrollment chunk embedded.
pub const ENROLL_MIN_CHUNK_S: f64 = 3.0;
/// Long speech runs are cut into pieces of at least this length.
pub const ENROLL_TARGET_CHUNK_S: f64 = 5.0;
/// Chunks a passage must give to be enrolled.
pub const ENROLL_MIN_CHUNKS: usize = 4;
/// Pauses shorter than this inside a passage are bridged.
const ENROLL_BRIDGE_S: f64 = 1.0;
/// A sample at or above this magnitude counts as clipped.
pub const CLIP_LEVEL: f32 = 0.99;
/// Clipping warning above this share of samples.
pub const MAX_CLIPPED_FRACTION: f64 = 0.005;
/// Low-level warning when speech RMS is below this.
pub const MIN_SPEECH_DBFS: f64 = -40.0;
/// Short-speech warning below this much speech.
pub const MIN_SPEECH_S: f64 = 10.0;
/// Tentative warning when speech is less than this far above a pause estimate.
pub const MIN_SNR_DB: f64 = 6.0;
/// Minimum continuous reference after excluding speech-adjacent edges.
const MIN_NOISE_S: f64 = 0.5;
/// Exclude 250 ms beside detected speech from noise references.
const NOISE_GUARD_SAMPLES: usize = TARGET_RATE as usize / 4;
/// Estimate reference power in 20 ms frames.
const NOISE_FRAME_SAMPLES: usize = TARGET_RATE as usize / 50;

/// Embeds physically usable ranges in model-bounded parts. Sources shorter
/// than 1.5 s or too short for the model are skipped.
pub fn embed_sample_ranges(
    samples: &[f32],
    embedder: &dyn SpeakerEmbedder,
    ranges: &[Range],
) -> Result<Vec<ConfirmationSample>> {
    let ranges: Vec<Range> = ranges
        .iter()
        .copied()
        .filter(|(a, b)| {
            *a >= 0.0
                && b - a >= MIN_CLEAN_S
                && slice_s(samples, *a, *b).len() >= (MIN_CLEAN_S * f64::from(TARGET_RATE)) as usize
        })
        .collect();
    Ok(to_samples(embed_ranges(samples, &ranges, embedder)?))
}

/// Enrollment chunks from VAD speech ranges: pauses under 1 s are bridged,
/// runs are cut into equal pieces of at least 5 s, and pieces under 3 s are
/// dropped.
pub fn enrollment_chunks(speech: &[Range]) -> Vec<Range> {
    let mut runs: Vec<Range> = Vec::new();
    let mut sorted = speech.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (a, b) in sorted {
        match runs.last_mut() {
            Some(last) if a - last.1 < ENROLL_BRIDGE_S => last.1 = last.1.max(b),
            _ => runs.push((a, b)),
        }
    }
    let mut out = Vec::new();
    for (a, b) in runs {
        let len = b - a;
        if len < ENROLL_MIN_CHUNK_S {
            continue;
        }
        let n = ((len / ENROLL_TARGET_CHUNK_S).floor() as usize).max(1);
        let step = len / n as f64;
        for i in 0..n {
            let start = a + step * i as f64;
            let end = if i + 1 == n { b } else { start + step };
            out.push((start, end));
        }
    }
    out
}

/// Level, noise and clipping of a recording (plan §8 item 13).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AudioChecks {
    pub duration_s: f64,
    pub speech_s: f64,
    /// RMS of the speech parts; `None` without speech.
    pub speech_rms_dbfs: Option<f64>,
    /// Median 20 ms frame level from sufficiently long, guarded pauses.
    /// `None` without a reference or when its median power is digital zero.
    pub noise_rms_dbfs: Option<f64>,
    /// Duration of full frames used for the estimate; zero means unavailable.
    pub noise_reference_s: f64,
    /// Estimated speech-to-pause level contrast, not proof of room noise.
    pub snr_db: Option<f64>,
    pub clipped_fraction: f64,
    /// `clipping`, `low_level`, `short_speech`, `noisy`.
    pub warnings: Vec<&'static str>,
}

fn dbfs(sum_sq: f64, n: usize) -> Option<f64> {
    if n == 0 || sum_sq <= 0.0 {
        return None;
    }
    Some(10.0 * (sum_sq / n as f64).log10())
}

fn pause_noise_estimate(samples: &[f32], is_speech: &[bool], noise_n: usize) -> (Option<f64>, f64) {
    let minimum = (MIN_NOISE_S * f64::from(TARGET_RATE)) as usize;
    let mut powers = Vec::new();
    let mut cursor = 0;
    while cursor < samples.len() {
        if is_speech[cursor] {
            cursor += 1;
            continue;
        }
        let run_start = cursor;
        while cursor < samples.len() && !is_speech[cursor] {
            cursor += 1;
        }
        let start = if run_start == 0 {
            0
        } else {
            run_start.saturating_add(NOISE_GUARD_SAMPLES)
        };
        let end = if cursor == samples.len() {
            cursor
        } else {
            cursor.saturating_sub(NOISE_GUARD_SAMPLES)
        };
        if end.saturating_sub(start) < minimum {
            continue;
        }
        if powers.capacity() == 0 {
            // Every reference frame is wholly inside the original non-speech
            // samples, so this bounds all frames without allocating for reads
            // that have no admissible reference.
            powers.reserve_exact(noise_n / NOISE_FRAME_SAMPLES);
        }
        for frame in samples[start..end].as_chunks::<NOISE_FRAME_SAMPLES>().0 {
            let sum: f64 = frame
                .iter()
                .map(|&x| {
                    let x = f64::from(x);
                    x * x
                })
                .sum();
            powers.push(sum / NOISE_FRAME_SAMPLES as f64);
        }
    }
    let count = powers.len();
    if count == 0 {
        return (None, 0.0);
    }
    let reference_s = (count * NOISE_FRAME_SAMPLES) as f64 / f64::from(TARGET_RATE);
    let (lower, middle, _) = powers.select_nth_unstable_by(count / 2, f64::total_cmp);
    let power = if count.is_multiple_of(2) {
        (*middle + lower.iter().copied().fold(0.0, f64::max)) / 2.0
    } else {
        *middle
    };
    (dbfs(power, 1), reference_s)
}

/// Checks 16 kHz `samples` whose speech is `speech`: clipping above 0.5 %,
/// low speech level below −40 dBFS, short speech under 10 s, and a tentative
/// noise warning below 6 dB speech-to-guarded-pause contrast.
pub fn audio_checks(samples: &[f32], speech: &[Range]) -> AudioChecks {
    let rate = f64::from(TARGET_RATE);
    let mut is_speech = vec![false; samples.len()];
    for &(a, b) in speech {
        let start = ((a.max(0.0) * rate) as usize).min(samples.len());
        let end = ((b.max(0.0) * rate) as usize).clamp(start, samples.len());
        is_speech[start..end].iter_mut().for_each(|s| *s = true);
    }
    let (mut speech_sq, mut speech_n, mut noise_n, mut clipped) = (0.0f64, 0usize, 0usize, 0usize);
    for (&x, &sp) in samples.iter().zip(&is_speech) {
        if sp {
            let x = f64::from(x);
            speech_sq += x * x;
            speech_n += 1;
        } else {
            noise_n += 1;
        }
        if x.abs() >= CLIP_LEVEL {
            clipped += 1;
        }
    }
    let speech_s = speech_n as f64 / rate;
    let speech_rms_dbfs = dbfs(speech_sq, speech_n);
    let (noise_rms_dbfs, noise_reference_s) = pause_noise_estimate(samples, &is_speech, noise_n);
    let snr_db = speech_rms_dbfs.zip(noise_rms_dbfs).map(|(s, n)| s - n);
    let clipped_fraction = if samples.is_empty() {
        0.0
    } else {
        clipped as f64 / samples.len() as f64
    };
    let mut warnings = Vec::new();
    if clipped_fraction > MAX_CLIPPED_FRACTION {
        warnings.push("clipping");
    }
    if speech_rms_dbfs.is_none_or(|d| d < MIN_SPEECH_DBFS) {
        warnings.push("low_level");
    }
    if speech_s < MIN_SPEECH_S {
        warnings.push("short_speech");
    }
    if snr_db.is_some_and(|s| s < MIN_SNR_DB) {
        warnings.push("noisy");
    }
    AudioChecks {
        duration_s: samples.len() as f64 / rate,
        speech_s,
        speech_rms_dbfs,
        noise_rms_dbfs,
        noise_reference_s,
        snr_db,
        clipped_fraction,
        warnings,
    }
}

/// Result of the confirmation-phrase check.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PhraseCheck {
    /// Mean cosine of the phrase to its three closest passage chunks.
    pub score: f32,
    pub threshold: f32,
    /// `calibrated` (the model's `suggest` threshold) or `relative`.
    pub method: &'static str,
    pub passed: bool,
}

fn median(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    let m = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[m - 1] + v[m]) / 2.0
    } else {
        v[m]
    }
}

/// Plan §8 item 11: with a calibrated `suggest` threshold the phrase's
/// top-3 mean cosine to the chunks must reach it; otherwise it must reach
/// median(pairwise chunk cosines) − 3·max(1.4826·MAD, 0.03).
pub fn phrase_check(phrase: &[f32], chunks: &[Vec<f32>], suggest: Option<f32>) -> PhraseCheck {
    let mut sims: Vec<f32> = chunks.iter().map(|c| identity::cosine(phrase, c)).collect();
    sims.sort_by(|a, b| b.total_cmp(a));
    let top = &sims[..sims.len().min(3)];
    let score = if top.is_empty() {
        0.0
    } else {
        top.iter().sum::<f32>() / top.len() as f32
    };
    let (threshold, method) = match suggest {
        Some(t) => (t, "calibrated"),
        None => {
            let mut pairs = Vec::new();
            for i in 0..chunks.len() {
                for j in i + 1..chunks.len() {
                    pairs.push(identity::cosine(&chunks[i], &chunks[j]));
                }
            }
            let med = median(&mut pairs.clone());
            let mut dev: Vec<f32> = pairs.iter().map(|p| (p - med).abs()).collect();
            let mad = median(&mut dev);
            (med - 3.0 * (1.4826 * mad).max(0.03), "relative")
        }
    };
    PhraseCheck {
        score,
        threshold,
        method,
        passed: !chunks.is_empty() && score >= threshold,
    }
}

/// The phrase's embedding over the span of its speech (the whole clip when
/// VAD found none); `None` when too short for the model.
pub fn embed_phrase(
    samples: &[f32],
    speech: &[Range],
    embedder: &dyn SpeakerEmbedder,
) -> Result<Option<Vec<f32>>> {
    let start = speech.iter().map(|r| r.0).fold(f64::INFINITY, f64::min);
    let end = speech.iter().map(|r| r.1).fold(0.0, f64::max);
    let clip = if start < end {
        slice_s(samples, start, end)
    } else {
        samples
    };
    embedder.embed(clip)
}

/// Embeddings for the 20 longest clean source turns of `speaker` (consecutive
/// segments ≥ 1.5 s), subdivided as needed by the model's declared capacity.
/// A grouped member stands for its group's kept speaker, the one
/// `ops::name_speaker` names, so the samples come from that speaker's audio.
///
/// Empty when the recording is not available or no turn could be embedded;
/// `ops::name_speaker` then falls back to the speaker's centroid.
pub fn confirmation_samples(
    lib: &Library,
    decoders: &DecoderChain,
    embedder: &dyn SpeakerEmbedder,
    meeting: &MeetingId,
    speaker: SpeakerId,
) -> Result<Vec<ConfirmationSample>> {
    confirmation_samples_for(lib, decoders, embedder, meeting, speaker, None)
}

pub(crate) fn confirmation_samples_for(
    lib: &Library,
    decoders: &DecoderChain,
    embedder: &dyn SpeakerEmbedder,
    meeting: &MeetingId,
    speaker: SpeakerId,
    person: Option<&PersonId>,
) -> Result<Vec<ConfirmationSample>> {
    let m = lib.load_meeting(meeting)?;
    if m.speaker(speaker).is_none() {
        return Err(broscribe_core::Error::NotFound(format!(
            "speaker {speaker} in meeting {meeting}"
        )));
    }
    let speaker = broscribe_core::ops::effective_speaker(&m, speaker);
    let mut turns = stored_clean_turns(&m, speaker);
    if let Some(person) = person {
        let evidence = broscribe_core::ops::UserSpeakerEvidence::of(&m);
        turns.retain(|&(a, b)| evidence.allows_sample(speaker, person, a, b));
    }
    if turns.is_empty() {
        return Ok(Vec::new());
    }
    let Some(source) = crate::audio::meeting_tracks(&m).into_iter().find(|s| {
        m.speaker(speaker)
            .is_some_and(|speaker| speaker.track == s.track)
    }) else {
        return Ok(Vec::new());
    };
    let path = if source.path.is_absolute() {
        source.path.clone()
    } else {
        lib.meeting_dir(&m.id)?.join(&source.path)
    };
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let audio = crate::audio::decode_meeting_track(lib, decoders, &m, &source)?;
    let local_turns: Vec<_> = turns
        .iter()
        .map(|(a, b)| ((a - source.offset_s).max(0.0), b - source.offset_s))
        .filter(|(a, b)| b > a)
        .collect();
    let mut samples = embed_sample_ranges(&audio, embedder, &local_turns)?;
    for sample in &mut samples {
        sample.start += source.offset_s as f32;
        sample.end += source.offset_s as f32;
        // `ops::name_speaker` refuses them if the meeting is reanalysed.
        sample.namespace = Some(m.speaker_namespace.clone());
    }
    Ok(samples)
}

/// One embedding per range of the file at `path` (seconds); ranges shorter
/// than 1.5 s or too short for the model are skipped. Used to enroll the
/// user's voice from a recording (`setup --enroll-me`).
pub fn embed_file_ranges(
    decoders: &DecoderChain,
    embedder: &dyn SpeakerEmbedder,
    path: &Path,
    ranges: &[(f64, f64)],
) -> Result<Vec<ConfirmationSample>> {
    let ranges: Vec<(f64, f64)> = ranges
        .iter()
        .copied()
        .filter(|(a, b)| *a >= 0.0 && b - a >= MIN_CLEAN_S)
        .collect();
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    let audio = decode_16k(decoders, path)?;
    embed_sample_ranges(&audio.samples, embedder, &ranges)
}

fn to_samples(embedded: Vec<((f64, f64), Vec<f32>)>) -> Vec<ConfirmationSample> {
    embedded
        .into_iter()
        .map(|((start, end), vector)| ConfirmationSample {
            vector,
            start: start as f32,
            end: end as f32,
            namespace: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{ImportOptions, import_file};
    use crate::testutil::{Fixture, models};
    use broscribe_core::schema::RecordingState;

    #[test]
    fn samples_come_from_stored_turns_with_their_ranges() {
        let fx = Fixture::new();
        let file = fx.wav("c.wav", &[(Some(0), 4.0), (Some(1), 4.0), (Some(0), 2.0)]);
        let mut m = models();
        let out = import_file(&fx.env(), &mut m, &file, &ImportOptions::default()).unwrap();
        let s1 = SpeakerId::new(1).unwrap();
        let samples =
            confirmation_samples(&fx.lib, &fx.chain, m.embedder.as_ref(), &out.meeting_id, s1)
                .unwrap();
        assert_eq!(samples.len(), 1);
        assert!(samples[0].start >= 0.0 && samples[0].end > samples[0].start + 1.5);
        let meeting = fx.lib.load_meeting(&out.meeting_id).unwrap();
        let centroid = meeting.speaker(s1).unwrap().centroid.as_ref().unwrap();
        let cos = broscribe_core::identity::cosine(&samples[0].vector, &centroid.vector().unwrap());
        assert!(cos > 0.9999, "{cos}");

        // Without the recording: nothing (name_speaker then uses the centroid).
        fx.lib
            .write(|tx| {
                tx.update_meeting(&out.meeting_id, |m| {
                    m.recording.as_mut().unwrap().state = RecordingState::Deleted;
                    Ok(())
                })
                .map(|_| ())
            })
            .unwrap();
        let none =
            confirmation_samples(&fx.lib, &fx.chain, m.embedder.as_ref(), &out.meeting_id, s1)
                .unwrap();
        assert!(none.is_empty());
    }

    /// One speaker across two VAD regions with times that do not survive
    /// `f32`: renaming it leaves the first person nothing from the meeting,
    /// for recorded and for legacy (speaker-less) confirmation samples.
    #[test]
    fn renaming_a_speaker_across_vad_regions_moves_every_confirmation_sample() {
        for legacy in [false, true] {
            let fx = Fixture::new();
            let file = fx.wav(
                "gap.wav",
                &[(Some(0), 2.2), (None, 1.3), (Some(0), 2.2), (Some(1), 4.0)],
            );
            let mut m = models();
            let out = import_file(&fx.env(), &mut m, &file, &ImportOptions::default()).unwrap();
            let meeting = fx.lib.load_meeting(&out.meeting_id).unwrap();
            let s1 = meeting.segments[0].speaker.unwrap();
            let own: Vec<_> = meeting
                .segments
                .iter()
                .filter(|s| s.speaker == Some(s1))
                .collect();
            assert!(
                own.windows(2).any(|w| w[1].start > w[0].end),
                "speaker spans a silence: {own:?}"
            );
            let name = |person: &str| {
                let samples = confirmation_samples(
                    &fx.lib,
                    &fx.chain,
                    m.embedder.as_ref(),
                    &out.meeting_id,
                    s1,
                )
                .unwrap();
                assert!(!samples.is_empty());
                let model = m.embedder.model_id().to_owned();
                broscribe_core::ops::with_index(&fx.lib, |tx, idx| {
                    broscribe_core::ops::name_speaker(
                        tx,
                        idx,
                        &out.meeting_id,
                        s1,
                        person,
                        samples,
                        &model,
                    )
                })
                .unwrap()
                .person_id
            };
            let ana = name("Ana Example");
            if legacy {
                fx.lib
                    .write(|tx| {
                        let mut vps = tx.library().load_voiceprints(&ana)?;
                        for v in &mut vps.embeddings {
                            v.source.speaker_id = None;
                        }
                        tx.save_voiceprints(&vps)
                    })
                    .unwrap();
            }
            name("Bob Example");
            let left = fx.lib.load_voiceprints(&ana).unwrap().embeddings;
            assert!(
                left.iter()
                    .all(|v| v.source.meeting_id.as_ref() != Some(&out.meeting_id)),
                "legacy={legacy}: {left:?}"
            );
        }
    }

    #[test]
    fn enrollment_chunks_bridge_split_and_drop() {
        // 0-2 and 2.5-12 bridge into 0-12 → two 6 s pieces; 20-22 is too short.
        let chunks = enrollment_chunks(&[(2.5, 12.0), (0.0, 2.0), (20.0, 22.0)]);
        assert_eq!(chunks, [(0.0, 6.0), (6.0, 12.0)]);
        assert_eq!(enrollment_chunks(&[(1.0, 4.5)]), [(1.0, 4.5)]);
        assert_eq!(enrollment_chunks(&[(0.0, 20.0)]).len(), 4);
    }

    #[test]
    fn audio_checks_flag_clipping_low_level_and_noise() {
        let mut loud = broscribe_models::fakes::sine(200.0, 12.0, 1.0);
        loud.extend(vec![0.0; 16_000]);
        let c = audio_checks(&loud, &[(0.0, 12.0)]);
        assert!(c.warnings.contains(&"clipping"), "{c:?}");
        assert!(!c.warnings.contains(&"short_speech"));
        assert_eq!(c.noise_rms_dbfs, None);
        assert!(c.noise_reference_s > 0.0);
        let json = serde_json::to_value(&c).unwrap();
        assert!(json["noise_rms_dbfs"].is_null());
        assert!(json["snr_db"].is_null());

        let quiet = broscribe_models::fakes::sine(200.0, 4.0, 0.001);
        let c = audio_checks(&quiet, &[(0.0, 4.0)]);
        assert!(c.warnings.contains(&"low_level"));
        assert!(c.warnings.contains(&"short_speech"));

        let mut noisy = broscribe_models::fakes::sine(200.0, 11.0, 0.1);
        noisy.extend(broscribe_models::fakes::sine(3000.0, 2.0, 0.08));
        let c = audio_checks(&noisy, &[(0.0, 11.0)]);
        assert!(c.snr_db.unwrap() < MIN_SNR_DB, "{c:?}");
        assert_eq!(c.warnings, ["noisy"]);
    }

    #[test]
    fn missed_speech_edges_do_not_turn_a_quiet_pause_into_noise() {
        let mut clean = broscribe_models::fakes::sine(200.0, 12.0, 0.25);
        clean.extend(std::iter::repeat_n(0.0, 12_000));
        // The detector trims 600 ms of real speech at either edge.
        // The remaining quiet reference must not be dominated by those edges.
        let checks = audio_checks(&clean, &[(0.6, 11.4)]);
        assert!(!checks.warnings.contains(&"noisy"), "{checks:?}");
        assert!(checks.noise_reference_s >= MIN_NOISE_S);
        assert_eq!(checks.noise_rms_dbfs, None);
        assert_eq!(checks.snr_db, None);
        assert!((checks.speech_s - 10.8).abs() < 0.001);
    }

    #[test]
    fn fragmented_and_speech_adjacent_gaps_are_not_noise_references() {
        let speech = broscribe_models::fakes::sine(200.0, 14.0, 0.25);
        let fragmented: Vec<_> = (0..13)
            .map(|i| {
                let start = f64::from(i) * 1.1;
                (start, start + 1.0)
            })
            .collect();
        for ranges in [fragmented, vec![(0.0, 6.0), (6.8, 14.0)], vec![(0.0, 14.0)]] {
            let checks = audio_checks(&speech, &ranges);
            assert_eq!(checks.noise_reference_s, 0.0, "{checks:?}");
            assert_eq!(checks.noise_rms_dbfs, None);
            assert_eq!(checks.snr_db, None);
            assert!(!checks.warnings.contains(&"noisy"), "{checks:?}");
        }
    }

    #[test]
    fn pause_noise_estimate_still_detects_stationary_background_noise() {
        let rate = TARGET_RATE as usize;
        for (noise_rms, expected_noisy) in [(0.01_f32, false), (0.18_f32, true)] {
            let mut seed = 0x9e37_79b9_u32;
            let noise_peak = noise_rms * 3.0_f32.sqrt();
            let step = std::f32::consts::TAU * 200.0 / rate as f32;
            let samples: Vec<_> = (0..14 * rate)
                .map(|i| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let noise = ((seed >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0) * noise_peak;
                    let voice = if (rate..13 * rate).contains(&i) {
                        0.25 * (step * i as f32).sin()
                    } else {
                        0.0
                    };
                    voice + noise
                })
                .collect();
            let checks = audio_checks(&samples, &[(1.0, 13.0)]);
            assert!(checks.noise_reference_s > 1.0, "{checks:?}");
            assert_eq!(
                checks.warnings.contains(&"noisy"),
                expected_noisy,
                "{checks:?}"
            );
            let contrast = checks.snr_db.unwrap();
            if expected_noisy {
                assert!((contrast - 3.0).abs() < 1.0, "{checks:?}");
            } else {
                assert!(contrast > 20.0, "{checks:?}");
            }
            assert!(!checks.warnings.contains(&"clipping"));
            assert!(!checks.warnings.contains(&"low_level"));
        }
    }

    #[test]
    fn reference_level_uses_the_exact_even_frame_median() {
        let mut samples = Vec::new();
        for i in 0..26 {
            let level = ((i * 7) % 26) as f32 / 50.0;
            samples.extend(std::iter::repeat_n(level, NOISE_FRAME_SAMPLES));
        }
        let checks = audio_checks(&samples, &[]);
        // The two middle frame powers are 0.24² and 0.26².
        let expected_dbfs = 10.0 * 0.0626_f64.log10();
        assert!((checks.noise_rms_dbfs.unwrap() - expected_dbfs).abs() < 0.0001);
        assert!((checks.noise_reference_s - 0.52).abs() < 0.0001);
    }

    #[test]
    fn noise_guards_preserve_overlapping_speech_and_clipping_measurements() {
        let mut samples = broscribe_models::fakes::sine(200.0, 12.0, 1.0);
        samples.extend(std::iter::repeat_n(0.0, 16_000));
        let reference = audio_checks(&samples, &[(0.0, 12.0)]);
        let overlapping = audio_checks(
            &samples,
            &[
                (5.0, 8.0),
                (0.0, 6.0),
                (2.0, 12.0),
                (-2.0, 0.0),
                (12.0, 12.0),
                (14.0, 13.0),
                (10.0, 5.0),
            ],
        );
        assert_eq!(reference, overlapping);
        assert!(reference.warnings.contains(&"clipping"));
    }

    #[test]
    fn phrase_check_relative_and_calibrated() {
        let a = vec![1.0, 0.0, 0.0];
        let chunks = vec![a.clone(), a.clone(), vec![0.99, 0.1, 0.0], a.clone()];
        let same = phrase_check(&a, &chunks, None);
        assert!(same.passed && same.method == "relative", "{same:?}");
        let other = phrase_check(&[0.0, 1.0, 0.0], &chunks, None);
        assert!(!other.passed);
        let calibrated = phrase_check(&[0.0, 1.0, 0.0], &chunks, Some(-1.0));
        assert!(calibrated.passed && calibrated.method == "calibrated");
    }
}

#[cfg(test)]
mod bounded_confirmation_tests {
    use super::*;
    use crate::import::{ImportOptions, import_file};
    use crate::testutil::{Fixture, models, write_wav};
    use broscribe_models::fakes::ToneEmbedder;

    #[test]
    fn long_confirmation_ranges_keep_identity_and_complete_ordered_coverage() {
        let fx = Fixture::new();
        let file = fx.wav("long.wav", &[(Some(0), 12.0)]);
        let mut set = models();
        let out = import_file(&fx.env(), &mut set, &file, &ImportOptions::default()).unwrap();
        let mut meeting = fx.lib.load_meeting(&out.meeting_id).unwrap();
        // Explicitly select the full known synthetic speaker run, rather
        // than relying on the fake ASR's last-word timestamp.
        meeting.segments.last_mut().unwrap().end = 12.0;
        fx.lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        let speaker = &meeting.speakers[0];
        let expected = speaker.centroid.as_ref().unwrap().vector().unwrap();
        let mut bounded = ToneEmbedder::new(set.embedder.model_id(), set.embedder.dim());
        bounded.max_samples = Some(2 * TARGET_RATE as usize);
        let samples =
            confirmation_samples(&fx.lib, &fx.chain, &bounded, &meeting.id, speaker.id).unwrap();
        assert!(samples.len() >= 6);
        assert_eq!(samples.first().unwrap().start, 0.0);
        assert_eq!(samples.last().unwrap().end, 12.0);
        assert!(samples.windows(2).all(|pair| pair[0].end == pair[1].start));
        for sample in samples {
            assert!(sample.end - sample.start <= 2.0);
            assert!(broscribe_core::identity::cosine(&sample.vector, &expected) > 0.99);
        }
    }

    struct AcceptsShort;
    impl SpeakerEmbedder for AcceptsShort {
        fn model_id(&self) -> &str {
            "synthetic-short-capable"
        }
        fn dim(&self) -> usize {
            2
        }
        fn embed(&self, input: &[f32]) -> Result<Option<Vec<f32>>> {
            Ok((!input.is_empty()).then(|| vec![1.0, 0.0]))
        }
    }

    #[test]
    fn truncated_audio_cannot_confirm_a_metadata_only_long_turn() {
        let fx = Fixture::new();
        let file = fx.wav("truncated.wav", &[(Some(0), 5.0)]);
        let out = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
        let meeting = fx.lib.load_meeting(&out.meeting_id).unwrap();
        let recording = meeting.recording.as_ref().unwrap();
        let path = fx
            .lib
            .meeting_dir(&meeting.id)
            .unwrap()
            .join(&recording.path);
        write_wav(&path, TARGET_RATE, &vec![0.2; 800]);
        let samples = confirmation_samples(
            &fx.lib,
            &fx.chain,
            &AcceptsShort,
            &meeting.id,
            meeting.speakers[0].id,
        )
        .unwrap();
        assert!(
            samples.is_empty(),
            "50 ms cannot become a confirmed voice sample"
        );
    }
}
