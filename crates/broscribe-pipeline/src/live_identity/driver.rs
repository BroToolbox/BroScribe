//! The live-speakers worker's loop body, independent of processes and pipes.

use std::path::PathBuf;
use std::time::Instant;

use broscribe_capture::journal::{self, SessionState};
use broscribe_core::{Error, PersonId, Result};
use broscribe_ipc::Track;
use broscribe_ipc::live::{
    LiveIdentitySettings, SpeakerWindow, SpeakersEvent, SpeakersInput, SpeakersStart,
};

use super::tail::TrackTail;
use super::tracker::{
    Evidence, EvidenceSource, SessionPool, SpeakerModels, VoiceTracker, session_sample,
};
use super::{capture_track, seconds};
use broscribe_capture::anchors::placements;

struct TrackState {
    tail: TrackTail,
    tracker: VoiceTracker,
    /// End of the last analysed window (live samples); `None` before the
    /// first audio.
    analysed_to: Option<u64>,
    /// Older audio was skipped since the last window.
    behind: bool,
}

/// What one [`Driver::step`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Step {
    pub windows: usize,
    /// Closed audio not analysed yet, all tracks (seconds).
    pub pending_s: f64,
    /// The session stopped and everything durable was analysed, or its
    /// journal is gone: the worker should exit.
    pub finished: bool,
}

/// One session's live speaker analysis: polls the journal, analyses a
/// window per hop per track and keeps at most one window and one hop of
/// backlog (older audio is skipped with a `gap`).
pub struct Driver {
    journal_dir: PathBuf,
    epoch: u64,
    settings: LiveIdentitySettings,
    tracks: [TrackState; 2],
    pool: SessionPool,
    fingerprint: Option<String>,
}

impl Driver {
    /// Refuses settings `LiveIdentitySettings::validate` refuses, among them
    /// a hop below `broscribe_ipc::live::MIN_HOP_S` (it would analyse the
    /// same window forever, or thousands of windows per poll).
    pub fn new(start: &SpeakersStart) -> Result<Self> {
        start.settings.validate().map_err(Error::InvalidInput)?;
        let cap = LiveIdentitySettings::samples(start.settings.retain_s) as usize;
        let track = |t: Track| TrackState {
            tail: TrackTail::new(capture_track(t), cap),
            tracker: VoiceTracker::new(),
            analysed_to: None,
            behind: false,
        };
        Ok(Self {
            journal_dir: start.journal_dir.clone(),
            epoch: start.epoch,
            settings: start.settings.clone(),
            tracks: [track(Track::Mic), track(Track::System)],
            pool: SessionPool::new(start.settings.max_session_samples, start.settings.max_turns),
            fingerprint: None,
        })
    }

    /// Samples retained per track now (bounded by `retain_s`).
    pub fn retained(&self) -> [usize; 2] {
        [
            self.tracks[0].tail.retained(),
            self.tracks[1].tail.retained(),
        ]
    }

    /// Session samples held now.
    #[cfg(test)]
    pub(crate) fn session_samples(&self) -> usize {
        self.pool.len()
    }

    /// Applies one stdin message. Per turn, only a message from a newer
    /// decision than the last one applied counts: a delayed Name never
    /// overrides the Clear that followed it. A sample then counts only if
    /// the forget fence its decision was committed under is still the
    /// person's current one: a Name from before any later forget never
    /// counts, whenever it arrives and whatever the clocks say; a Name
    /// committed after the forget carries the new fence and does.
    ///
    /// `busy` brackets the evidence load and the sample's embedding (as
    /// it brackets each window in [`Self::step`]), so a hang there shows
    /// in the worker's heartbeat like a hung window.
    pub fn input(
        &mut self,
        input: SpeakersInput,
        models: &SpeakerModels<'_>,
        evidence: &mut dyn EvidenceSource,
        busy: &dyn Fn(bool),
        emit: &mut dyn FnMut(SpeakersEvent) -> Result<()>,
    ) -> Result<()> {
        match input {
            SpeakersInput::SessionSample {
                turn_id,
                decision,
                forget_fence,
                track,
                from_sample,
                to_sample,
                person_id,
            } => {
                if !self.pool.admit(&turn_id, decision) {
                    return emit(SpeakersEvent::SessionSample {
                        turn_id,
                        used: false,
                    });
                }
                // This decision replaces whatever the turn held before.
                self.pool.revoke(&turn_id);
                busy(true);
                let vector = self.session_vector(
                    forget_fence,
                    &person_id,
                    track,
                    (from_sample, to_sample),
                    models,
                    evidence,
                );
                busy(false);
                let used = vector.is_some();
                if let Some(((person, fence), v)) = vector {
                    self.pool.add(&turn_id, person, v, fence);
                }
                emit(SpeakersEvent::SessionSample { turn_id, used })
            }
            SpeakersInput::RevokeSessionSample { turn_id, decision } => {
                if self.pool.admit(&turn_id, decision) {
                    self.pool.revoke(&turn_id);
                }
                Ok(())
            }
        }
    }

