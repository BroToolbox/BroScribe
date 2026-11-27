//! Echo-duplicate dropping (plan §12.7): never drop the user's speech.
//!
//! A mic segment is dropped only when it is an echo duplicate: its text and
//! timing match the aligned system-track transcript while the system track had
//! speech, AND its voice fails the Me match (or scores higher for a remote
//! speaker). No dropping when Me is not enrolled, in the `headphones` setup,
//! in room setups, when the system had no speech within ±`proximity_s`, or
//! for segments shorter than `min_duration_s`. Other mic speech that fails the
//! Me match (non-room setups) becomes "Unknown (mic)" for the review queue.

use serde::{Deserialize, Serialize};

use crate::setup::Setup;

/// A mic-track transcript segment with its voice scores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicSegment {
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
    /// Similarity to the Me voiceprint; `None` when no embedding was taken.
    pub me_score: Option<f32>,
    /// Best similarity to the remote speakers active at that time.
    pub best_remote_score: Option<f32>,
}

/// A system-track transcript segment (mic timeline, after alignment).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemSegment {
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
}

/// Decision for one mic segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicDecision {
    /// Me (or, in room setups, whoever diarization finds).
    Keep,
    /// Echo duplicate of system-track speech.
    DropEcho,
    /// Not the user and not an echo duplicate: "Unknown (mic)", review queue.
    UnknownMic,
}

/// Rule thresholds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuplicateConfig {
    /// Calibrated Me match threshold (from the voice-identity settings).
    pub me_threshold: f32,
    /// System speech must be within this distance of the mic segment.
    pub proximity_s: f64,
    /// Shorter mic segments are always kept.
    pub min_duration_s: f64,
    /// Fraction of mic words found in nearby system text.
    pub min_text_match: f32,
    /// Fraction of the mic segment covered by (padded) system speech.
    pub min_time_overlap: f64,
}

impl DuplicateConfig {
    /// Plan §12.7 defaults around the caller's calibrated Me threshold.
    pub fn new(me_threshold: f32) -> Self {
        Self {
            me_threshold,
            proximity_s: 0.5,
            min_duration_s: 1.5,
            min_text_match: 0.6,
            min_time_overlap: 0.5,
        }
    }
}

/// Classifies every mic segment (same order as `mic`).
pub fn classify_mic_segments(
    mic: &[MicSegment],
    system: &[SystemSegment],
    me_enrolled: bool,
    setup: Setup,
    cfg: &DuplicateConfig,
) -> Vec<MicDecision> {
    mic.iter()
        .map(|seg| classify(seg, system, me_enrolled, setup, cfg))
        .collect()
}

fn classify(
    seg: &MicSegment,
    system: &[SystemSegment],
    me_enrolled: bool,
    setup: Setup,
    cfg: &DuplicateConfig,
) -> MicDecision {
    if !me_enrolled || setup.is_room() || seg.end_s - seg.start_s < cfg.min_duration_s {
        return MicDecision::Keep;
    }
    let Some(me) = seg.me_score else {
        return MicDecision::Keep;
    };
    let fails_me = me < cfg.me_threshold || seg.best_remote_score.is_some_and(|r| r > me);
    if !fails_me {
        return MicDecision::Keep;
    }
    if setup != Setup::Headphones && is_duplicate(seg, system, cfg) {
        MicDecision::DropEcho
    } else {
        MicDecision::UnknownMic
    }
}

