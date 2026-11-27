//! The queue's identity reevaluation job ([`REIDENTIFY_KIND`], design §6.4):
//! after confirmed voice evidence changed, every eligible speaker is
//! reassessed with the same per-meeting planner and publication policy as
//! `identity re-resolve` ([`crate::reresolve`]).
//!
//! One run claims the job of one model and works in short library writes,
//! one meeting each, reading and updating the job's input through its lease
//! ([`LeaseHandle::update_in`]) in the same write, so requests made between
//! writes are never overwritten:
//!
//! 1. An interrupted publication ([`ReidentifyInput::in_flight`]) is
//!    recovered first, before anything may end the job (a model switch
//!    included): the changes that reached `meeting.json` get their index
//!    rows and weak samples repaired and are counted once
//!    ([`reresolve::repair_publication`]).
//! 2. A pass starts (or restarts) whenever the request generation is ahead
//!    of the pass or the model's thresholds changed. Its source meetings go
//!    first, then every meeting ID in sorted order (the cursor), then the
//!    pass is complete. A model switch ends the job: its work is moot.
//! 3. Only `done` meetings are reassessed; others resolve with the current
//!    evidence when their own processing finishes. A `done` meeting another
//!    job works on (confirmation samples, search-vector refreshes and
//!    playback caches aside: they never write the meeting) is deferred and
//!    retried after the pass, then every [`DEFER_RETRY`] until free;
//!    reached again any other way while free, it waits no more. An
//!    unreadable meeting in a normal visit is reported by ID and error
//!    code and skipped. An unreadable interrupted publication keeps its
//!    repair checkpoint and fails for retry rather than losing unfinished
//!    repairs. Every committed change shows the generation it answers
//!    before the pass ends.
//! 4. After [`MEETINGS_PER_RUN`] meetings the job goes back to the end of
//!    the queue, so jobs queued meanwhile run first.
//!
//! Voice samples are read into memory once per run and checked against the
//! library under every write ([`crate::reresolve::Evidence`]); nothing of
//! them is stored in the job. The run's own weak samples never trigger
//! another request or teach the rest of the run.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use broscribe_core::config::Config;
use broscribe_core::index::Index;
pub use broscribe_core::learning::REIDENTIFY_KIND;
use broscribe_core::learning::{InFlight, LearningReport, Pass, ReidentifyInput};
use broscribe_core::ops::SpeakerChange;
use broscribe_core::queue::{LeaseHandle, Next, Queue};
use broscribe_core::{Error, Library, MeetingId, Result, WriteTx};
use chrono::TimeDelta;

use crate::reresolve::{self, Evidence, Repaired, Visit};

/// Meetings one run visits before the job yields to the rest of the queue.
pub const MEETINGS_PER_RUN: usize = 40;
/// How long a job that only waits for busy meetings waits before retrying.
pub const DEFER_RETRY: Duration = Duration::from_secs(60);

/// What one run did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOutcome {
    /// Counts only: no names or text.
    pub message: String,
    /// Meetings whose identities this run changed or repaired.
    pub changed: Vec<MeetingId>,
    /// The job was put back in the queue to continue at once.
    pub continues: bool,
}

/// Runs the claimed reevaluation job of `lease` for at most `budget`
/// meetings. `data_dir` holds the configuration, read in every write (the
/// active model and thresholds as they are now). `progress` receives a
/// counts-only line after each meeting that changed.
pub fn run(
    lib: &Library,
    lease: &LeaseHandle,
    data_dir: &Path,
    budget: usize,
    progress: &mut dyn FnMut(&str),
) -> Result<RunOutcome> {
    let mut state = Run {
        lib,
        ids: lib.list_meeting_ids()?,
        visited: 0,
        budget: budget.max(1),
        evidence: None,
        meetings: None,
        tried_deferred: HashSet::new(),
        changed: Vec::new(),
    };
    loop {
        match lib.write(|tx| state.step(tx, lease, data_dir))? {
            Step::Continue(None) => {}
            Step::Continue(Some(line)) => progress(&line),
            Step::Stop { message, continues } => {
                state.changed.sort();
                state.changed.dedup();
                return Ok(RunOutcome {
                    message,
                    changed: state.changed,
                    continues,
                });
            }
        }
    }
}

enum Step {
    /// One write done; the line, when any, reports what it changed.
    Continue(Option<String>),
    Stop {
        message: String,
        continues: bool,
    },
}

/// Where the meeting a step visits came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Source,
    Sweep,
    Deferred,
}

struct Run<'l> {
    lib: &'l Library,
    /// Every meeting ID when the run started, sorted (the sweep order).
    ids: Vec<MeetingId>,
    visited: usize,
    budget: usize,
    /// The model's samples, read at the run's first visit of a pass.
    evidence: Option<Evidence>,
    meetings: Option<reresolve::Meetings<'l>>,
    /// Deferred meetings this run already retried (or deferred).
    tried_deferred: HashSet<MeetingId>,
    changed: Vec<MeetingId>,
}

