//! The recording's final setup (design §5). Admission never infers in person
//! from the absence of a meeting app, so a manual start is classified as
//! remote; the recording itself settles whether there was a remote meeting.
use broscribe_capture::journal::SessionMeta;
use broscribe_core::{Error, Result};
use broscribe_dsp::{Setup, VadConfig, energy_vad, frame_energies, frame_len};

/// The system track carried speech only if one run of activity lasted this
/// many seconds: short sounds (notifications), each lengthened by the VAD's
/// hangover, never add up to remote speech.
const SYSTEM_SPEECH_MIN_S: f64 = 2.0;

/// The admitted setup, or the in-person alternative admission offered when no
/// meeting app held the mic during the recording and the system track (the
/// remote side) carried no continuous speech. An explicit setup offers no
/// alternative, so it always wins.
pub(crate) fn resolve_final_setup(meta: &SessionMeta, system: &[f32], rate: u32) -> Result<Setup> {
    let admitted = meta
        .setup
        .as_deref()
        .unwrap_or("speakers_mic")
        .parse::<Setup>()
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
    let Some(in_person) = meta.in_person_setup.as_deref() else {
        return Ok(admitted);
    };
    if meta.meeting_app_seen || longest_system_run_s(system, rate) >= SYSTEM_SPEECH_MIN_S {
        return Ok(admitted);
    }
    match in_person.parse::<Setup>() {
        Ok(setup) => Ok(setup),
        Err(error) => {
            tracing::warn!(%error, "ignoring an invalid in-person setup");
            Ok(admitted)
        }
    }
}

/// Seconds of the longest continuous energy activity on the system track.
fn longest_system_run_s(system: &[f32], rate: u32) -> f64 {
    let frame = frame_len(rate);
    let (mut longest, mut run) = (0usize, 0usize);
    for active in energy_vad(&frame_energies(system, frame), &VadConfig::default()) {
        run = if active { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    (longest * frame) as f64 / f64::from(rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    fn meta(setup: &str, in_person: Option<&str>, app_seen: bool) -> SessionMeta {
        SessionMeta {
            setup: Some(setup.into()),
            in_person_setup: in_person.map(Into::into),
            meeting_app_seen: app_seen,
            ..Default::default()
        }
    }

    fn silence(seconds: f64) -> Vec<f32> {
        vec![0.0; (seconds * f64::from(RATE)) as usize]
    }

    /// Silence with `talk_s` seconds of a synthetic voiced tone in the middle.
    fn remote_talk(seconds: f64, talk_s: f64) -> Vec<f32> {
        let mut track = silence(seconds);
        tone(&mut track, 1.0, talk_s);
        track
    }

    /// A synthetic voiced tone over `from_s..from_s + len_s` of `track`.
    fn tone(track: &mut [f32], from_s: f64, len_s: f64) {
        let from = (from_s * f64::from(RATE)) as usize;
        let to = from + (len_s * f64::from(RATE)) as usize;
        for (i, sample) in track[from..to].iter_mut().enumerate() {
            let t = i as f32 / RATE as f32;
            *sample = 0.3 * (2.0 * std::f32::consts::PI * 220.0 * t).sin();
        }
    }

    fn resolve(meta: &SessionMeta, system: &[f32]) -> String {
        resolve_final_setup(meta, system, RATE).unwrap().to_string()
    }

    #[test]
    fn record_manual_start_without_app_or_remote_speech_is_in_person() {
        let meta = meta("speakers_mic", Some("in_person"), false);
        assert_eq!(resolve(&meta, &silence(30.0)), "in_person");
        // A short system sound (a notification) is not remote speech.
        assert_eq!(resolve(&meta, &remote_talk(30.0, 0.5)), "in_person");
    }

    #[test]
    fn record_short_system_sounds_never_add_up_to_remote_speech() {
        let meta = meta("speakers_mic", Some("in_person"), false);
        // Ten 300 ms notification blips 1.3 s apart: 3 s of sound, 5 s of
        // activity with the VAD's hangover, but no continuous speech.
        let mut blips = silence(30.0);
        for i in 0..10 {
            tone(&mut blips, 2.0 + f64::from(i) * 1.3, 0.3);
        }
        assert_eq!(resolve(&meta, &blips), "in_person");
        // One continuous run of 2 s or more is the remote side talking.
        assert_eq!(resolve(&meta, &remote_talk(30.0, 2.5)), "speakers_mic");
    }

    #[test]
    fn record_remote_evidence_keeps_the_admitted_setup() {
        // The meeting app took the mic after the start (joined late), or
        // Teams held it with detection off.
        let seen = meta("speakers_mic", Some("in_person"), true);
        assert_eq!(resolve(&seen, &silence(30.0)), "speakers_mic");
        // A headphones pair with the remote side talking.
        let headphones = meta("headphones", Some("in_person"), false);
        assert_eq!(resolve(&headphones, &remote_talk(30.0, 10.0)), "headphones");
        assert_eq!(resolve(&headphones, &silence(30.0)), "in_person");
    }

    #[test]
    fn record_continuously_audible_remote_side_keeps_the_admitted_setup() {
        // The whole system track is audible (nonstop talk or music): with no
        // quiet frames its level is not its own noise floor.
        let meta = meta("speakers_mic", Some("in_person"), false);
        let mut nonstop = silence(30.0);
        tone(&mut nonstop, 0.0, 30.0);
        assert_eq!(resolve(&meta, &nonstop), "speakers_mic");
    }

    #[test]
    fn record_explicit_or_confirmed_setup_always_wins() {
        // No alternative offered: explicit `--setup`, a detected app, or a
        // daemon without a detector.
        assert_eq!(
            resolve(&meta("headphones", None, false), &silence(30.0)),
            "headphones"
        );
        // An unreadable alternative keeps the admitted setup.
        assert_eq!(
            resolve(&meta("headphones", Some("bogus"), false), &silence(30.0)),
            "headphones"
        );
        assert!(resolve_final_setup(&meta("bogus", None, false), &[], RATE).is_err());
    }
}
