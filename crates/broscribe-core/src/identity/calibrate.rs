//! Threshold calibration (design §8.3, plan §13.7): date splits, fitting
//! `merge`, `auto_name`, `margin` and `suggest`, and the frozen evaluation.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, FixedOffset};
use serde::Serialize;

use super::score::NameKind;
use crate::config::Thresholds;
use crate::ids::{MeetingId, PersonId};

/// Share of the date-sorted hinted-other meetings used for enrollment.
const ENROLL_SHARE: f64 = 0.6;
/// Share used for calibration; the rest is evaluated.
const CALIBRATE_SHARE: f64 = 0.2;
/// When no setting names anyone without a wrong name, at most this share of
/// auto-named talk time may be wrong.
const WRONG_TOLERANCE: f64 = 0.02;
/// Talk-weighted precision a top-1 suggestion needs.
const SUGGEST_PRECISION: f64 = 0.8;

/// A manifest meeting as seen by [`split_by_date`].
#[derive(Clone, Debug, PartialEq)]
pub struct CalMeeting {
    pub id: MeetingId,
    pub started_at: DateTime<FixedOffset>,
    /// Hints for people other than the user.
    pub has_other_hints: bool,
    /// Hints for the user ("Me").
    pub has_self_hints: bool,
}

/// Disjoint meeting sets, each in `(started_at, id)` order.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Splits {
    pub enroll: Vec<MeetingId>,
    pub calibrate: Vec<MeetingId>,
    pub evaluate: Vec<MeetingId>,
}

/// Splits meetings by date (plan §13.7). Meetings with hints for other people
/// are sorted by `(started_at, id)`; the first round(0.6N) enroll, the next
/// round(0.2N) calibrate, the rest evaluate. With T1/T2 the start times of the
/// first calibration/evaluation meetings, a self-only meeting evaluates when
/// it starts at or after T2, calibrates at or after T1, else enrolls (all
/// enroll when there are no later sets). Meetings without hints are skipped.
pub fn split_by_date(meetings: &[CalMeeting]) -> Splits {
    let key = |m: &&CalMeeting| (m.started_at, m.id.clone());
    let mut others: Vec<&CalMeeting> = meetings.iter().filter(|m| m.has_other_hints).collect();
    others.sort_by_key(key);
    let n = others.len();
    let n_enroll = ((ENROLL_SHARE * n as f64).round() as usize).min(n);
    let n_cal = ((CALIBRATE_SHARE * n as f64).round() as usize).min(n - n_enroll);
    let t1 = others.get(n_enroll).map(|m| m.started_at);
    let t2 = others.get(n_enroll + n_cal).map(|m| m.started_at);

    let mut enroll: Vec<&CalMeeting> = others[..n_enroll].to_vec();
    let mut calibrate: Vec<&CalMeeting> = others[n_enroll..n_enroll + n_cal].to_vec();
    let mut evaluate: Vec<&CalMeeting> = others[n_enroll + n_cal..].to_vec();
    for m in meetings
        .iter()
        .filter(|m| m.has_self_hints && !m.has_other_hints)
    {
        if t2.is_some_and(|t| m.started_at >= t) {
            evaluate.push(m);
        } else if t1.is_some_and(|t| m.started_at >= t) {
            calibrate.push(m);
        } else {
            enroll.push(m);
        }
    }
    let ids = |mut set: Vec<&CalMeeting>| -> Vec<MeetingId> {
        set.sort_by_key(key);
        set.into_iter().map(|m| m.id.clone()).collect()
    };
    Splits {
        enroll: ids(enroll),
        calibrate: ids(calibrate),
        evaluate: ids(evaluate),
    }
}

/// Fits `merge` from within-meeting cluster pairs `(cosine, same person)`:
/// the smallest threshold with zero wrong merges, i.e. just above the highest
/// different-person cosine (the lowest same-person cosine when there are no
/// different-person pairs). `None` when no same-person pair lies above it.
pub fn fit_merge(pairs: &[(f32, bool)]) -> Option<f32> {
    let max_different = pairs
        .iter()
        .filter(|p| !p.1)
        .map(|p| p.0)
        .max_by(f32::total_cmp);
    let lowest_same_above = pairs
        .iter()
        .filter(|p| p.1 && max_different.is_none_or(|d| p.0 > d))
        .map(|p| p.0)
        .min_by(f32::total_cmp)?;
    Some(max_different.map_or(lowest_same_above, f32::next_up))
}

