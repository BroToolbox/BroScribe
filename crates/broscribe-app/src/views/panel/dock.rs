//! The "Speaking now" dock of the recording window (direction B, CLI-F14,
//! REC-F44, REC-F45): the pure model, tested without a window.
//!
//! - **Target.** The dock names the turn being spoken: the live turn that
//!   started last. *Who's this?* on an older turn retargets it to that turn
//!   until *Back to speaking now*, or until that turn leaves the preview.
//! - **People in this call.** The meeting app's participant list and the
//!   calendar invitees (observed or expected names, never confirmations),
//!   the people you confirmed, the people named live and the people whose
//!   stored voice is suggested on a turn, merged by person ID, e-mail or
//!   folded name. Your own entries are left out: your microphone is you.
//! - **Order.** First seen, first listed, for the whole recording, so the
//!   number keys **1–4** keep pointing at the same person while someone
//!   speaks.
//! - **Chip state.** "tap to confirm" when the target's suggestion is that
//!   person, else "named · N turns", "suggested · N turns", "known voice"
//!   (stored voice samples) or "not heard yet".
//! - **Tap.** Names the target turn, or confirms it when the target's
//!   suggestion is that person. Never a reject and never automatic: the
//!   only per-turn actions stay Name, Confirm and Clear (design.md, *Live
//!   speaker identity*).

use std::collections::BTreeSet;

use broscribe_core::names::folded_name;
use broscribe_ipc::live::{
    LiveAction, LivePerson, LiveTurn, LiveTurnStatus, SuggestionSource, Track,
};
use broscribe_readers::RosterMember;

use crate::ui::format::format_clock;

/// People reachable with a number key.
pub const DOCK_KEYS: usize = 4;

/// Where a person of the call comes from, in listing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChipSource {
    /// The meeting app's participant list (observed).
    Call,
    /// The calendar event's invitees (expected).
    Calendar,
    /// A participant you confirmed.
    Attended,
    /// Named by you on a live turn.
    NamedLive,
    /// Their stored voice is suggested on a turn.
    KnownVoice,
}

impl ChipSource {
    fn words(self) -> &'static str {
        match self {
            ChipSource::Call => "seen in the call",
            ChipSource::Calendar => "invited",
            ChipSource::Attended => "attended",
            ChipSource::NamedLive => "named live",
            ChipSource::KnownVoice => "voice suggested",
        }
    }
}

/// A registered person, as the dock needs them.
#[derive(Clone, Debug, PartialEq)]
pub struct KnownPerson {
    pub id: String,
    pub name: String,
    pub emails: Vec<String>,
    pub is_self: bool,
    /// Has stored voice samples for the active speaker model.
    pub has_voice: bool,
    /// Their colour (1 to the number of person hues).
    pub hue: u8,
}

/// What the dock is built from.
#[derive(Clone, Copy, Debug, Default)]
pub struct DockInputs<'a> {
    pub observed: &'a [RosterMember],
    pub expected: &'a [RosterMember],
    /// Confirmed participants: (person ID, name).
    pub confirmed: &'a [(String, String)],
    pub turns: &'a [LiveTurn],
    pub known: &'a [KnownPerson],
}

/// One person of the call.
#[derive(Clone, Debug, PartialEq)]
pub struct DockPerson {
    /// Stable identity for ordering: the folded name (the ID without one).
    pub key: String,
    pub name: String,
    pub person_id: Option<String>,
    pub email: Option<String>,
    pub sources: BTreeSet<ChipSource>,
    pub has_voice: bool,
    /// The registered person's colour; `None` for a name nobody has yet.
    pub hue: Option<u8>,
}

impl DockPerson {
    /// What `record.live_attribute` names: the ID, else the e-mail, else
    /// the plain name (a name-only person when nobody has it).
    pub fn person_ref(&self) -> String {
        self.person_id
            .clone()
            .or_else(|| self.email.clone())
            .unwrap_or_else(|| self.name.clone())
    }

    pub fn first_name(&self) -> &str {
        first_name(&self.name)
    }