impl Run<'_> {
    /// One short write: reads the job's input, does one thing and saves it.
    fn step(&mut self, tx: &mut WriteTx<'_>, lease: &LeaseHandle, data_dir: &Path) -> Result<Step> {
        let lib = tx.library().clone();
        let job = Queue::open(tx.root())?.get(lease.id())?;
        let mut input = ReidentifyInput::of(&job).ok_or_else(|| {
            Error::InvalidInput(format!("job {} has an invalid reidentify input", job.id))
        })?;
        // The generation the work committed now answers: the pass's, or
        // the last complete one's for deferred retries after it.
        let generation = input
            .pass
            .as_ref()
            .map_or(input.processed, |p| p.generation);

        // 1. An interrupted publication, before anything may end the job:
        // its changes may already be in `meeting.json`, and a model switch
        // makes further work moot, never the repair of what was written.
        if let Some(flight) = input.in_flight.take() {
            match reresolve::repair_publication(tx, &lib, &flight)? {
                Repaired::Applied(applied) => {
                    if !applied.is_empty() {
                        self.changed.push(flight.meeting_id.clone());
                        if let Some(evidence) = &mut self.evidence {
                            evidence.own_write(&lib)?;
                        }
                    }
                    commit(
                        &mut input,
                        generation,
                        &flight.meeting_id,
                        flight.source,
                        &applied,
                    );
                }
                // Keep the stored checkpoint until repair is possible.
                // Replanning alone cannot recover samples/index changes
                // that already reached meeting.json before the crash.
                Repaired::Unreadable(e) => {
                    return Err(Error::Inference(format!(
                        "identity repair pending for {} ({})",
                        flight.meeting_id,
                        e.code()
                    )));
                }
            }
            save(tx, lease, None, &input)?;
            return Ok(Step::Continue(None));
        }

        let config = Config::load(data_dir)?;
        if input.model != config.profile.speaker_embedding {
            input.pass = None;
            input.source_meetings.clear();
            input.deferred.clear();
            input.processed = input.requested;
            save(tx, lease, Some(Next::Done), &input)?;
            return Ok(Step::Stop {
                message: "skipped: the active speaker-embedding model changed".into(),
                continues: false,
            });
        }
        let thresholds = config.thresholds(&input.model).copied();

        // 2. A pass for the latest request and thresholds.
        let stale = input
            .pass
            .as_ref()
            .is_some_and(|p| p.generation < input.requested || p.thresholds != thresholds);
        if stale || (input.pass.is_none() && input.processed < input.requested) {
            input.pass = Some(Pass::new(input.requested, thresholds));
            // The new sweep visits them again.
            input.deferred.clear();
            report(&mut input);
            // A fresh snapshot for the new pass.
            self.evidence = None;
            self.meetings = None;
            self.tried_deferred.clear();
            save(tx, lease, None, &input)?;
            return Ok(Step::Continue(None));
        }

        // 3. The meeting to visit, or the end of the pass or job.
        let first_source = input.source_meetings.first().cloned();
        let next_deferred = input
            .deferred
            .iter()
            .find(|m| !self.tried_deferred.contains(*m))
            .cloned();
        let (meeting, origin) = match (&mut input.pass, first_source) {
            (Some(_), Some(m)) => (m, Origin::Source),
            (Some(pass), None) if !pass.swept => {
                let next = match &pass.cursor {
                    Some(c) => self.ids.iter().find(|id| *id > c),
                    None => self.ids.first(),
                }
                .cloned();
                match next {
                    Some(id) => (id, Origin::Sweep),
                    None => {
                        pass.swept = true;
                        save(tx, lease, None, &input)?;
                        return Ok(Step::Continue(None));
                    }
                }
            }
            (Some(pass), None) => {
                let generation = pass.generation;
                input.pass = None;
                input.processed = generation;
                report(&mut input).generation = generation;
                // Deferred during this pass: retried after a wait.
                self.tried_deferred.extend(input.deferred.iter().cloned());
                if input.requested > input.processed {
                    save(tx, lease, None, &input)?;
                    return Ok(Step::Continue(None));
                }
                return self.finish(tx, lease, &mut input);
            }
            (None, _) => match next_deferred {
                Some(m) => (m, Origin::Deferred),
                None => return self.finish(tx, lease, &mut input),
            },
        };
        if self.visited >= self.budget {
            save(tx, lease, Some(Next::Requeue { not_before: None }), &input)?;
            return Ok(Step::Stop {
                message: format!(
                    "{} meeting(s) checked; continuing after other jobs",
                    self.visited
                ),
                continues: true,
            });
        }
        self.visited += 1;

        // 4. Visit it.
        let loaded = match self.evidence.take() {
            Some(e) => e,
            None => Evidence::load(&lib, &input.model)?,
        };
        let evidence = self.evidence.insert(loaded);
        if self.meetings.is_none() {
            self.meetings = Some(reresolve::Meetings::list(self.lib)?);
        }
        let mut line = None;
        let mut busy = false;
        match reresolve::visit(
            &lib,
            self.meetings.as_ref().expect("loaded above"),
            &meeting,
            evidence,
            thresholds.as_ref(),
            &config.live,
        )? {
            Visit::Skipped | Visit::Unchanged => report(&mut input).mark_readable(&meeting),
            Visit::Busy => {
                report(&mut input).mark_readable(&meeting);
                busy = true;
            }
            // Reported and skipped: one damaged meeting never stops the
            // meetings after it.
            Visit::Unreadable(e) => unreadable(&mut input, &meeting, &e),
            Visit::Planned(plan) => {
                let source = origin == Origin::Source;
                let changes = plan.changes.clone();
                // Recorded before any file is written (see `recover`).
                input.in_flight = Some(InFlight {
                    meeting_id: meeting.clone(),
                    source,
                    changes: changes.clone(),
                });
                save(tx, lease, None, &input)?;
                let mut index = Index::open(tx.root())?;
                reresolve::publish(tx, &mut index, *plan)?;
                evidence.own_write(&lib)?;
                input.in_flight = None;
                commit(&mut input, generation, &meeting, source, &changes);
                line = Some(report(&mut input).summary());
                self.changed.push(meeting.clone());
            }
        }
        match origin {
            Origin::Source => input.source_meetings.retain(|m| *m != meeting),
            Origin::Sweep => {
                if let Some(pass) = &mut input.pass {
                    pass.cursor = Some(meeting.clone());
                }
            }
            Origin::Deferred => {
                self.tried_deferred.insert(meeting.clone());
            }
        }
        // Whichever way it was reached, a meeting no longer busy waits no
        // more.
        if busy {
            if !input.deferred.contains(&meeting) {
                input.deferred.push(meeting);
            }
        } else {
            input.deferred.retain(|m| *m != meeting);
        }
        let waiting = input.deferred.len();
        report(&mut input).deferred = waiting;
        save(tx, lease, None, &input)?;
        Ok(Step::Continue(line))
    }

    /// Nothing left this run: done, or back in the queue after
    /// [`DEFER_RETRY`] while deferred meetings wait.
    fn finish(
        &mut self,
        tx: &mut WriteTx<'_>,
        lease: &LeaseHandle,
        input: &mut ReidentifyInput,
    ) -> Result<Step> {
        let waiting = input.deferred.len();
        let r = report(input);
        r.deferred = waiting;
        let message = r.summary();
        let next = if waiting == 0 {
            Next::Done
        } else {
            let wait = TimeDelta::from_std(DEFER_RETRY).unwrap_or(TimeDelta::MAX);
            Next::Requeue {
                not_before: Some(broscribe_core::schema::now() + wait),
            }
        };
        save(tx, lease, Some(next), input)?;
        Ok(Step::Stop {
            message,
            continues: false,
        })
    }
}