/// One calibration speaker: its scores against every enrolled candidate and
/// the hinted truth (`None` = impostor: a person who was not enrolled, or a
/// leave-one-out trial).
#[derive(Clone, Debug, PartialEq)]
pub struct Trial {
    pub scores: Vec<(PersonId, f32)>,
    pub truth: Option<PersonId>,
    pub talk_s: f64,
}

/// A trial's top-1 candidate.
struct Top {
    score: f32,
    /// Lead over the runner-up; +∞ without one.
    lead: f32,
    correct: bool,
    impostor: bool,
    talk_s: f64,
}

#[derive(Clone, Copy)]
struct Candidate {
    auto_name: f32,
    margin: f32,
    correct_s: f64,
    wrong_s: f64,
}

impl Candidate {
    /// More correct talk; then fewer wrong; then the more conservative
    /// setting (higher `auto_name`, then higher `margin`).
    fn better_than(&self, other: &Candidate) -> bool {
        self.correct_s
            .total_cmp(&other.correct_s)
            .then_with(|| other.wrong_s.total_cmp(&self.wrong_s))
            .then_with(|| self.auto_name.total_cmp(&other.auto_name))
            .then_with(|| self.margin.total_cmp(&other.margin))
            .is_gt()
    }
}

/// Fits `auto_name`, `margin` and `suggest` (`merge` is left `None`; see
/// [`fit_merge`]). Returns `None` when no trial has an enrolled truth (the
/// guard: nothing is written).
///
/// - `auto_name`/`margin`: on the grid of observed top-1 scores × observed
///   leads (plus 0), a trial is auto-named when its top score ≥ `auto_name`
///   and its lead ≥ `margin`. The fit minimises wrong-name talk time first
///   (target 0; when no zero-wrong setting names anyone correctly, at most 2%
///   of auto-named talk time), then maximises correctly auto-named talk time;
///   ties go to the more conservative setting. Both stay `None` when no
///   setting qualifies.
/// - `suggest`: evaluated only at thresholds at or above the impostor floor
///   (just above the best impostor's top score): the lowest one at and above
///   which top-1 suggestions are correct for ≥ 80% of talk time. When none
///   qualifies, a value above every observed score (suggest nothing). It is
///   independent of `auto_name`: resolution checks auto-naming first.
pub fn fit_thresholds(trials: &[Trial]) -> Option<Thresholds> {
    if !trials.iter().any(|t| t.truth.is_some()) {
        return None;
    }
    let tops: Vec<Top> = trials
        .iter()
        .filter_map(|t| {
            // NaN scores are no evidence (and would stall the threshold sweeps).
            let mut s: Vec<&(PersonId, f32)> = t.scores.iter().filter(|c| !c.1.is_nan()).collect();
            s.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let (best, score) = s.first().map(|&(p, sc)| (p, *sc))?;
            Some(Top {
                score,
                lead: s.get(1).map_or(f32::INFINITY, |r| score - r.1),
                correct: t.truth.as_ref() == Some(best),
                impostor: t.truth.is_none(),
                talk_s: t.talk_s.max(0.0),
            })
        })
        .collect();

    let (auto_name, margin) =
        fit_auto(&tops).map_or((None, None), |c| (Some(c.auto_name), Some(c.margin)));
    let suggest = fit_suggest(&tops);
    Some(Thresholds {
        auto_name,
        suggest,
        margin,
        merge: None,
        auto_merge: None,
    })
}