    fn is(&self, person: &LivePerson) -> bool {
        match &self.person_id {
            Some(id) => *id == person.person_id,
            None => same_name(&self.name, &person.name),
        }
    }
}

/// The first word of a name; the part before `@` of an e-mail.
pub fn first_name(name: &str) -> &str {
    let name = name.trim();
    let name = name.split('@').next().unwrap_or(name);
    name.split_whitespace().next().unwrap_or(name)
}

fn same_name(a: &str, b: &str) -> bool {
    let a = folded_name(a);
    !a.is_empty() && a == folded_name(b)
}

/// The registered person a roster entry stands for: the same e-mail, else
/// the only person with that (folded) name.
fn resolve<'a>(member: &RosterMember, known: &'a [KnownPerson]) -> Option<&'a KnownPerson> {
    if let Some(email) = member.email.as_deref().filter(|e| !e.is_empty())
        && let Some(person) = known
            .iter()
            .find(|p| p.emails.iter().any(|e| e.eq_ignore_ascii_case(email)))
    {
        return Some(person);
    }
    let mut named = known.iter().filter(|p| same_name(&p.name, &member.name));
    match (named.next(), named.next()) {
        (Some(person), None) => Some(person),
        _ => None,
    }
}

struct Candidate {
    name: String,
    person_id: Option<String>,
    email: Option<String>,
    source: ChipSource,
    has_voice: bool,
    hue: Option<u8>,
}

fn merge(people: &mut Vec<DockPerson>, c: Candidate) {
    let found = people.iter_mut().find(|p| {
        (p.person_id.is_some() && p.person_id == c.person_id)
            || (p.email.is_some()
                && c.email
                    .as_deref()
                    .zip(p.email.as_deref())
                    .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b)))
            || ((p.person_id.is_none() || c.person_id.is_none()) && same_name(&p.name, &c.name))
    });
    match found {
        Some(p) => {
            p.sources.insert(c.source);
            p.has_voice |= c.has_voice;
            if p.person_id.is_none() {
                p.person_id = c.person_id;
                p.hue = c.hue;
            }
            if p.email.is_none() {
                p.email = c.email;
            }
        }
        None => {
            let key = Some(folded_name(&c.name))
                .filter(|k| !k.is_empty())
                .or_else(|| c.person_id.clone())
                .unwrap_or_default();
            if key.is_empty() {
                return;
            }
            people.push(DockPerson {
                key,
                name: c.name,
                person_id: c.person_id,
                email: c.email,
                sources: BTreeSet::from([c.source]),
                has_voice: c.has_voice,
                hue: c.hue,
            });
        }
    }
}

/// The turns in time order (the agent publishes the two tracks apart).
fn by_time(turns: &[LiveTurn]) -> Vec<&LiveTurn> {
    let mut sorted: Vec<&LiveTurn> = turns.iter().collect();
    sorted.sort_by(|a, b| a.start_s.total_cmp(&b.start_s));
    sorted
}