    /// The session sample a Name allows: its person and fence, and the
    /// embedding of the turn's clean speech in `[from, to)`. `None` without
    /// a fence (unknown), with unreadable evidence, a person the evidence
    /// no longer allows, audio no longer retained, or no clean speech.
    fn session_vector(
        &mut self,
        forget_fence: Option<String>,
        person_id: &str,
        track: Track,
        (from, to): (u64, u64),
        models: &SpeakerModels<'_>,
        evidence: &mut dyn EvidenceSource,
    ) -> Option<((PersonId, String), Vec<f32>)> {
        let allowed = match (forget_fence, PersonId::parse(person_id), evidence.current()) {
            (Some(fence), Ok(person), Ok(ev)) => {
                self.refresh(&ev);
                SessionPool::allowed(&ev, &person, &fence).then_some((person, fence))
            }
            // No fence (unknown) or unreadable evidence: no sample rather
            // than one that may outlive a forget.
            _ => None,
        }?;
        let state = &self.tracks[track.index()];
        let audio = state.tail.slice(from, to)?;
        session_sample(&state.tracker, audio, from, to, models.embedder)
            .ok()
            .flatten()
            .map(|v| (allowed, v))
    }

    /// Adopts newly loaded evidence: session samples it no longer allows
    /// (person deleted, voice forgotten) are dropped.
    fn refresh(&mut self, ev: &Evidence) {
        if self.fingerprint.as_deref() != Some(ev.fingerprint.as_str()) {
            self.pool.prune(ev);
            self.fingerprint = Some(ev.fingerprint.clone());
        }
    }

    /// Reads newly closed chunks and analyses every due window.
    pub fn step(
        &mut self,
        models: &SpeakerModels<'_>,
        evidence: &mut dyn EvidenceSource,
        busy: &dyn Fn(bool),
        emit: &mut dyn FnMut(SpeakersEvent) -> Result<()>,
    ) -> Result<Step> {
        let manifest = match journal::load_manifest(&self.journal_dir) {
            Ok(m) => m,
            // Published or discarded: nothing left to preview.
            Err(Error::NotFound(_)) => {
                return Ok(Step {
                    finished: true,
                    ..Step::default()
                });
            }
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Step {
                    finished: true,
                    ..Step::default()
                });
            }
            Err(e) => return Err(e),
        };
        let stopped = manifest.state == SessionState::Stopped;
        let window = LiveIdentitySettings::samples(self.settings.window_s);
        let hop = LiveIdentitySettings::samples(self.settings.hop_s);
        let mut step = Step::default();
        let mut all_done = true;
        for track in Track::ALL {
            let all = placements(&manifest, capture_track(track));
            let closed_end = all.last().map(|p| p.end());
            let state = &mut self.tracks[track.index()];
            if state.analysed_to.is_none() {
                state.analysed_to = all.first().map(|p| p.start);
            }
            // Behind by more than a window and a hop: skip to the last
            // window, report the rest as a preview gap.
            let mut skip_before = 0;
            if let (Some(end), Some(done)) = (closed_end, state.analysed_to)
                && end > done + window + hop
            {
                skip_before = end - window;
                emit(SpeakersEvent::Gap {
                    track,
                    from_sample: done,
                    to_sample: skip_before,
                })?;
                state.analysed_to = Some(end - hop);
                state.behind = true;
            }
            let polled = state.tail.poll(&self.journal_dir, &all, skip_before)?;
            if polled.missing > 0 {
                emit(SpeakersEvent::Warning {
                    message: format!("{} closed journal chunk(s) were unreadable", polled.missing),
                })?;
            }
            let caught_up = state.tail.caught_up(&all);
            loop {
                let state = &mut self.tracks[track.index()];
                let done = state.analysed_to.unwrap_or(state.tail.start());
                let tail_end = state.tail.end();
                let end = if tail_end >= done + hop {
                    done + hop
                } else if stopped && caught_up && tail_end > done {
                    tail_end
                } else {
                    break;
                };
                let from = end.saturating_sub(window).max(state.tail.start());
                if end <= from {
                    state.analysed_to = Some(end);
                    continue;
                }
                let started = Instant::now();
                busy(true);
                let current = evidence.current();
                let outcome = current.and_then(|ev| {
                    self.refresh(&ev);
                    let state = &mut self.tracks[track.index()];
                    let audio = state
                        .tail
                        .slice(from, end)
                        .ok_or_else(|| Error::Internal("live window not retained".into()))?;
                    let analysed = state.tracker.analyse(
                        from,
                        audio,
                        models,
                        &ev,
                        &self.pool,
                        &self.settings,
                    )?;
                    Ok((analysed, ev.fingerprint.clone()))
                });
                busy(false);
                let state = &mut self.tracks[track.index()];
                state.analysed_to = Some(end);
                match outcome {
                    Ok((mut analysed, fingerprint)) => {
                        for span in &mut analysed.spans {
                            span.from = state.tail.point(span.from_sample);
                            span.to = state.tail.point(span.to_sample);
                        }
                        emit(SpeakersEvent::Window(SpeakerWindow {
                            epoch: self.epoch,
                            track,
                            from_sample: from,
                            to_sample: end,
                            spans: analysed.spans,
                            voices: analysed.voices,
                            evidence: fingerprint,
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            behind: std::mem::take(&mut state.behind),
                        }))?;
                        step.windows += 1;
                    }
                    // One failed window is a preview gap, not a crash.
                    Err(e) => {
                        emit(SpeakersEvent::Warning {
                            message: format!("speaker analysis window failed ({})", e.code()),
                        })?;
                        emit(SpeakersEvent::Gap {
                            track,
                            from_sample: from,
                            to_sample: end,
                        })?;
                    }
                }
            }
            let state = &self.tracks[track.index()];
            let analysed = state.analysed_to.unwrap_or(0);
            if let Some(end) = closed_end {
                step.pending_s += seconds(end.saturating_sub(analysed));
            }
            if !(caught_up && state.tail.end() <= analysed.max(state.tail.start())) {
                all_done = false;
            }
        }
        step.finished = stopped && all_done;
        Ok(step)
    }
}