fn fit_auto(tops: &[Top]) -> Option<Candidate> {
    let mut margins: Vec<f32> = tops
        .iter()
        .map(|t| t.lead)
        .filter(|l| l.is_finite() && *l > 0.0)
        .collect();
    margins.push(0.0);
    margins.sort_by(f32::total_cmp);
    margins.dedup();

    let mut by_score: Vec<&Top> = tops.iter().collect();
    by_score.sort_by(|a, b| b.score.total_cmp(&a.score));

    let mut zero_wrong: Option<Candidate> = None;
    let mut tolerated: Option<Candidate> = None;
    for &margin in &margins {
        let (mut correct_s, mut wrong_s) = (0.0, 0.0);
        let mut i = 0;
        // Sweep auto_name down through the observed scores.
        while i < by_score.len() {
            let auto_name = by_score[i].score;
            while i < by_score.len() && by_score[i].score >= auto_name {
                let t = by_score[i];
                if t.lead >= margin {
                    if t.correct {
                        correct_s += t.talk_s;
                    } else {
                        wrong_s += t.talk_s;
                    }
                }
                i += 1;
            }
            if correct_s <= 0.0 {
                continue;
            }
            let c = Candidate {
                auto_name,
                margin,
                correct_s,
                wrong_s,
            };
            let slot = if wrong_s <= 0.0 {
                &mut zero_wrong
            } else if wrong_s <= WRONG_TOLERANCE * (correct_s + wrong_s) {
                &mut tolerated
            } else {
                continue;
            };
            if slot.as_ref().is_none_or(|b| c.better_than(b)) {
                *slot = Some(c);
            }
        }
    }
    zero_wrong.or(tolerated)
}

fn fit_suggest(tops: &[Top]) -> Option<f32> {
    let mut by_score: Vec<&Top> = tops.iter().collect();
    by_score.sort_by(|a, b| b.score.total_cmp(&a.score));
    let above_all = by_score.first()?.score.next_up();
    let floor = tops
        .iter()
        .filter(|t| t.impostor)
        .map(|t| t.score)
        .max_by(f32::total_cmp)
        .map(f32::next_up);
    let (mut correct_s, mut total_s) = (0.0, 0.0);
    // Lowest qualifying observed score at or above the floor, and whether an
    // observed (non-qualifying) score lies between the floor and it.
    let mut precise: Option<f32> = None;
    let mut below_precise = false;
    let mut i = 0;
    while i < by_score.len() {
        let s = by_score[i].score;
        if floor.is_some_and(|f| s < f) {
            break;
        }
        while i < by_score.len() && by_score[i].score >= s {
            total_s += by_score[i].talk_s;
            if by_score[i].correct {
                correct_s += by_score[i].talk_s;
            }
            i += 1;
        }
        if total_s > 0.0 && correct_s >= SUGGEST_PRECISION * total_s {
            precise = Some(s);
            below_precise = false;
        } else if precise.is_some() {
            below_precise = true;
        }
    }
    Some(match (precise, floor) {
        // The floor admits the same trials as the lowest qualifying score
        // when no observed score lies between them.
        (Some(p), Some(f)) if !below_precise => f.min(p),
        (Some(p), _) => p,
        // Nothing at or above the floor is precise enough: suggest nothing
        // that was observed.
        (None, _) => above_all,
    })
}

/// The hinted person of an evaluation interval.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "person_id", rename_all = "snake_case")]
pub enum PersonOrSelf {
    /// The user.
    Me(PersonId),
    Person(PersonId),
}

impl PersonOrSelf {
    pub fn person_id(&self) -> &PersonId {
        match self {
            PersonOrSelf::Me(p) | PersonOrSelf::Person(p) => p,
        }
    }
}

/// A stretch of hinted time in an evaluation meeting with one outcome:
/// the hinted person, whether they were enrolled from the enrollment set,
/// the diarized speaker's prediction there, and whether a diarized turn
/// covers it. Callers cut hinted time at speaker-turn boundaries, so a
/// cluster that mixes people yields one interval per hinted person.
#[derive(Clone, Debug, PartialEq)]
pub struct EvalInterval {
    pub hinted: PersonOrSelf,
    pub enrolled: bool,
    pub predicted: Option<(PersonId, NameKind)>,
    pub covered: bool,
    pub seconds: f64,
}