/// Everyone in the call, in source order, merged and without yourself.
pub fn people(inputs: DockInputs<'_>) -> Vec<DockPerson> {
    let known = |id: &str| inputs.known.iter().find(|p| p.id == id);
    let is_self = |id: &str| known(id).is_some_and(|p| p.is_self);
    let mut out = Vec::new();
    let roster = inputs
        .observed
        .iter()
        .map(|m| (m, ChipSource::Call))
        .chain(inputs.expected.iter().map(|m| (m, ChipSource::Calendar)));
    for (member, source) in roster {
        let person = resolve(member, inputs.known);
        if member.is_self || person.is_some_and(|p| p.is_self) || member.name.trim().is_empty() {
            continue;
        }
        merge(
            &mut out,
            Candidate {
                name: person.map_or_else(|| member.name.trim().to_owned(), |p| p.name.clone()),
                person_id: person.map(|p| p.id.clone()),
                email: member.email.clone().filter(|e| !e.is_empty()),
                source,
                has_voice: person.is_some_and(|p| p.has_voice),
                hue: person.map(|p| p.hue),
            },
        );
    }
    for (id, name) in inputs.confirmed {
        if is_self(id) {
            continue;
        }
        merge(
            &mut out,
            Candidate {
                name: name.clone(),
                person_id: Some(id.clone()),
                email: None,
                source: ChipSource::Attended,
                has_voice: known(id).is_some_and(|p| p.has_voice),
                hue: known(id).map(|p| p.hue),
            },
        );
    }
    // Names on microphone turns are you (an unregistered self included).
    let turns: Vec<&LiveTurn> = by_time(inputs.turns)
        .into_iter()
        .filter(|t| t.track == Track::System)
        .collect();
    let live = turns
        .iter()
        .filter_map(|t| t.named.as_ref().map(|p| (p, ChipSource::NamedLive)))
        .chain(turns.iter().filter_map(|t| {
            t.suggestion
                .as_ref()
                .filter(|s| {
                    t.status == LiveTurnStatus::Suggested && s.source == SuggestionSource::Library
                })
                .map(|s| (&s.person, ChipSource::KnownVoice))
        }));
    for (person, source) in live {
        if is_self(&person.person_id) {
            continue;
        }
        merge(
            &mut out,
            Candidate {
                name: person.name.clone(),
                person_id: Some(person.person_id.clone()),
                email: None,
                source,
                has_voice: source == ChipSource::KnownVoice
                    || known(&person.person_id).is_some_and(|p| p.has_voice),
                hue: known(&person.person_id).map(|p| p.hue),
            },
        );
    }
    out
}

/// Puts `people` in first-seen order: `order` remembers every key listed so
/// far in this recording and grows at the end only.
pub fn stable_order(order: &mut Vec<String>, mut people: Vec<DockPerson>) -> Vec<DockPerson> {
    for person in &people {
        if !order.contains(&person.key) {
            order.push(person.key.clone());
        }
    }
    people.sort_by_key(|p| order.iter().position(|k| *k == p.key));
    people
}

/// What the dock names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DockTarget {
    #[default]
    SpeakingNow,
    /// An older turn chosen with *Who's this?*.
    Turn(String),
}

/// The turn being spoken: the one that started last.
pub fn speaking_now(turns: &[LiveTurn]) -> Option<&LiveTurn> {
    turns
        .iter()
        .enumerate()
        .max_by(|(ia, a), (ib, b)| a.start_s.total_cmp(&b.start_s).then(ia.cmp(ib)))
        .map(|(_, t)| t)
}

/// The turn the dock acts on, and whether it is an older turn than the one
/// being spoken. A retarget whose turn left the preview, or that became the
/// turn being spoken, is the turn being spoken.
pub fn target_turn<'a>(target: &DockTarget, turns: &'a [LiveTurn]) -> (Option<&'a LiveTurn>, bool) {
    let now = speaking_now(turns);
    match target {
        DockTarget::Turn(id) => match turns.iter().find(|t| t.turn_id == *id) {
            Some(turn) if now.is_some_and(|n| n.turn_id != turn.turn_id) => (Some(turn), true),
            _ => (now, false),
        },
        DockTarget::SpeakingNow => (now, false),
    }
}

/// The dock's title line.
pub fn target_title(turn: Option<&LiveTurn>, retargeted: bool) -> String {
    match turn {
        Some(turn) if retargeted => format!("Naming the turn at {}", format_clock(turn.start_s)),
        _ => "Speaking now".to_owned(),
    }
}

/// Why the chips cannot name the target now, when they can't.
pub fn blocked(turn: Option<&LiveTurn>, paused: bool, connected: bool) -> Option<&'static str> {
    if !connected {
        return Some("Can’t reach BroScribe Agent. Names wait until it reconnects.");
    }
    match turn {
        None if paused => Some("Nothing is being said while paused."),
        None => Some("No live turn to name yet."),
        Some(turn) if turn.track == Track::Mic => {
            Some("This turn is from your microphone, so it’s you. Nothing to name.")
        }
        Some(_) => None,
    }
}

/// A chip's state in words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipState {
    /// The target turn's suggestion is this person.
    TapToConfirm,
    Named(usize),
    Suggested(usize),
    KnownVoice,
    NotHeard,
}