/// Text and timing match against system speech near the segment.
fn is_duplicate(seg: &MicSegment, system: &[SystemSegment], cfg: &DuplicateConfig) -> bool {
    let lo = seg.start_s - cfg.proximity_s;
    let hi = seg.end_s + cfg.proximity_s;
    let near: Vec<&SystemSegment> = system
        .iter()
        .filter(|s| s.end_s >= lo && s.start_s <= hi)
        .collect();
    if near.is_empty() {
        return false;
    }
    // Timing: mic duration covered by the union of padded system segments.
    let mut spans: Vec<(f64, f64)> = near
        .iter()
        .map(|s| {
            let a = (s.start_s - cfg.proximity_s).max(seg.start_s);
            let b = (s.end_s + cfg.proximity_s).min(seg.end_s);
            (a, b)
        })
        .filter(|(a, b)| b > a)
        .collect();
    spans.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut covered = 0.0;
    let mut cur: Option<(f64, f64)> = None;
    for (a, b) in spans {
        match cur {
            Some((ca, cb)) if a <= cb => cur = Some((ca, cb.max(b))),
            Some((ca, cb)) => {
                covered += cb - ca;
                cur = Some((a, b));
            }
            None => cur = Some((a, b)),
        }
    }
    if let Some((ca, cb)) = cur {
        covered += cb - ca;
    }
    let duration = seg.end_s - seg.start_s;
    if duration <= 0.0 || covered / duration < cfg.min_time_overlap {
        return false;
    }
    // Text: share of mic words present in the nearby system text (multiset).
    let mic_words = words(&seg.text);
    if mic_words.is_empty() {
        return false;
    }
    let mut pool: Vec<String> = near.iter().flat_map(|s| words(&s.text)).collect();
    let mut hits = 0usize;
    for w in &mic_words {
        if let Some(i) = pool.iter().position(|p| p == w) {
            pool.swap_remove(i);
            hits += 1;
        }
    }
    hits as f32 / mic_words.len() as f32 >= cfg.min_text_match
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: f32 = 0.6;

    fn mic(start: f64, end: f64, text: &str, me: f32, remote: Option<f32>) -> MicSegment {
        MicSegment {
            start_s: start,
            end_s: end,
            text: text.into(),
            me_score: Some(me),
            best_remote_score: remote,
        }
    }

    fn sys(start: f64, end: f64, text: &str) -> SystemSegment {
        SystemSegment {
            start_s: start,
            end_s: end,
            text: text.into(),
        }
    }

    fn run(
        m: &[MicSegment],
        s: &[SystemSegment],
        enrolled: bool,
        setup: Setup,
    ) -> Vec<MicDecision> {
        classify_mic_segments(m, s, enrolled, setup, &DuplicateConfig::new(THRESHOLD))
    }

    fn system() -> Vec<SystemSegment> {
        vec![sys(10.0, 14.0, "the quarterly numbers look fine to me")]
    }

    #[test]
    fn echo_duplicate_is_dropped() {
        let m = [mic(
            10.1,
            14.1,
            "The quarterly numbers look fine",
            0.2,
            Some(0.7),
        )];
        assert_eq!(
            run(&m, &system(), true, Setup::SpeakersMic),
            [MicDecision::DropEcho]
        );
    }

    #[test]
    fn me_matching_speech_is_kept_even_when_text_matches() {
        let m = [mic(
            10.1,
            14.1,
            "the quarterly numbers look fine",
            0.9,
            Some(0.3),
        )];
        assert_eq!(
            run(&m, &system(), true, Setup::SpeakersMic),
            [MicDecision::Keep]
        );
    }

    #[test]
    fn remote_scoring_higher_than_me_counts_as_failing_me() {
        let m = [mic(
            10.1,
            14.1,
            "the quarterly numbers look fine",
            0.65,
            Some(0.8),
        )];
        assert_eq!(
            run(&m, &system(), true, Setup::SpeakersMic),
            [MicDecision::DropEcho]
        );
    }

    #[test]
    fn no_dropping_without_enrollment_short_segments_or_headphones() {
        let m = [mic(
            10.1,
            14.1,
            "the quarterly numbers look fine",
            0.2,
            None,
        )];
        assert_eq!(
            run(&m, &system(), false, Setup::SpeakersMic),
            [MicDecision::Keep]
        );
        let short = [mic(10.1, 11.4, "the quarterly numbers", 0.2, None)];
        assert_eq!(
            run(&short, &system(), true, Setup::SpeakersMic),
            [MicDecision::Keep]
        );
        assert_eq!(
            run(&m, &system(), true, Setup::Headphones),
            [MicDecision::UnknownMic]
        );
    }

    #[test]
    fn no_system_speech_nearby_means_unknown_not_dropped() {
        let m = [mic(
            20.0,
            24.0,
            "the quarterly numbers look fine",
            0.2,
            None,
        )];
        assert_eq!(
            run(&m, &system(), true, Setup::SpeakersMic),
            [MicDecision::UnknownMic]
        );
        // 0.6 s gap: outside ±0.5 s.
        let m = [mic(
            14.6,
            18.0,
            "the quarterly numbers look fine",
            0.2,
            None,
        )];
        assert_eq!(
            run(&m, &system(), true, Setup::SpeakersMic),
            [MicDecision::UnknownMic]
        );
    }

    #[test]
    fn different_text_during_system_speech_is_unknown() {
        let m = [mic(
            10.1,
            14.1,
            "can someone share the slides please",
            0.2,
            None,
        )];
        assert_eq!(
            run(&m, &system(), true, Setup::SpeakersMic),
            [MicDecision::UnknownMic]
        );
    }

    #[test]
    fn room_setups_never_drop_or_relabel() {
        let m = [mic(
            10.1,
            14.1,
            "the quarterly numbers look fine",
            0.2,
            Some(0.7),
        )];
        assert_eq!(
            run(&m, &system(), true, Setup::HybridRoom),
            [MicDecision::Keep]
        );
        assert_eq!(
            run(&m, &system(), true, Setup::InPerson),
            [MicDecision::Keep]
        );
    }

    #[test]
    fn missing_embedding_is_kept() {
        let mut m = mic(10.1, 14.1, "the quarterly numbers look fine", 0.0, None);
        m.me_score = None;
        assert_eq!(
            run(&[m], &system(), true, Setup::SpeakersMic),
            [MicDecision::Keep]
        );
    }
}