/// Seconds with the number of distinct people behind them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Tally {
    pub seconds: f64,
    pub people: usize,
}

/// Coverage of the user's own voice.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MeReport {
    pub hinted: Tally,
    /// Correct auto-named or correct top-suggestion seconds.
    pub covered_s: f64,
    /// Correct auto-named seconds.
    pub auto_s: f64,
    /// `covered_s / hinted.seconds`.
    pub coverage: Option<f64>,
    /// `auto_s / covered_s`.
    pub auto_share: Option<f64>,
}

/// People who were not enrolled: any automatic name for them is wrong.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UnenrolledReport {
    pub hinted: Tally,
    /// Covered, never auto-named people (their seconds: covered, not auto-named).
    pub asked_once: Tally,
    /// People auto-named at least once (their seconds: auto-named).
    pub wrongly_named: Tally,
    /// Covered seconds that carried a (necessarily wrong) suggestion.
    pub suggested_s: f64,
}

/// Evaluation per hinted second. Every hinted second is exactly one of
/// `correct_s` (auto-named as the hinted person), `wrong_s` (auto-named as
/// someone else), `unnamed_s` (covered but not auto-named) or `missed_s` (no
/// diarized turn).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct EvalReport {
    pub hinted: Tally,
    pub correct_s: f64,
    pub wrong_s: f64,
    pub unnamed_s: f64,
    pub missed_s: f64,
    /// Auto-named hinted time, enrolled or not.
    pub auto_named: Tally,
    /// `wrong_s / auto_named.seconds`.
    pub wrong_name_rate: Option<f64>,
    /// Hinted time of enrolled people (the user included when enrolled).
    pub enrolled: Tally,
    /// Correct auto-named or correct top-suggestion seconds of enrolled people.
    pub covered_s: f64,
    /// Correct auto-named seconds of enrolled people.
    pub auto_s: f64,
    /// `covered_s / enrolled.seconds`.
    pub coverage: Option<f64>,
    /// `auto_s / covered_s`.
    pub auto_share: Option<f64>,
    /// `None` without self hints in the evaluation set.
    pub me: Option<MeReport>,
    pub unenrolled: UnenrolledReport,
}

fn ratio(num: f64, den: f64) -> Option<f64> {
    (den > 0.0).then_some(num / den)
}

#[derive(Default)]
struct Acc {
    seconds: f64,
    people: BTreeSet<PersonId>,
}

impl Acc {
    fn add(&mut self, p: &PersonId, s: f64) {
        self.seconds += s;
        self.people.insert(p.clone());
    }

    fn tally(&self) -> Tally {
        Tally {
            seconds: self.seconds,
            people: self.people.len(),
        }
    }
}