impl ChipState {
    pub fn words(self) -> String {
        let turns = |n: usize| {
            if n == 1 {
                "1 turn".to_owned()
            } else {
                format!("{n} turns")
            }
        };
        match self {
            ChipState::TapToConfirm => "tap to confirm".to_owned(),
            ChipState::Named(n) => format!("named · {}", turns(n)),
            ChipState::Suggested(n) => format!("suggested · {}", turns(n)),
            ChipState::KnownVoice => "known voice".to_owned(),
            ChipState::NotHeard => "not heard yet".to_owned(),
        }
    }

    /// Unconfirmed states are drawn dashed.
    pub fn unconfirmed(self) -> bool {
        matches!(self, ChipState::TapToConfirm | ChipState::Suggested(_))
    }
}

fn suggested_person(turn: &LiveTurn) -> Option<&LivePerson> {
    turn.suggestion
        .as_ref()
        .filter(|_| turn.status == LiveTurnStatus::Suggested)
        .map(|s| &s.person)
}

pub fn chip_state(person: &DockPerson, turns: &[LiveTurn], target: Option<&LiveTurn>) -> ChipState {
    if target
        .and_then(suggested_person)
        .is_some_and(|p| person.is(p))
    {
        return ChipState::TapToConfirm;
    }
    let named = turns
        .iter()
        .filter(|t| t.named.as_ref().is_some_and(|p| person.is(p)))
        .count();
    if named > 0 {
        return ChipState::Named(named);
    }
    let suggested = turns
        .iter()
        .filter(|t| suggested_person(t).is_some_and(|p| person.is(p)))
        .count();
    if suggested > 0 {
        ChipState::Suggested(suggested)
    } else if person.has_voice {
        ChipState::KnownVoice
    } else {
        ChipState::NotHeard
    }
}

/// One chip of the row.
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    pub person: DockPerson,
    pub state: ChipState,
    /// "1"–"4" for the first [`DOCK_KEYS`] people.
    pub key: Option<usize>,
}

impl Chip {
    /// The one line on the chip: first name and state.
    pub fn line(&self) -> String {
        format!("{} {}", self.person.first_name(), self.state.words())
    }

    /// The accessible name: the full name, its sources, its state, what a
    /// tap does and its key.
    pub fn spoken(&self, target: Option<&LiveTurn>) -> String {
        let sources: Vec<&str> = self.person.sources.iter().map(|s| s.words()).collect();
        let action = match (self.state, target) {
            (ChipState::TapToConfirm, Some(t)) => {
                format!(
                    "Confirms the suggestion on the turn at {}",
                    format_clock(t.start_s)
                )
            }
            (_, Some(t)) => format!("Names the turn at {}", format_clock(t.start_s)),
            (_, None) => "Names the turn being spoken".to_owned(),
        };
        let mut text = format!(
            "{}, {}, {}. {action}",
            self.person.name,
            sources.join(", "),
            self.state.words()
        );
        if let Some(key) = self.key {
            text.push_str(&format!(". Key {}", key + 1));
        }
        text
    }
}

/// The chips in order, with their keys.
pub fn chips(people: Vec<DockPerson>, turns: &[LiveTurn], target: Option<&LiveTurn>) -> Vec<Chip> {
    people
        .into_iter()
        .enumerate()
        .map(|(ix, person)| Chip {
            state: chip_state(&person, turns, target),
            key: (ix < DOCK_KEYS).then_some(ix),
            person,
        })
        .collect()
}

/// The chip a key press picks: `1`–`4` without modifiers.
pub fn key_index(key: &str, modified: bool) -> Option<usize> {
    if modified {
        return None;
    }
    let digit: usize = key.parse().ok()?;
    (1..=DOCK_KEYS).contains(&digit).then(|| digit - 1)
}