/// The input's report, created when missing.
fn report(input: &mut ReidentifyInput) -> &mut LearningReport {
    input.report.get_or_insert_with(LearningReport::default)
}

/// Folds a meeting's committed `changes` into the report, which then shows
/// `generation` (the request generation they answer) as soon as they are
/// written, not only when the whole pass ends: readers waiting for that
/// request see partial results.
fn commit(
    input: &mut ReidentifyInput,
    generation: u64,
    meeting: &MeetingId,
    source: bool,
    changes: &[SpeakerChange],
) {
    let r = report(input);
    r.mark_readable(meeting);
    r.record(meeting, source, changes);
    if !changes.is_empty() {
        r.generation = r.generation.max(generation);
    }
}

/// Reports `meeting` as unreadable by its error code only: the error's
/// text may name a meeting folder (its title), which the job never stores.
fn unreadable(input: &mut ReidentifyInput, meeting: &MeetingId, e: &Error) {
    tracing::warn!(meeting = %meeting, code = e.code(), "reevaluation skipped an unreadable meeting");
    report(input).mark_unreadable(meeting, e.code());
}

/// Writes `input` through the lease in this write. Requests cannot write
/// between this step's read and now (they need the library lock this write
/// holds); a stored generation ahead of `input` is still kept, never
/// overwritten.
fn save(
    tx: &mut WriteTx<'_>,
    lease: &LeaseHandle,
    next: Option<Next>,
    input: &ReidentifyInput,
) -> Result<()> {
    lease.update_in(tx, next, |stored| {
        let mut out = input.clone();
        if let Some(current) = ReidentifyInput::of_value(stored)
            && current.requested > out.requested
        {
            out.requested = current.requested;
            for m in current.source_meetings {
                if !out.source_meetings.contains(&m) {
                    out.source_meetings.push(m);
                }
            }
        }
        *stored = out.to_value()?;
        Ok(())
    })
}

#[cfg(test)]
mod tests;