/// Scores the evaluation set once with frozen thresholds (plan §13.7).
pub fn evaluate(intervals: &[EvalInterval]) -> EvalReport {
    let mut r = EvalReport::default();
    let (mut hinted, mut auto_named, mut enrolled) =
        (Acc::default(), Acc::default(), Acc::default());
    let (mut me, mut me_covered, mut me_auto) = (Acc::default(), 0.0, 0.0);
    let (mut unenrolled, mut wrongly) = (Acc::default(), Acc::default());
    // Unenrolled covered, not auto-named seconds per person.
    let mut asked: BTreeMap<PersonId, f64> = BTreeMap::new();

    for iv in intervals {
        let p = iv.hinted.person_id();
        let s = iv.seconds.max(0.0);
        hinted.add(p, s);
        if iv.enrolled {
            enrolled.add(p, s);
        } else {
            unenrolled.add(p, s);
        }
        let is_me = matches!(iv.hinted, PersonOrSelf::Me(_));
        if is_me {
            me.add(p, s);
        }
        if !iv.covered {
            r.missed_s += s;
            continue;
        }
        let (right_person, kind) = match &iv.predicted {
            Some((predicted, kind)) => (predicted == p, Some(*kind)),
            None => (false, None),
        };
        match kind {
            Some(NameKind::Auto) => {
                auto_named.add(p, s);
                if right_person {
                    r.correct_s += s;
                } else {
                    r.wrong_s += s;
                }
                if !iv.enrolled {
                    wrongly.add(p, s);
                }
            }
            Some(NameKind::Suggested) | None => {
                r.unnamed_s += s;
                if !iv.enrolled {
                    *asked.entry(p.clone()).or_default() += s;
                    if kind.is_some() {
                        r.unenrolled.suggested_s += s;
                    }
                }
            }
        }
        if right_person {
            let auto = kind == Some(NameKind::Auto);
            if iv.enrolled {
                r.covered_s += s;
                if auto {
                    r.auto_s += s;
                }
            }
            if is_me {
                me_covered += s;
                if auto {
                    me_auto += s;
                }
            }
        }
    }

    // A person auto-named even once counts as wrongly named, not asked once:
    // drop them and their seconds.
    asked.retain(|p, _| !wrongly.people.contains(p));

    r.hinted = hinted.tally();
    r.auto_named = auto_named.tally();
    r.wrong_name_rate = ratio(r.wrong_s, auto_named.seconds);
    r.enrolled = enrolled.tally();
    r.coverage = ratio(r.covered_s, enrolled.seconds);
    r.auto_share = ratio(r.auto_s, r.covered_s);
    r.me = (!me.people.is_empty()).then(|| MeReport {
        hinted: me.tally(),
        covered_s: me_covered,
        auto_s: me_auto,
        coverage: ratio(me_covered, me.seconds),
        auto_share: ratio(me_auto, me_covered),
    });
    r.unenrolled.hinted = unenrolled.tally();
    r.unenrolled.asked_once = Tally {
        seconds: asked.values().sum(),
        people: asked.len(),
    };
    r.unenrolled.wrongly_named = wrongly.tally();
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(hex: &str) -> PersonId {
        PersonId::parse(&format!("p-{hex}")).unwrap()
    }

    fn mid(n: u32) -> MeetingId {
        MeetingId::parse(&format!("m-{n:06x}")).unwrap()
    }

    fn at(day: u32, hour: u32) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(&format!("2026-01-{day:02}T{hour:02}:00:00+00:00")).unwrap()
    }

    fn meeting(n: u32, day: u32, hour: u32, other: bool, me: bool) -> CalMeeting {
        CalMeeting {
            id: mid(n),
            started_at: at(day, hour),
            has_other_hints: other,
            has_self_hints: me,
        }
    }

    #[test]
    fn split_by_date_places_self_only_meetings_by_cut_times() {
        // Ten hinted-other meetings on days 1..=10, given out of order.
        let mut meetings: Vec<CalMeeting> = (1..=10)
            .rev()
            .map(|d| meeting(d, d, 9, true, d % 2 == 0))
            .collect();
        meetings.push(meeting(101, 6, 18, false, true)); // before T1 (day 7) → enroll
        meetings.push(meeting(102, 7, 12, false, true)); // T1 ≤ t < T2 → calibrate
        meetings.push(meeting(103, 9, 9, false, true)); // exactly T2 → evaluate
        meetings.push(meeting(104, 20, 9, false, true)); // after everything → evaluate
        meetings.push(meeting(105, 3, 9, false, false)); // no hints → skipped
        let s = split_by_date(&meetings);
        assert_eq!(
            s.enroll,
            vec![mid(1), mid(2), mid(3), mid(4), mid(5), mid(6), mid(101)]
        );
        assert_eq!(s.calibrate, vec![mid(7), mid(102), mid(8)]);
        assert_eq!(s.evaluate, vec![mid(9), mid(103), mid(10), mid(104)]);
    }

    #[test]
    fn split_by_date_small_sets() {
        let s = split_by_date(&[meeting(1, 1, 9, true, false), meeting(2, 5, 9, false, true)]);
        assert_eq!(s.enroll, vec![mid(1), mid(2)]);
        assert!(s.calibrate.is_empty() && s.evaluate.is_empty());
        let s = split_by_date(&[meeting(2, 5, 9, false, true)]);
        assert_eq!(s.enroll, vec![mid(2)]);
        assert_eq!(split_by_date(&[]), Splits::default());
    }

    #[test]
    fn fit_merge_avoids_wrong_merges() {
        let pairs = [
            (0.9, true),
            (0.7, false),
            (0.75, true),
            (0.6, true),
            (0.3, false),
        ];
        assert_eq!(fit_merge(&pairs), Some(0.7f32.next_up()));
        assert_eq!(fit_merge(&[(0.8, true), (0.6, true)]), Some(0.6));
        assert_eq!(fit_merge(&[(0.5, true), (0.7, false)]), None);
        assert_eq!(fit_merge(&[]), None);
    }

    fn trial(scores: &[(&str, f32)], truth: Option<&str>, talk_s: f64) -> Trial {
        Trial {
            scores: scores.iter().map(|&(p, s)| (pid(p), s)).collect(),
            truth: truth.map(pid),
            talk_s,
        }
    }

    #[test]
    fn fit_guard_without_enrolled_truth() {
        let trials = [
            trial(&[("a00001", 0.9)], None, 10.0),
            trial(&[("b00002", 0.4)], None, 5.0),
        ];
        assert_eq!(fit_thresholds(&trials), None);
        assert_eq!(fit_thresholds(&[]), None);
    }

    #[test]
    fn fit_prefers_zero_wrong_names() {
        let (ana, bob) = ("a00001", "b00002");
        let trials = [
            trial(&[(ana, 0.9), (bob, 0.5)], Some(ana), 10.0),
            trial(&[(bob, 0.85), (ana, 0.4)], Some(bob), 10.0),
            trial(&[(ana, 0.8), (bob, 0.75)], None, 5.0),
            trial(&[(ana, 0.6), (bob, 0.55)], Some(ana), 3.0),
            trial(&[(bob, 0.5), (ana, 0.3)], None, 2.0),
        ];
        let t = fit_thresholds(&trials).unwrap();
        assert_eq!(t.auto_name, Some(0.85));
        assert_eq!(t.margin, Some(0.9 - 0.5));
        // 80% precision is reached at 0.6, floored just above the best impostor (0.8).
        assert_eq!(t.suggest, Some(0.8f32.next_up()));
        assert_eq!(t.merge, None);
    }

    #[test]
    fn fit_tolerates_two_percent_when_zero_wrong_is_impossible() {
        let (ana, bob) = ("a00001", "b00002");
        let trials = [
            trial(&[(ana, 0.95), (bob, 0.3)], None, 1.0),
            trial(&[(bob, 0.9), (ana, 0.4)], Some(bob), 100.0),
        ];
        let t = fit_thresholds(&trials).unwrap();
        assert_eq!(t.auto_name, Some(0.9));
        assert_eq!(t.margin, Some(0.9 - 0.4));

        let hopeless = [
            trial(&[(ana, 0.95)], None, 50.0),
            trial(&[(bob, 0.9)], Some(bob), 10.0),
        ];
        let t = fit_thresholds(&hopeless).unwrap();
        assert_eq!((t.auto_name, t.margin), (None, None));
    }

    #[test]
    fn evaluation_counts_mixed_cluster_per_second() {
        let (ana, bob, carl, dan, me) = (
            pid("a00001"),
            pid("b00002"),
            pid("c00003"),
            pid("d00004"),
            pid("e00005"),
        );
        let iv = |hinted: PersonOrSelf,
                  enrolled,
                  predicted: Option<(&PersonId, NameKind)>,
                  covered,
                  seconds| {
            EvalInterval {
                hinted,
                enrolled,
                predicted: predicted.map(|(p, k)| (p.clone(), k)),
                covered,
                seconds,
            }
        };
        use NameKind::{Auto, Suggested};
        use PersonOrSelf::{Me, Person};
        // One cluster auto-named Ana is mostly Ana (10 s) but holds 3 s of Bob.
        let intervals = [
            iv(Person(ana.clone()), true, Some((&ana, Auto)), true, 10.0),
            iv(Person(bob.clone()), true, Some((&ana, Auto)), true, 3.0),
            iv(
                Person(carl.clone()),
                false,
                Some((&bob, Suggested)),
                true,
                4.0,
            ),
            iv(Me(me.clone()), true, Some((&me, Suggested)), true, 5.0),
            iv(Person(dan.clone()), false, Some((&ana, Auto)), true, 2.0),
            iv(Person(ana.clone()), true, None, false, 1.0),
        ];
        let r = evaluate(&intervals);
        assert_eq!(
            r.hinted,
            Tally {
                seconds: 25.0,
                people: 5
            }
        );
        assert_eq!(
            (r.correct_s, r.wrong_s, r.unnamed_s, r.missed_s),
            (10.0, 5.0, 9.0, 1.0)
        );
        assert_eq!(
            r.auto_named,
            Tally {
                seconds: 15.0,
                people: 3
            }
        );
        assert_eq!(r.wrong_name_rate, Some(5.0 / 15.0));
        assert_eq!(
            r.enrolled,
            Tally {
                seconds: 19.0,
                people: 3
            }
        );
        assert_eq!((r.covered_s, r.auto_s), (15.0, 10.0));
        assert_eq!(r.coverage, Some(15.0 / 19.0));
        assert_eq!(r.auto_share, Some(10.0 / 15.0));
        let m = r.me.unwrap();
        assert_eq!(
            m.hinted,
            Tally {
                seconds: 5.0,
                people: 1
            }
        );
        assert_eq!(
            (m.covered_s, m.auto_s, m.coverage, m.auto_share),
            (5.0, 0.0, Some(1.0), Some(0.0))
        );
        assert_eq!(
            r.unenrolled.hinted,
            Tally {
                seconds: 6.0,
                people: 2
            }
        );
        assert_eq!(
            r.unenrolled.asked_once,
            Tally {
                seconds: 4.0,
                people: 1
            }
        );
        assert_eq!(
            r.unenrolled.wrongly_named,
            Tally {
                seconds: 2.0,
                people: 1
            }
        );
        assert_eq!(r.unenrolled.suggested_s, 4.0);
    }

    #[test]
    fn fit_suggest_is_not_capped_at_auto_name() {
        let (ana, bob) = ("a00001", "b00002");
        let trials = [
            // Correct at 0.7 with a 0.1 lead.
            trial(&[(ana, 0.7), (bob, 0.6)], Some(ana), 10.0),
            // A long impostor at 0.9 with a 0.01 lead.
            trial(&[(ana, 0.9), (bob, 0.89)], None, 100.0),
        ];
        let t = fit_thresholds(&trials).unwrap();
        assert_eq!(t.auto_name, Some(0.7));
        assert_eq!(t.margin, Some(0.7 - 0.6));
        assert!(t.suggest.unwrap() > 0.9, "{:?}", t.suggest);
    }

    #[test]
    fn fit_suggest_floor_precedes_precision() {
        let (ana, bob) = ("a00001", "b00002");
        let trials = [
            trial(&[(ana, 0.8)], Some(ana), 100.0),
            trial(&[(bob, 0.95)], Some(ana), 10.0),
            trial(&[(ana, 0.9)], None, 1.0),
        ];
        let t = fit_thresholds(&trials).unwrap();
        // Above the floor only the wrong 0.95 suggestion remains: suggest nothing.
        assert_eq!(t.suggest, Some(0.95f32.next_up()));
    }

    #[test]
    fn evaluation_wrongly_named_excludes_asked_once() {
        let (ana, dan) = (pid("a00001"), pid("d00004"));
        let iv = |predicted: Option<(PersonId, NameKind)>| EvalInterval {
            hinted: PersonOrSelf::Person(dan.clone()),
            enrolled: false,
            predicted,
            covered: true,
            seconds: 5.0,
        };
        let r = evaluate(&[iv(None), iv(Some((ana.clone(), NameKind::Auto)))]);
        assert_eq!(r.unenrolled.asked_once, Tally::default());
        assert_eq!(
            r.unenrolled.wrongly_named,
            Tally {
                seconds: 5.0,
                people: 1
            }
        );
    }
}