/// What tapping `person` does to `turn`: Confirm its suggestion when it is
/// that person, nothing when the turn already carries their name, else
/// Name.
pub fn tap(person: &DockPerson, turn: &LiveTurn) -> Option<(LiveAction, String)> {
    if let Some(suggested) = suggested_person(turn).filter(|p| person.is(p)) {
        return Some((LiveAction::Confirm, suggested.person_id.clone()));
    }
    if turn.status == LiveTurnStatus::Named && turn.named.as_ref().is_some_and(|p| person.is(p)) {
        return None;
    }
    Some((LiveAction::Name, person.person_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_ipc::live::LiveSuggestion;

    fn member(name: &str, email: Option<&str>) -> RosterMember {
        RosterMember {
            name: name.into(),
            email: email.map(Into::into),
            is_self: false,
        }
    }

    fn known(id: &str, name: &str, email: &str, voice: bool) -> KnownPerson {
        KnownPerson {
            id: id.into(),
            name: name.into(),
            emails: vec![email.into()],
            is_self: false,
            has_voice: voice,
            hue: 1,
        }
    }

    fn lp(id: &str, name: &str) -> LivePerson {
        LivePerson {
            person_id: id.into(),
            name: name.into(),
        }
    }

    fn turn(id: &str, start: f64, track: Track) -> LiveTurn {
        LiveTurn {
            session_id: "s".into(),
            turn_id: id.into(),
            revision: 1,
            epoch: 1,
            track,
            from_sample: 0,
            to_sample: 0,
            start_s: start,
            end_s: start + 3.0,
            text: "words".into(),
            words: Vec::new(),
            status: LiveTurnStatus::Voice,
            unresolved: None,
            voice: Some(3),
            suggestion: None,
            named: None,
            evidence: "e".into(),
        }
    }

    fn named(mut t: LiveTurn, p: LivePerson) -> LiveTurn {
        t.status = LiveTurnStatus::Named;
        t.named = Some(p);
        t
    }

    fn suggested(mut t: LiveTurn, p: LivePerson, source: SuggestionSource) -> LiveTurn {
        t.status = LiveTurnStatus::Suggested;
        t.suggestion = Some(LiveSuggestion {
            person: p,
            source,
            uncalibrated: false,
        });
        t
    }

    fn names(people: &[DockPerson]) -> Vec<&str> {
        people.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn people_merge_call_calendar_confirmed_and_live_without_yourself() {
        let observed = [
            member("Iris Vale", None),
            RosterMember {
                name: "Me Myself".into(),
                email: None,
                is_self: true,
            },
        ];
        let expected = [
            member("iris vale", Some("iris@example.test")),
            member("Noor Haddad", Some("noor@example.test")),
        ];
        let known = [
            known("p-iris", "Iris Vale", "iris@example.test", false),
            known("p-ben", "Ben Okafor", "ben@example.test", true),
            KnownPerson {
                is_self: true,
                ..known("p-me", "Owner", "me@example.test", true)
            },
        ];
        let confirmed = [("p-me".to_owned(), "Owner".to_owned())];
        let turns = [
            named(
                turn("m", 5.0, Track::Mic),
                lp("p-unregistered-me", "Synthetic Me"),
            ),
            named(
                turn("t2", 20.0, Track::System),
                lp("p-tomas", "Tomás Reyes"),
            ),
            suggested(
                turn("t1", 10.0, Track::System),
                lp("p-ben", "Ben Okafor"),
                SuggestionSource::Library,
            ),
        ];
        let people = people(DockInputs {
            observed: &observed,
            expected: &expected,
            confirmed: &confirmed,
            turns: &turns,
            known: &known,
        });
        assert_eq!(
            names(&people),
            ["Iris Vale", "Noor Haddad", "Tomás Reyes", "Ben Okafor"]
        );
        let iris = &people[0];
        assert_eq!(iris.person_id.as_deref(), Some("p-iris"));
        assert_eq!(
            iris.sources,
            BTreeSet::from([ChipSource::Call, ChipSource::Calendar])
        );
        // Unregistered: the e-mail is what names them.
        assert_eq!(people[1].person_ref(), "noor@example.test");
        assert!(people[3].has_voice);
    }

    #[test]
    fn an_ambiguous_name_stays_unresolved() {
        let known = [
            known("p-1", "Sam Lee", "a@example.test", false),
            known("p-2", "Sam Lee", "b@example.test", false),
        ];
        let observed = [member("Sam Lee", None)];
        let people = people(DockInputs {
            observed: &observed,
            known: &known,
            ..Default::default()
        });
        assert_eq!(people[0].person_id, None);
        assert_eq!(people[0].person_ref(), "Sam Lee");
    }

    #[test]
    fn a_roster_name_named_live_is_one_chip() {
        let observed = [member("Noor Haddad", None)];
        let turns = [named(
            turn("t1", 1.0, Track::System),
            lp("p-new", "Noor Haddad"),
        )];
        let people = people(DockInputs {
            observed: &observed,
            turns: &turns,
            ..Default::default()
        });
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].person_id.as_deref(), Some("p-new"));
    }

    #[test]
    fn order_is_first_seen_for_the_whole_recording() {
        let p = |name: &str| DockPerson {
            key: folded_name(name),
            name: name.into(),
            person_id: None,
            email: None,
            sources: BTreeSet::new(),
            has_voice: false,
            hue: None,
        };
        let mut order = Vec::new();
        let first = stable_order(&mut order, vec![p("Iris"), p("Noor")]);
        assert_eq!(names(&first), ["Iris", "Noor"]);
        // A new source listing someone earlier never moves the others.
        let second = stable_order(&mut order, vec![p("Ben"), p("Noor"), p("Iris")]);
        assert_eq!(names(&second), ["Iris", "Noor", "Ben"]);
        // Someone who leaves and comes back keeps their place.
        let third = stable_order(&mut order, vec![p("Ben"), p("Iris")]);
        assert_eq!(names(&third), ["Iris", "Ben"]);
        let fourth = stable_order(&mut order, vec![p("Ben"), p("Noor"), p("Iris")]);
        assert_eq!(names(&fourth), ["Iris", "Noor", "Ben"]);
    }

    #[test]
    fn chip_states_in_words() {
        let iris = lp("p-iris", "Iris Vale");
        let tomas = lp("p-tomas", "Tomás Reyes");
        let turns = [
            named(turn("a", 1.0, Track::System), iris.clone()),
            named(turn("b", 2.0, Track::System), iris.clone()),
            named(turn("c", 3.0, Track::System), iris.clone()),
            suggested(
                turn("d", 4.0, Track::System),
                tomas.clone(),
                SuggestionSource::Library,
            ),
            suggested(
                turn("e", 5.0, Track::System),
                tomas.clone(),
                SuggestionSource::Session,
            ),
        ];
        let known = [
            known("p-iris", "Iris Vale", "i@example.test", false),
            known("p-ben", "Ben Okafor", "b@example.test", true),
        ];
        let observed = [
            member("Iris Vale", None),
            member("Noor Haddad", None),
            member("Ben Okafor", None),
        ];
        let people = people(DockInputs {
            observed: &observed,
            turns: &turns,
            known: &known,
            ..Default::default()
        });
        // No target: plain counts.
        let lines: Vec<String> = chips(people.clone(), &turns, None)
            .iter()
            .map(Chip::line)
            .collect();
        assert_eq!(
            lines,
            [
                "Iris named · 3 turns",
                "Noor not heard yet",
                "Ben known voice",
                "Tomás suggested · 2 turns"
            ]
        );
        // The turn being spoken suggests Tomás: his chip asks to confirm.
        let chips = chips(people, &turns, speaking_now(&turns));
        assert_eq!(chips[3].state, ChipState::TapToConfirm);
        assert_eq!(chips[3].line(), "Tomás tap to confirm");
        assert!(chips[3].state.unconfirmed());
        assert_eq!(
            chips[3].spoken(speaking_now(&turns)),
            "Tomás Reyes, voice suggested, tap to confirm. Confirms the suggestion on the turn at 0:05. Key 4"
        );
    }

    #[test]
    fn only_the_first_four_have_keys() {
        let observed: Vec<_> = ["A a", "B b", "C c", "D d", "E e"]
            .iter()
            .map(|n| member(n, None))
            .collect();
        let chips = chips(
            people(DockInputs {
                observed: &observed,
                ..Default::default()
            }),
            &[],
            None,
        );
        let keys: Vec<_> = chips.iter().map(|c| c.key).collect();
        assert_eq!(keys, [Some(0), Some(1), Some(2), Some(3), None]);
    }

    #[test]
    fn keys_one_to_four_without_modifiers() {
        assert_eq!(key_index("1", false), Some(0));
        assert_eq!(key_index("4", false), Some(3));
        assert_eq!(key_index("5", false), None);
        assert_eq!(key_index("0", false), None);
        assert_eq!(key_index("1", true), None);
        assert_eq!(key_index("n", false), None);
    }

    #[test]
    fn speaking_now_is_the_latest_turn_by_time() {
        // Published out of time order (two tracks).
        let turns = [
            turn("late", 30.0, Track::System),
            turn("early", 10.0, Track::Mic),
        ];
        assert_eq!(speaking_now(&turns).unwrap().turn_id, "late");
        assert!(speaking_now(&[]).is_none());
    }

    #[test]
    fn retargeting_and_back() {
        let turns = [
            turn("old", 721.0, Track::System),
            turn("now", 760.0, Track::System),
        ];
        let (t, older) = target_turn(&DockTarget::Turn("old".into()), &turns);
        assert_eq!((t.unwrap().turn_id.as_str(), older), ("old", true));
        assert_eq!(target_title(t, older), "Naming the turn at 12:01");
        let (t, older) = target_turn(&DockTarget::SpeakingNow, &turns);
        assert_eq!((t.unwrap().turn_id.as_str(), older), ("now", false));
        assert_eq!(target_title(t, older), "Speaking now");
        // A turn that left the preview, or is the one being spoken, is
        // just the turn being spoken.
        let (t, older) = target_turn(&DockTarget::Turn("gone".into()), &turns);
        assert_eq!((t.unwrap().turn_id.as_str(), older), ("now", false));
        let (_, older) = target_turn(&DockTarget::Turn("now".into()), &turns);
        assert!(!older);
    }

    #[test]
    fn blocked_reasons() {
        let mic = turn("m", 1.0, Track::Mic);
        let other = turn("o", 1.0, Track::System);
        assert!(blocked(Some(&other), false, true).is_none());
        assert!(
            blocked(Some(&mic), false, true)
                .unwrap()
                .contains("microphone")
        );
        assert!(blocked(None, true, true).unwrap().contains("paused"));
        assert!(blocked(None, false, true).unwrap().contains("No live turn"));
        assert!(
            blocked(Some(&other), false, false)
                .unwrap()
                .contains("Agent")
        );
    }

    #[test]
    fn tap_names_confirms_or_does_nothing() {
        let person = |id: Option<&str>, name: &str| DockPerson {
            key: folded_name(name),
            name: name.into(),
            person_id: id.map(Into::into),
            email: None,
            sources: BTreeSet::new(),
            has_voice: false,
            hue: None,
        };
        let tomas = person(Some("p-tomas"), "Tomás Reyes");
        let noor = person(None, "Noor Haddad");
        let sugg = suggested(
            turn("t", 1.0, Track::System),
            lp("p-tomas", "Tomás Reyes"),
            SuggestionSource::Library,
        );
        assert_eq!(
            tap(&tomas, &sugg),
            Some((LiveAction::Confirm, "p-tomas".into()))
        );
        // Someone else on a suggested turn: name them (never a reject).
        assert_eq!(
            tap(&noor, &sugg),
            Some((LiveAction::Name, "Noor Haddad".into()))
        );
        let done = named(turn("t", 1.0, Track::System), lp("p-tomas", "Tomás Reyes"));
        assert_eq!(tap(&tomas, &done), None);
        assert_eq!(
            tap(&noor, &done),
            Some((LiveAction::Name, "Noor Haddad".into()))
        );
    }

    #[test]
    fn first_names() {
        assert_eq!(first_name("Tomás Reyes"), "Tomás");
        assert_eq!(first_name("noor@example.test"), "noor");
        assert_eq!(first_name("  Iris "), "Iris");
    }
}
