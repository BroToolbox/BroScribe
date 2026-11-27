//! The person picker shared by the meeting's Transcript and Speakers tabs and
//! the review queue's Name action: search the registered people and choose
//! one by [`PersonId`], so naming never depends on retyping an exact display
//! name.
//!
//! - The people come from `Index::list_people`, read on the background
//!   executor and re-read whenever [`AppState::people_revision`] changes;
//!   late reads are discarded by generation.
//! - Search is case- and accent-insensitive: every typed word must occur in
//!   the person's name, an alias, an email or the ID ([`folded_name`] keys
//!   computed once per load). At most [`MAX_RESULTS`] candidates are shown;
//!   a keystroke scans the people once and orders only those shown
//!   ([`PeopleIndex`] keeps positions, names and the browse order per load).
//! - Candidates show their email and, when display names repeat, their ID.
//! - Opened for a speaker ([`PersonPicker::set_voice_target`]), the picker
//!   ranks people by voice with `ops::voice_matches` (the order the resolver
//!   names from) over [`AppState::people_samples`]. With an empty query the
//!   [`VOICE_ROWS`] closest voices come first ("Closest voices": raw
//!   similarity, a "likely" badge when the resolver would name them, the
//!   current suggestion marked), then everyone else; with a query, matching
//!   people keep search order and show their similarity. A new people
//!   revision, model or thresholds, or a changed target, clears the scores
//!   at once and recomputes them. A host reload drops any result in flight
//!   when it starts ([`PersonPicker::begin_voice_reload`]) and keeps the
//!   scores shown when it ends with the same target. Late results are
//!   dropped, and a failure only hides the scores.
//! - Typing highlights the first existing match. Arrow keys move the highlight
//!   without taking focus from search. Return chooses the highlighted option.
//! - The final option offers a new name only when it is not an exact match,
//!   email or ID. Creating it requires an explicit arrow or click selection.
//!   The highlight follows its person when people or voices arrive late.
//! - `Escape` or Cancel ends the picker.
//!
//! The picker only reports [`PersonPickerEvent`]s; the host binds the choice
//! to the meeting / speaker it opened the picker for and writes it.

use crate::ui::kit::{self, BadgeTone, button};
use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use broscribe_core::config::Thresholds;
use broscribe_core::index::{Index, PersonSummary};
use broscribe_core::names::folded_name;
use broscribe_core::ops::{self, VoiceMatch};
use broscribe_core::schema::{Meeting, Speaker};
use broscribe_core::{Error, Library, PersonId, Result, SpeakerId};
use gpui::component::ActiveTheme as _;
use gpui::component::input::{Enter, Escape, Input, InputEvent, InputState, MoveDown, MoveUp};
use gpui::component::kbd::Kbd;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, Focusable as _, FontWeight,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, rems,
};

use crate::state::AppState;
use crate::ui::text_line;
use crate::ui::type_scale::{TextRole, TextRoleExt as _};

/// Most candidates shown at once; typing narrows the rest.
pub const MAX_RESULTS: usize = 8;
/// Closest voices listed first with an empty query.
pub const VOICE_ROWS: usize = 3;

// ---- model (pure, tested) ----

/// A registered person with precomputed search keys.
#[derive(Clone, Debug, PartialEq)]
pub struct PickerPerson {
    pub id: PersonId,
    /// The display name: the name, `Me` for an unnamed self, else the ID.
    pub name: String,
    pub is_self: bool,
    pub aliases: Vec<String>,
    pub emails: Vec<String>,
    pub meetings: u32,
    /// Another person shows the same (folded) display name.
    pub duplicate_name: bool,
    /// Folded name, aliases, emails and the ID: what words match against.
    keys: Vec<String>,
    /// Folded name and aliases: what a typed name must not equal.
    exact: Vec<String>,
}

impl PickerPerson {
    fn new(summary: PersonSummary) -> Self {
        let name = display_name(&summary.name, summary.is_self, &summary.id);
        let mut exact: Vec<String> = std::iter::once(summary.name.as_str())
            .chain(summary.aliases.iter().map(String::as_str))
            .map(folded_name)
            .filter(|k| !k.is_empty())
            .collect();
        if summary.is_self {
            exact.push("me".to_owned());
        }
        let mut keys = exact.clone();
        keys.extend(
            summary
                .emails
                .iter()
                .map(|e| folded_name(e))
                .filter(|k| !k.is_empty()),
        );
        keys.push(summary.id.as_str().to_owned());
        Self {
            id: summary.id,
            name,
            is_self: summary.is_self,
            aliases: summary.aliases,
            emails: summary.emails,
            meetings: summary.meetings,
            duplicate_name: false,
            keys,
            exact,
        }
    }

    /// What tells this candidate apart: email, aliases, the ID when the
    /// display name repeats (or is the ID), and the speaker-meeting count.
    pub fn detail(&self) -> String {
        let mut parts = Vec::new();
        if self.is_self {
            parts.push("you".to_owned());
        }
        if let Some(email) = self.emails.first() {
            parts.push(email.clone());
        }
        if !self.aliases.is_empty() {
            let shown: Vec<&str> = self.aliases.iter().take(3).map(String::as_str).collect();
            parts.push(format!("also {}", shown.join(", ")));
        }
        if self.duplicate_name {
            parts.push(format!("ID {}", self.id));
        }
        parts.push(speaker_meetings(self.meetings));
        parts.join(" · ")
    }

    fn rank(&self, tokens: &[String], folded_query: &str) -> Option<u8> {
        if !tokens
            .iter()
            .all(|t| self.keys.iter().any(|k| k.contains(t.as_str())))
        {
            return None;
        }
        Some(if tokens.is_empty() {
            2
        } else if self.exact.iter().any(|e| e == folded_query) {
            0
        } else if self.keys.iter().any(|k| k.starts_with(folded_query)) {
            1
        } else {
            2
        })
    }
}

pub(super) fn display_name(name: &str, is_self: bool, id: &PersonId) -> String {
    let name = name.trim();
    match (name.is_empty(), is_self) {
        (false, _) => name.to_owned(),
        (true, true) => "Me".to_owned(),
        (true, false) => id.to_string(),
    }
}

/// A [`PersonSummary`] meeting count as shown: meetings where the person
/// names a whole speaker (confirmed or automatic). Corrected lines and
/// attendance are not counted, so zero never claims "no meetings".
pub fn speaker_meetings(meetings: u32) -> String {
    match meetings {
        0 => "Not named in a meeting yet".to_owned(),
        1 => "Named in 1 meeting".to_owned(),
        n => format!("Named in {n} meetings"),
    }
}

/// The registered people of one load, with what every keystroke needs
/// computed once: positions by ID, typed-name keys and the browse order.
/// Searching never sorts or deduplicates the whole list again.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PeopleIndex {
    people: Vec<PickerPerson>,
    /// Each person's position, by ID.
    by_id: HashMap<PersonId, usize>,
    /// Folded names and aliases: the first person (list order) with each.
    exact: HashMap<String, usize>,
    /// Everyone in empty-query order: most meetings first, then list order.
    browse: Vec<usize>,
}

impl std::ops::Deref for PeopleIndex {
    type Target = [PickerPerson];

    fn deref(&self) -> &[PickerPerson] {
        &self.people
    }
}

impl PeopleIndex {
    /// The position of `id`, when listed.
    pub fn position(&self, id: &PersonId) -> Option<usize> {
        self.by_id.get(id).copied()
    }
}

/// Search keys for `people` (in `Index::list_people` order), flagging
/// repeated display names; linear apart from the one browse-order sort.
pub fn index_people(people: Vec<PersonSummary>) -> PeopleIndex {
    let mut people: Vec<PickerPerson> = people.into_iter().map(PickerPerson::new).collect();
    let folded: Vec<String> = people.iter().map(|p| folded_name(&p.name)).collect();
    let mut repeats: HashMap<&str, usize> = HashMap::with_capacity(folded.len());
    for name in &folded {
        *repeats.entry(name.as_str()).or_default() += 1;
    }
    for (person, name) in people.iter_mut().zip(&folded) {
        person.duplicate_name = repeats[name.as_str()] > 1;
    }
    let by_id = people
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id.clone(), i))
        .collect();
    let mut exact = HashMap::new();
    for (i, person) in people.iter().enumerate() {
        for key in &person.exact {
            exact.entry(key.clone()).or_insert(i);
        }
    }
    let mut browse: Vec<usize> = (0..people.len()).collect();
    browse.sort_unstable_by_key(|&i| (Reverse(people[i].meetings), i));
    PeopleIndex {
        people,
        by_id,
        exact,
        browse,
    }
}

/// Every registered person, ready to search (reads the disk: background
/// executor). Empty without an index.
pub fn load_people(lib: &Library) -> Result<PeopleIndex> {
    let root = lib.root();
    let summaries = match Index::open_readonly(root) {
        Ok(index) => index.list_people()?,
        Err(Error::NotFound(_)) if !Index::path(root).exists() => Vec::new(),
        Err(e) => return Err(e),
    };
    Ok(index_people(summaries))
}

fn query_tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(folded_name)
        .filter(|t| !t.is_empty())
        .collect()
}

/// Candidates for `query`, best first (indices into `people`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Matches {
    /// At most [`MAX_RESULTS`].
    pub shown: Vec<usize>,
    /// Every matching person.
    pub total: usize,
    /// The first `voice` entries of `shown` are the closest voices.
    pub voice: usize,
}

/// People whose keys contain every word of `query`: exact names and aliases
/// first, then prefixes, then other matches; ties by meeting count, then in
/// list (name) order. An empty query lists everyone.
pub fn search(people: &PeopleIndex, query: &str) -> Matches {
    let (total, shown) = ranked(people, query, MAX_RESULTS);
    Matches {
        total,
        shown,
        voice: 0,
    }
}

/// All matching row indices for the People list, retaining its display order.
pub(super) fn matching_people(people: &PeopleIndex, query: &str) -> Vec<usize> {
    let tokens = query_tokens(query);
    let folded = tokens.join(" ");
    people
        .iter()
        .enumerate()
        .filter(|(_, person)| tokens.is_empty() || person.rank(&tokens, &folded).is_some())
        .map(|(index, _)| index)
        .collect()
}

/// How many people match [`search`], and the first `limit` of them in its
/// order: one pass over the people and a partial selection, never a sort
/// of every match (an empty query reads the precomputed browse order).
fn ranked(people: &PeopleIndex, query: &str, limit: usize) -> (usize, Vec<usize>) {
    let tokens = query_tokens(query);
    if tokens.is_empty() {
        return (
            people.browse.len(),
            people.browse.iter().take(limit).copied().collect(),
        );
    }
    let folded = tokens.join(" ");
    let mut hits: Vec<(u8, Reverse<u32>, usize)> = people
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            p.rank(&tokens, &folded)
                .map(|r| (r, Reverse(p.meetings), i))
        })
        .collect();
    let total = hits.len();
    if hits.len() > limit {
        hits.select_nth_unstable(limit);
        hits.truncate(limit);
    }
    hits.sort_unstable();
    (total, hits.into_iter().map(|h| h.2).collect())
}

/// [`search`] with the speaker's closest voices (`voices`, best first): with
/// an empty query, the first [`VOICE_ROWS`] of them that are registered
/// people lead the list, followed by everyone else in search order (no one
/// twice); with a query, plain search order.
pub fn compose(people: &PeopleIndex, query: &str, voices: &[VoiceMatch]) -> Matches {
    if !query_tokens(query).is_empty() || voices.is_empty() {
        return search(people, query);
    }
    let lead: Vec<usize> = voices
        .iter()
        .filter_map(|v| people.position(&v.person_id))
        .take(VOICE_ROWS.min(MAX_RESULTS))
        .collect();
    let (total, rest) = ranked(people, query, MAX_RESULTS + lead.len());
    Matches {
        total,
        shown: lead
            .iter()
            .copied()
            .chain(rest.into_iter().filter(|i| !lead.contains(i)))
            .take(MAX_RESULTS)
            .collect(),
        voice: lead.len(),
    }
}

/// "voice 0.87": a raw similarity with two decimals, not a probability.
pub fn voice_score(score: f32) -> String {
    format!("voice {score:.2}")
}

/// The speaker [`PersonPicker::set_voice_target`] ranks people against: the
/// meeting as the host read it, so a grouped member ranks as its kept
/// speaker without any member's rejected people (the ranking equals
/// resolution).
#[derive(Clone, Debug)]
pub struct VoiceTarget {
    pub meeting: Meeting,
    pub speaker_id: SpeakerId,
    /// `Meeting::roster_person_ids`, as the resolver uses it.
    pub roster: BTreeSet<PersonId>,
}

impl VoiceTarget {
    /// `speaker` of `meeting`, as the meeting was read; `None` when absent.
    pub fn of(meeting: &Meeting, speaker: SpeakerId) -> Option<Self> {
        meeting.speaker(speaker)?;
        Some(Self {
            meeting: meeting.clone(),
            speaker_id: speaker,
            roster: meeting.roster_person_ids(),
        })
    }

    /// The target speaker as read.
    pub fn speaker(&self) -> Option<&Speaker> {
        self.meeting.speaker(self.speaker_id)
    }
}

/// The same target when everything the ranking reads is unchanged: the
/// meeting, the speaker, its speakers and groups (rejections, merges) and
/// the roster. Other meeting edits keep the scores shown.
impl PartialEq for VoiceTarget {
    fn eq(&self, other: &Self) -> bool {
        self.meeting.id == other.meeting.id
            && self.speaker_id == other.speaker_id
            && self.roster == other.roster
            && self.meeting.speakers == other.meeting.speakers
            && self.meeting.speaker_groups == other.meeting.speaker_groups
    }
}

/// What "Use typed name" would do with the query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypedName {
    /// Nothing typed.
    Empty,
    /// The trimmed text, offered as a name.
    Offer(String),
    /// A registered person (display name) already has this name or alias.
    Registered(String),
    /// An email or a person ID: only registered people can match it.
    Reference,
}

/// Whether `query` is offered as a typed name.
pub fn typed_name(people: &PeopleIndex, query: &str) -> TypedName {
    let text = query.trim();
    if text.is_empty() {
        return TypedName::Empty;
    }
    if text.contains('@') || PersonId::parse(text).is_ok() {
        return TypedName::Reference;
    }
    match people.exact.get(&folded_name(text)) {
        Some(&i) => TypedName::Registered(people[i].name.clone()),
        None => TypedName::Offer(text.to_owned()),
    }
}

// ---- view ----

/// What the user chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersonChoice {
    /// A registered person, by ID.
    Existing(PersonId),
    /// Text from "Use typed name": resolved or created by the backend.
    Typed(String),
}

impl PersonChoice {
    /// The `person_ref` for `ops::name_speaker`: an ID never falls back to a
    /// name, so a deleted or merged person fails with `NotFound`.
    pub fn person_ref(&self) -> &str {
        match self {
            Self::Existing(id) => id.as_str(),
            Self::Typed(text) => text,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersonPickerEvent {
    Picked(PersonChoice),
    Cancelled,
}

pub struct PersonPicker {
    state: Entity<AppState>,
    input: Entity<InputState>,
    /// Accessible name of the search field (`Name for Speaker 2`).
    label: SharedString,
    cancel_button: bool,
    existing_only: bool,
    people: PeopleIndex,
    loaded: bool,
    loading: bool,
    error: Option<String>,
    generation: u64,
    seen_people_revision: u64,
    query: String,
    matches: Matches,
    /// Index into the shown choices, with the typed name after existing people.
    highlighted: Option<usize>,
    /// Shown after `Enter` with nothing highlighted.
    hint: Option<&'static str>,
    /// The speaker whose closest voices are listed.
    voice_target: Option<Arc<VoiceTarget>>,
    /// Bumped by every voice request; only the matching result is applied.
    voice_generation: u64,
    /// The model and thresholds of the current voice request.
    seen_voice_settings: VoiceSettings,
    /// Every person ranked by voice for the current request, best first.
    voices: Vec<VoiceMatch>,
    /// The position in `voices` of each person, so a rendered option finds
    /// its score without scanning everyone.
    voice_of: HashMap<PersonId, usize>,
    /// `voices` is the applied result of the current request.
    voices_ready: bool,
    voice_failed: bool,
    _subscriptions: Vec<Subscription>,
}

/// What voice scores depend on besides the samples and the target.
type VoiceSettings = (String, Option<Thresholds>);

fn voice_settings(state: &AppState) -> VoiceSettings {
    let config = state.config();
    let model = config.profile.speaker_embedding.clone();
    let thresholds = config.thresholds(&model).copied();
    (model, thresholds)
}

impl EventEmitter<PersonPickerEvent> for PersonPicker {}

impl PersonPicker {
    /// A picker with an empty query and focus in its search field; loads the
    /// people in the background.
    pub fn new(
        state: Entity<AppState>,
        label: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = Self::new_unfocused(state, label, window, cx);
        picker.focus_search(window, cx);
        picker
    }

    pub(crate) fn new_unfocused(
        state: Entity<AppState>,
        label: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search people by name, alias or email")
        });
        let subscriptions = vec![
            cx.subscribe(
                &input,
                |this: &mut Self, input, ev: &InputEvent, cx| match ev {
                    InputEvent::Change => {
                        let text = input.read(cx).value().to_string();
                        this.query_changed(text, cx);
                    }
                    InputEvent::PressEnter { .. } => this.confirm_highlighted(cx),
                    _ => {}
                },
            ),
            cx.observe(&state, |this: &mut Self, state, cx| {
                let (revision, settings) = {
                    let s = state.read(cx);
                    (s.people_revision(), voice_settings(s))
                };
                let people_changed = revision != this.seen_people_revision;
                if people_changed {
                    this.seen_people_revision = revision;
                    this.reload(cx);
                }
                if people_changed || settings != this.seen_voice_settings {
                    this.request_voices(cx);
                }
            }),
        ];
        let (seen_people_revision, seen_voice_settings) = {
            let s = state.read(cx);
            (s.people_revision(), voice_settings(s))
        };
        let mut this = Self {
            state,
            input,
            label: label.into(),
            cancel_button: true,
            existing_only: false,
            people: PeopleIndex::default(),
            loaded: false,
            loading: false,
            error: None,
            generation: 0,
            seen_people_revision,
            query: String::new(),
            matches: Matches::default(),
            highlighted: None,
            hint: None,
            voice_target: None,
            voice_generation: 0,
            seen_voice_settings,
            voices: Vec::new(),
            voice_of: HashMap::new(),
            voices_ready: false,
            voice_failed: false,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    /// Re-reads the people on the background executor.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        let lib = self.state.read(cx).library().clone();
        let task = cx
            .background_executor()
            .spawn(async move { load_people(&lib) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |picker, cx| picker.apply_people(generation, result, cx))
                .ok();
        })
        .detach();
        cx.notify();
    }

    /// Applies a load; `false` (nothing changes) when a newer one started.
    /// The highlighted person stays highlighted while still listed.
    pub fn apply_people(
        &mut self,
        generation: u64,
        result: Result<PeopleIndex>,
        cx: &mut Context<Self>,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        match result {
            Ok(people) => {
                // By ID: the new list may add, drop or reorder people.
                let kept = self.highlighted_id();
                self.people = people;
                let first_load = !self.loaded;
                self.loaded = true;
                self.error = None;
                self.recompose(kept);
                if first_load {
                    self.highlighted = self.first_query_match();
                }
            }
            Err(e) => {
                tracing::warn!(code = e.code(), "reading registered people");
                self.error = Some(crate::jobs::core_user_text(&e))
            }
        }
        cx.notify();
        true
    }

    /// Lists the closest voices to `target` (the host passes the speaker as
    /// it read it, again after every reload of its meeting); `None` lists
    /// none. A different target clears the scores shown until the new ones
    /// arrive; the same target keeps them when they are already shown and
    /// is requested again when a reload dropped its request.
    pub fn set_voice_target(&mut self, target: Option<Arc<VoiceTarget>>, cx: &mut Context<Self>) {
        let same = match (&self.voice_target, &target) {
            (Some(old), Some(new)) => old == new,
            (None, None) => true,
            _ => false,
        };
        if same && (self.voices_ready || target.is_none()) {
            return;
        }
        self.voice_target = target;
        self.request_voices(cx);
    }

    /// A host reload of the target's meeting started: any voice result in
    /// flight is dropped (it may describe the speaker as it was), while the
    /// scores shown stay until the reload completes with
    /// [`Self::set_voice_target`].
    pub fn begin_voice_reload(&mut self) {
        self.voice_generation += 1;
    }

    /// A host reload ended without re-reading the speaker: requests the
    /// voices again when [`Self::begin_voice_reload`] dropped a request.
    pub fn resume_voices(&mut self, cx: &mut Context<Self>) {
        if !self.voices_ready && self.voice_target.is_some() {
            self.request_voices(cx);
        }
    }

    /// Starts a voice request for the current target, people revision,
    /// model and thresholds; the scores shown so far are cleared at once.
    fn request_voices(&mut self, cx: &mut Context<Self>) {
        self.voice_generation += 1;
        let generation = self.voice_generation;
        self.voices_ready = false;
        self.voice_failed = false;
        if !self.voices.is_empty() {
            self.voices.clear();
            self.voice_of.clear();
            self.recompose(self.highlighted_id());
        }
        cx.notify();
        self.seen_voice_settings = voice_settings(self.state.read(cx));
        let Some(target) = self.voice_target.clone() else {
            return;
        };
        let (model, thresholds) = self.seen_voice_settings.clone();
        let load = self.state.update(cx, |s, cx| s.people_samples(cx));
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let result = match load.await {
                Ok(people) => {
                    executor
                        .spawn(async move {
                            ops::voice_matches(
                                &people,
                                &target.meeting,
                                target.speaker_id,
                                Some(&target.roster),
                                thresholds.as_ref(),
                                &model,
                                usize::MAX,
                            )
                            .map_err(|e| e.code().to_owned())
                        })
                        .await
                }
                Err(e) => Err(e),
            };
            this.update(cx, |picker, cx| picker.apply_voices(generation, result, cx))
                .ok();
        })
        .detach();
    }

    /// Applies a voice request; `false` (nothing changes) when a newer one
    /// started. A failure lists no voices; search keeps working.
    pub fn apply_voices(
        &mut self,
        generation: u64,
        result: std::result::Result<Vec<VoiceMatch>, String>,
        cx: &mut Context<Self>,
    ) -> bool {
        if generation != self.voice_generation {
            return false;
        }
        self.voices = match result {
            Ok(voices) => {
                self.voices_ready = true;
                voices
            }
            Err(code) => {
                tracing::warn!(%code, "closest voices unavailable");
                self.voice_failed = true;
                Vec::new()
            }
        };
        self.voice_of = self
            .voices
            .iter()
            .enumerate()
            .map(|(i, v)| (v.person_id.clone(), i))
            .collect();
        self.recompose(self.highlighted_id());
        cx.notify();
        true
    }

    /// The highlighted person's ID.
    fn highlighted_id(&self) -> Option<PersonId> {
        self.highlighted_person().map(|p| p.id.clone())
    }

    /// Recomputes the shown candidates, keeping `kept` (the highlighted
    /// person's ID, taken before the people or voices changed) highlighted
    /// while still listed.
    fn recompose(&mut self, kept: Option<PersonId>) {
        let kept_typed = self.highlighted == Some(self.matches.shown.len());
        self.matches = compose(&self.people, &self.query, &self.voices);
        self.highlighted = kept.and_then(|id| {
            self.matches
                .shown
                .iter()
                .position(|&i| self.people[i].id == id)
        });
        if kept_typed && matches!(self.typed(), TypedName::Offer(_)) {
            self.highlighted = Some(self.matches.shown.len());
        }
    }

    /// The current voice request's generation.
    #[cfg(test)]
    pub(crate) fn voice_generation(&self) -> u64 {
        self.voice_generation
    }

    /// The speaker the closest voices are computed for.
    #[cfg(test)]
    pub(crate) fn voice_target(&self) -> Option<&VoiceTarget> {
        self.voice_target.as_deref()
    }

    /// The voice match of `person` shown in the list, if any.
    pub fn voice_of(&self, person: &PersonId) -> Option<&VoiceMatch> {
        self.voices.get(*self.voice_of.get(person)?)
    }

    /// The search text as typed.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Replaces the query (as typing does).
    pub fn set_query(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        self.query_changed(text.to_owned(), cx);
    }

    fn query_changed(&mut self, text: String, cx: &mut Context<Self>) {
        if text == self.query {
            return;
        }
        self.query = text;
        self.matches = compose(&self.people, &self.query, &self.voices);
        self.highlighted = self.first_query_match();
        self.hint = None;
        cx.notify();
    }

    fn first_query_match(&self) -> Option<usize> {
        (!self.query.trim().is_empty() && !self.matches.shown.is_empty()).then_some(0)
    }

    /// The shown candidates, best first.
    pub fn candidates(&self) -> impl Iterator<Item = &PickerPerson> {
        self.matches.shown.iter().map(|&i| &self.people[i])
    }

    /// The "Closest voices" section: the first shown candidates.
    #[cfg(test)]
    pub(crate) fn closest_voices(&self) -> impl Iterator<Item = &PickerPerson> {
        self.candidates().take(self.matches.voice)
    }

    #[cfg(test)]
    pub(crate) fn highlighted(&self) -> Option<usize> {
        self.highlighted
    }

    fn highlighted_person(&self) -> Option<&PickerPerson> {
        let i = *self.matches.shown.get(self.highlighted?)?;
        self.people.get(i)
    }

    /// What "Use typed name" would offer for the current query.
    pub fn typed(&self) -> TypedName {
        if self.existing_only || !self.loaded || self.loading || self.error.is_some() {
            TypedName::Empty
        } else {
            typed_name(&self.people, &self.query)
        }
    }

    /// Whether the search field has the keyboard focus.
    pub(crate) fn search_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        use gpui::Focusable as _;
        self.input.focus_handle(cx).is_focused(window)
    }

    pub(crate) fn set_existing_only(&mut self) {
        self.existing_only = true;
    }

    pub(crate) fn set_cancel_button(&mut self, show: bool) {
        self.cancel_button = show;
    }

    pub(crate) fn search_focus(&self, cx: &gpui::App) -> gpui::FocusHandle {
        use gpui::Focusable as _;
        self.input.focus_handle(cx)
    }

    pub(crate) fn focus_search(&self, window: &mut Window, cx: &mut gpui::App) {
        self.search_focus(cx).focus(window, cx);
    }

    /// Moves the highlight; from none, `↓` starts at the first candidate and
    /// `↑` at the last.
    pub fn move_highlight(&mut self, down: bool, cx: &mut Context<Self>) {
        let len =
            self.matches.shown.len() + usize::from(matches!(self.typed(), TypedName::Offer(_)));
        if len == 0 {
            return;
        }
        self.highlighted = Some(match (self.highlighted, down) {
            (None, true) => 0,
            (None, false) => len - 1,
            (Some(h), true) => (h + 1).min(len - 1),
            (Some(h), false) => h.saturating_sub(1),
        });
        self.hint = None;
        cx.notify();
    }

    /// `Enter`: chooses the highlighted candidate; without one, only explains.
    pub fn confirm_highlighted(&mut self, cx: &mut Context<Self>) {
        match self.highlighted {
            Some(ix) if ix == self.matches.shown.len() => self.use_typed(cx),
            Some(ix) => self.pick(ix, cx),
            None => {
                self.hint = Some("Highlight a person with ↓ first.");
                cx.notify();
            }
        }
    }

    /// Chooses the shown candidate `ix`.
    pub fn pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(&i) = self.matches.shown.get(ix) {
            let id = self.people[i].id.clone();
            cx.emit(PersonPickerEvent::Picked(PersonChoice::Existing(id)));
        }
    }

    /// A rendered option carries its identity, never a position from an old frame.
    fn pick_id(&mut self, id: &PersonId, cx: &mut Context<Self>) {
        if let Some(person) = self.candidates().find(|person| person.id == *id) {
            cx.emit(PersonPickerEvent::Picked(PersonChoice::Existing(
                person.id.clone(),
            )));
        }
    }

    /// "Use typed name": only when the query is offered as a name.
    pub fn use_typed(&mut self, cx: &mut Context<Self>) {
        if let TypedName::Offer(text) = self.typed() {
            cx.emit(PersonPickerEvent::Picked(PersonChoice::Typed(text)));
        }
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(PersonPickerEvent::Cancelled);
    }

    fn status(&self) -> String {
        if !self.loaded || self.loading {
            return "Loading people…".to_owned();
        }
        if let Some(hint) = self.hint {
            return hint.to_owned();
        }
        match self.typed() {
            TypedName::Registered(name) => return format!("{name} is already in the list."),
            TypedName::Reference => {
                return "Emails and IDs only match people already in the list.".to_owned();
            }
            _ => {}
        }
        let shown = self.matches.shown.len();
        match (shown, self.query.trim().is_empty()) {
            (0, true) => "No people yet. Type a name to add one.".to_owned(),
            (0, false) => format!("Nobody matches “{}”.", self.query.trim()),
            _ if self.matches.total > shown => {
                format!(
                    "Showing {shown} of {} · keep typing to narrow",
                    self.matches.total
                )
            }
            (1, _) => "1 person".to_owned(),
            _ => format!("{shown} people"),
        }
    }

    /// Who is highlighted, for the non-live `person-picker-highlight` label.
    ///
    /// Focus stays in the search field, which is not an ancestor of the
    /// options, so GPUI would ignore an active-descendant claim on the
    /// highlighted option; and the highlight stays out of the live status
    /// line (ACC-18). Its own `Label` keeps it readable without speaking on
    /// every arrow key (design §4.2).
    fn highlight_text(&self) -> Option<String> {
        let ix = self.highlighted?;
        if let Some(person) = self.highlighted_person() {
            return Some(match self.option_voice(ix, person) {
                Some(voice) => {
                    format!("{}, voice similarity {:.2}", person.name, voice.score_value)
                }
                None => person.name.clone(),
            });
        }
        if let TypedName::Offer(name) = self.typed() {
            return Some(format!("Add “{name}” as a new person"));
        }
        None
    }

    /// What option `ix` (showing `person`) says about their voice: in the
    /// closest voices section, the similarity, the "likely" badge and the
    /// current suggestion; with a query, the similarity and the current
    /// suggestion.
    fn option_voice(&self, ix: usize, person: &PickerPerson) -> Option<OptionVoice> {
        let in_section = ix < self.matches.voice;
        if !in_section && query_tokens(&self.query).is_empty() {
            return None;
        }
        let v = self.voice_of(&person.id)?;
        let likely = in_section && v.likely;
        let mut aria = format!("voice similarity {:.2}", v.score);
        if likely {
            aria.push_str(", likely");
        }
        if v.current_suggestion {
            aria.push_str(", current suggestion");
        }
        Some(OptionVoice {
            score: voice_score(v.score),
            score_value: v.score,
            aria,
            likely,
            current: v.current_suggestion,
        })
    }

    fn render_option(
        &self,
        ix: usize,
        person: &PickerPerson,
        focused: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let highlighted = self.highlighted == Some(ix);
        let detail = person.detail();
        let voice = self.option_voice(ix, person);
        let aria = match &voice {
            Some(v) => format!("{}, {}, {detail}", person.name, v.aria),
            None => format!("{}, {detail}", person.name),
        };
        let click_id = person.id.clone();
        let mut facts = Vec::new();
        if let Some(email) = person.emails.first() {
            facts.push(email.clone());
        }
        facts.push(speaker_meetings(person.meetings));
        if person.duplicate_name {
            facts.push(format!("ID {}", person.id));
        }
        let folded = folded_name(self.query.trim());
        if !folded.is_empty()
            && let Some(alias) = person
                .aliases
                .iter()
                .find(|name| folded_name(name).contains(&folded))
        {
            facts.push(format!("also {alias}"));
        }
        let metadata = kit::row_metadata_colour(highlighted, cx);
        kit::list_row(("person-picker-option", ix), highlighted, focused, cx)
            .debug_selector(move || format!("person-picker-option-{ix}"))
            .role(Role::ListBoxOption)
            .aria_label(aria)
            .aria_selected(highlighted)
            .h(rems(2.75))
            .flex_none()
            .flex_col()
            .justify_center()
            .gap_0()
            .overflow_hidden()
            .tooltip(kit::title_tooltip(person.name.clone()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.pick_id(&click_id, cx);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_role(TextRole::Label)
                            .font_weight(FontWeight::MEDIUM)
                            .child(person.name.clone()),
                    )
                    .when(person.is_self, |row| {
                        row.child(kit::badge(BadgeTone::Neutral, "You", cx))
                    })
                    .when(voice.as_ref().is_some_and(|v| v.current), |row| {
                        row.child(
                            kit::badge(BadgeTone::Suggested, "Suggested", cx)
                                .debug_selector(move || format!("picker-suggested-{ix}")),
                        )
                    })
                    .when(voice.as_ref().is_some_and(|v| v.likely), |row| {
                        row.child(
                            kit::badge(BadgeTone::Suggested, "Likely", cx)
                                .debug_selector(move || format!("picker-likely-{ix}")),
                        )
                    })
                    .when_some(voice, |row, voice| {
                        row.child(
                            div()
                                .id(("picker-voice-score", ix))
                                .debug_selector(move || format!("picker-voice-score-{ix}"))
                                .ml_auto()
                                .flex_none()
                                .text_role(TextRole::Caption)
                                .text_color(metadata)
                                .child(voice.score),
                        )
                    }),
            )
            .child(
                div()
                    .truncate()
                    .text_role(TextRole::Caption)
                    .text_color(metadata)
                    .child(facts.join(" · ")),
            )
            .into_any_element()
    }
}

/// What one option shows about its voice ([`PersonPicker::option_voice`]).
struct OptionVoice {
    /// "voice 0.87".
    score: String,
    score_value: f32,
    /// "voice similarity 0.87, likely, current suggestion".
    aria: String,
    likely: bool,
    current: bool,
}

/// A labelled section of the candidate list.
fn option_group(
    id: &'static str,
    label: &'static str,
    options: Vec<AnyElement>,
    muted: gpui::Hsla,
) -> AnyElement {
    div()
        .id(id)
        .role(Role::Group)
        .aria_label(label)
        .flex()
        .flex_col()
        .child(
            div()
                .px_2()
                .pt_1()
                .text_role(TextRole::Caption)
                .text_color(muted)
                .child(label),
        )
        .children(options)
        .into_any_element()
}

impl Render for PersonPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let focused = self.search_focused(window, cx);
        let mut options: Vec<AnyElement> = self
            .matches
            .shown
            .iter()
            .enumerate()
            .map(|(ix, &i)| {
                self.render_option(ix, &self.people[i], self.search_focused(window, cx), cx)
            })
            .collect();
        if self.matches.voice > 0 {
            let rest = options.split_off(self.matches.voice);
            let mut groups = vec![option_group(
                "person-picker-voices",
                "Closest voices",
                options,
                muted,
            )];
            if !rest.is_empty() {
                groups.push(option_group(
                    "person-picker-others",
                    "Everyone else",
                    rest,
                    muted,
                ));
            }
            options = groups;
        }
        if self.voice_target.is_some()
            && !self.voices_ready
            && !self.voice_failed
            && self.query.trim().is_empty()
        {
            if !options.is_empty() {
                options = vec![option_group(
                    "person-picker-others",
                    "Everyone else",
                    options,
                    muted,
                )];
            }
            options.insert(
                0,
                option_group(
                    "person-picker-voices",
                    "Closest voices",
                    vec![
                        div()
                            .id("person-picker-voice-placeholder")
                            .debug_selector(|| "person-picker-voice-placeholder".into())
                            .h(rems(2.75 * VOICE_ROWS as f32))
                            .px_2()
                            .child(kit::status_line(
                                "person-picker-comparing",
                                kit::StatusKind::Pending,
                                "Comparing voices…",
                                cx,
                            ))
                            .into_any_element(),
                    ],
                    muted,
                ),
            );
        }
        if let TypedName::Offer(text) = self.typed() {
            let label = format!("Add “{text}” as a new person");
            let highlighted = self.highlighted == Some(self.matches.shown.len());
            options.push(
                kit::list_row("person-picker-use-typed", highlighted, focused, cx)
                    .role(Role::ListBoxOption)
                    .aria_label(label.clone())
                    .aria_selected(highlighted)
                    .h(rems(2.75))
                    .items_center()
                    .child(crate::ui::icons::icon(crate::ui::icons::ADD))
                    .child(div().truncate().child(label))
                    .tooltip(kit::title_tooltip(
                        "Creates a new person and names this speaker.",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.typed() == TypedName::Offer(text.clone()) {
                            this.use_typed(cx);
                        }
                        cx.stop_propagation();
                    }))
                    .into_any_element(),
            );
        }
        let search = self.input.focus_handle(cx);
        let key_hint =
            |action: &dyn gpui::Action| Kbd::binding_for_action_in(action, &search, window);
        let keys = [key_hint(&MoveUp), key_hint(&MoveDown)];
        let enter = key_hint(&Enter {
            secondary: false,
            shift: false,
        });
        div()
            .id("person-picker")
            .key_context("PersonPicker")
            .role(Role::Group)
            .aria_label(self.label.clone())
            .aria_keyshortcuts("ArrowUp ArrowDown Enter Escape")
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                this.move_highlight(false, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                this.move_highlight(true, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                this.cancel(cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &gpui::base::actions::Cancel, _, cx| {
                this.cancel(cx);
                cx.stop_propagation();
            }))
            // Keys the picker handled never reach the host's list shortcuts.
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" => {
                        this.cancel(cx);
                        cx.stop_propagation();
                    }
                    "enter" => cx.stop_propagation(),
                    _ => {}
                }
            }))
            .child(Input::new(&self.input).aria_label(self.label.clone()))
            .when_some(self.error.as_ref(), |el, error| {
                el.child(kit::retry_line(
                    "person-picker-status",
                    crate::jobs::failure_text("Cannot read registered people.", error, ""),
                    cx,
                    |this, _, cx| this.reload(cx),
                ))
            })
            .when(self.error.is_none(), |el| {
                el.child(
                    text_line("person-picker-status", Role::Status, self.status())
                        .text_role(TextRole::Caption)
                        .text_color(muted),
                )
            })
            .when(self.voice_failed, |el| {
                el.child(kit::retry_line(
                    "person-picker-voice-error",
                    "Couldn't compare voices.",
                    cx,
                    |this, _, cx| this.request_voices(cx),
                ))
            })
            .children(self.highlight_text().map(|text| {
                text_line("person-picker-highlight", Role::Label, text)
                    .text_role(TextRole::Caption)
                    .text_color(muted)
            }))
            .child(
                div()
                    .id("person-picker-list")
                    .role(Role::ListBox)
                    .aria_label("People")
                    .flex()
                    .flex_col()
                    .when(!self.cancel_button, |list| {
                        list.max_h(gpui::rems(16.)).overflow_y_scroll()
                    })
                    .children(options),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .when(
                        focused && keys.iter().all(Option::is_some) && enter.is_some(),
                        |footer| {
                            footer.child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .children(keys.into_iter().flatten())
                                    .child(
                                        text_line("picker-move-hint", Role::Label, "Move")
                                            .text_role(TextRole::Caption),
                                    )
                                    .children(enter)
                                    .child(
                                        text_line("picker-choose-hint", Role::Label, "Choose")
                                            .text_role(TextRole::Caption),
                                    ),
                            )
                        },
                    )
                    .when(self.cancel_button, |row| {
                        row.child(
                            button(
                                "person-picker-cancel",
                                "Cancel",
                                true,
                                cx,
                                |this: &mut Self, _, cx| this.cancel(cx),
                            )
                            .accessibility_label("Cancel naming"),
                        )
                    }),
            )
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! A synthetic library shared by the picker, meeting and review tests.

    use std::path::Path;

    use broscribe_core::schema::{
        Assignment, Meeting, Person, ProcessingStatus, Segment, Source, Speaker, Suggestion,
    };
    use broscribe_core::{Library, MeetingId, PersonId, SpeakerId};
    use broscribe_platform::fakes::FakePaths;
    use broscribe_platform::resolve_paths;
    use chrono::DateTime;
    use gpui::{AppContext as _, Entity, TestAppContext};

    use crate::state::AppState;

    pub struct Synthetic {
        pub ana: PersonId,
        /// Same display name as `ana`, another email.
        pub ana_other: PersonId,
        /// Accented name with the alias `Zé`.
        pub jose: PersonId,
        /// The user, without a name.
        pub me: PersonId,
        /// A caption placeholder waiting for review.
        pub placeholder: PersonId,
        pub meeting: MeetingId,
    }

    pub fn sid(n: u32) -> SpeakerId {
        SpeakerId::new(n).unwrap()
    }

    /// People plus one done meeting with three speakers waiting for review:
    /// 1 unnamed, 2 suggested as José, 3 unnamed. Indexed.
    pub fn seed(lib: &Library) -> Synthetic {
        let person = |name: &str, emails: &[&str]| {
            let mut p = Person::new(PersonId::random(), name);
            p.emails = emails.iter().map(|e| (*e).to_owned()).collect();
            p
        };
        let ana = person("Ana Silva", &["ana@example.com"]);
        let ana_other = person("Ana Silva", &["ana.silva@other.example"]);
        let mut jose = person("José Pereira", &["jp@example.com"]);
        jose.aliases = vec!["Zé".into()];
        let mut me = person("", &[]);
        me.is_self = true;
        let mut placeholder = person("Caption Person", &[]);
        placeholder.needs_review = true;

        let started = DateTime::parse_from_rfc3339("2026-11-02T10:00:00Z").unwrap();
        let mut m = Meeting::new(MeetingId::random(), Source::Import, started);
        m.title = Some("Synthetic sync".into());
        m.duration_s = 60.0;
        m.processing.status = ProcessingStatus::Done;
        m.speakers = (1..=3)
            .map(|n| {
                let mut s = Speaker::new(sid(n), "t0");
                s.talk_s = 40.0 - f64::from(n) * 10.0;
                s
            })
            .collect();
        m.speakers[1].assignment = Assignment::Suggested;
        m.speakers[1].suggestion = Some(Suggestion {
            person_id: jose.id.clone(),
            score: 0.7,
        });
        m.segments = (1..=3)
            .map(|n| Segment {
                speech_overlap: Vec::new(),
                id: n,
                track: "t0".into(),
                speaker: Some(sid(n)),
                start: f64::from(n) * 5.0,
                end: f64::from(n) * 5.0 + 3.0,
                text: format!("line {n}"),
                words: Vec::new(),
            })
            .collect();
        lib.write(|tx| {
            for p in [&ana, &ana_other, &jose, &me, &placeholder] {
                tx.save_person(p)?;
            }
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)?;
            broscribe_core::index::Index::rebuild(tx)?;
            Ok(())
        })
        .unwrap();
        Synthetic {
            ana: ana.id,
            ana_other: ana_other.id,
            jose: jose.id,
            me: me.id,
            placeholder: placeholder.id,
            meeting: m.id,
        }
    }

    pub fn state(cx: &mut TestAppContext, dir: &Path) -> (Entity<AppState>, Library, Synthetic) {
        state_with_cli(cx, dir, dir.join("no-such-cli"))
    }

    /// [`state`] whose one-shot commands run `cli` (a synthetic script).
    pub fn state_with_cli(
        cx: &mut TestAppContext,
        dir: &Path,
        cli: std::path::PathBuf,
    ) -> (Entity<AppState>, Library, Synthetic) {
        let library = Library::open(dir.join("lib")).unwrap();
        let synthetic = seed(&library);
        let lib = library.clone();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir)).unwrap(),
                    Default::default(),
                    library,
                    cli,
                ),
                cx,
            )
        });
        // Naming queues confirm-samples jobs; no CLI child runs in tests.
        state.update(cx, |s, cx| s.set_local_jobs_allowed(false, cx));
        cx.update(gpui::init);
        (state, lib, synthetic)
    }

    /// Registered people on disk.
    pub fn people_count(lib: &Library) -> usize {
        lib.list_people().unwrap().len()
    }

    /// The default speaker-embedding model, which the test app state uses.
    pub fn voice_model() -> String {
        broscribe_core::config::Config::default()
            .profile
            .speaker_embedding
    }

    /// A synthetic 4-D voice: unit vector at cosine `c` to `[1, 0, 0, 0]`.
    pub fn voice(c: f32) -> Vec<f32> {
        vec![c, (1.0 - c * c).max(0.0).sqrt(), 0.0, 0.0]
    }

    /// Gives `person` one enrollment sample `vector` (no meeting).
    pub fn enroll(lib: &Library, person: &PersonId, vector: &[f32]) {
        use broscribe_core::schema::{
            Strength, Voiceprint, VoiceprintKind, VoiceprintSource, Voiceprints, encode_vector,
        };
        let mut prints = Voiceprints::empty(person.clone());
        prints.embeddings.push(Voiceprint {
            id: format!("synthetic-{person}"),
            model: voice_model(),
            dim: vector.len(),
            vector_b64: encode_vector(vector),
            source: VoiceprintSource {
                kind: VoiceprintKind::Enrollment,
                meeting_id: None,
                sha256: None,
                start: 0.0,
                end: 1.0,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: broscribe_core::schema::now(),
        });
        lib.write(|tx| tx.save_voiceprints(&prints)).unwrap();
    }

    /// Gives speaker `n` of `m` the centroid `vector` (default model).
    pub fn set_voice(m: &mut Meeting, n: u32, vector: &[f32]) {
        m.speaker_mut(sid(n)).unwrap().centroid = Some(broscribe_core::schema::Centroid::new(
            voice_model(),
            vector,
            3,
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use broscribe_core::schema::{Assignment, Centroid, Person, Suggestion};
    use gpui::TestAppContext;

    use super::fixtures::{Synthetic, enroll, state, voice, voice_model};
    use super::*;
    use crate::watch::Changes;

    fn summary(name: &str, aliases: &[&str], emails: &[&str], meetings: u32) -> PersonSummary {
        PersonSummary {
            id: PersonId::random(),
            name: name.into(),
            is_self: false,
            aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
            emails: emails.iter().map(|e| (*e).to_owned()).collect(),
            hue: 1,
            meetings,
            talk_s: 0.0,
        }
    }

    fn ids(people: &PeopleIndex, query: &str) -> Vec<PersonId> {
        search(people, query)
            .shown
            .into_iter()
            .map(|i| people[i].id.clone())
            .collect()
    }

    #[test]
    fn partial_alias_accent_and_email_queries_find_the_right_ids() {
        let mut me = summary("", &[], &[], 9);
        me.is_self = true;
        let people = index_people(vec![
            summary("Ana Silva", &[], &["ana@example.com"], 1),
            summary("Ana Silva", &[], &["ana.silva@other.example"], 4),
            summary("José Pereira", &["Zé"], &["jp@example.com"], 2),
            summary("Bruno", &[], &[], 0),
            me,
        ]);
        let id = |i: usize| people[i].id.clone();
        // Partial and case-insensitive; more meetings first among equals.
        assert_eq!(ids(&people, "an"), [id(1), id(0)]);
        assert_eq!(ids(&people, "SIL an"), [id(1), id(0)]);
        // Accents fold both ways, aliases and emails match.
        assert_eq!(ids(&people, "jose"), [id(2)]);
        assert_eq!(ids(&people, "Péreira"), [id(2)]);
        assert_eq!(ids(&people, "ze"), [id(2)]);
        assert_eq!(ids(&people, "other.example"), [id(1)]);
        assert_eq!(ids(&people, "ana@example"), [id(0)]);
        // The ID and the unnamed self.
        assert_eq!(ids(&people, id(3).as_str()), [id(3)]);
        assert_eq!(ids(&people, "me"), [id(4)]);
        assert_eq!(people[4].name, "Me");
        assert!(ids(&people, "nobody").is_empty());
        // An empty query lists everyone, busiest first.
        assert_eq!(ids(&people, " ")[0], id(4));
    }

    #[test]
    fn duplicate_display_names_stay_distinct() {
        let people = index_people(vec![
            summary("Ana Silva", &[], &["ana@example.com"], 1),
            summary("ana silva", &[], &[], 1),
            summary("Bruno", &[], &["b@example.com"], 1),
        ]);
        assert!(people[0].duplicate_name && people[1].duplicate_name);
        assert!(!people[2].duplicate_name);
        assert_ne!(people[0].id, people[1].id);
        assert!(people[0].detail().contains("ana@example.com"));
        assert!(people[0].detail().contains(people[0].id.as_str()));
        assert!(people[1].detail().contains(people[1].id.as_str()));
        assert!(!people[2].detail().contains(people[2].id.as_str()));
    }

    #[test]
    fn typed_name_is_hidden_for_registered_names_and_references() {
        let people = index_people(vec![summary("José Pereira", &["Zé"], &[], 1)]);
        assert_eq!(typed_name(&people, "  "), TypedName::Empty);
        assert_eq!(
            typed_name(&people, " jose  pereira "),
            TypedName::Registered("José Pereira".into())
        );
        assert_eq!(
            typed_name(&people, "ZE"),
            TypedName::Registered("José Pereira".into())
        );
        assert_eq!(typed_name(&people, "new@example.com"), TypedName::Reference);
        assert_eq!(typed_name(&people, "p-abcdef"), TypedName::Reference);
        // A partial name is not a registered one: offered only explicitly.
        assert_eq!(typed_name(&people, "José"), TypedName::Offer("José".into()));
    }

    type PickerFixture = (
        gpui::WindowHandle<PersonPicker>,
        Entity<AppState>,
        Library,
        Synthetic,
        Rc<RefCell<Vec<PersonPickerEvent>>>,
        Subscription,
        tempfile::TempDir,
    );

    fn open_picker(cx: &mut TestAppContext) -> PickerFixture {
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, synthetic) = state(cx, dir.path());
        let window = cx.add_window({
            let app = app.clone();
            |window, cx| PersonPicker::new(app, "Name for Speaker 1", window, cx)
        });
        let picker = window.update(cx, |_, _, cx| cx.entity()).unwrap();
        let events = Rc::new(RefCell::new(Vec::new()));
        let subscription = cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&picker, move |_, ev: &PersonPickerEvent, _| {
                events.borrow_mut().push(ev.clone())
            })
        });
        cx.run_until_parked();
        (window, app, lib, synthetic, events, subscription, dir)
    }

    #[gpui::test]
    fn typing_selects_the_first_existing_match_and_never_implicitly_creates(
        cx: &mut TestAppContext,
    ) {
        let (window, _app, _lib, synthetic, events, _sub, _dir) = open_picker(cx);
        window
            .update(cx, |picker, window, cx| {
                picker.set_query("jose", window, cx);
                assert_eq!(picker.highlighted(), Some(0));
            })
            .unwrap();
        cx.simulate_keystrokes(window.into(), "enter");
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            [PersonPickerEvent::Picked(PersonChoice::Existing(
                synthetic.jose
            ))]
        );

        window
            .update(cx, |picker, window, cx| {
                picker.set_query("Carla Nova", window, cx);
                assert_eq!(picker.highlighted(), None);
                assert_eq!(picker.status(), "Nobody matches “Carla Nova”.");
            })
            .unwrap();
        cx.simulate_keystrokes(window.into(), "enter");
        cx.run_until_parked();
        assert_eq!(events.borrow().len(), 1);
        window
            .update(cx, |picker, _, _| {
                assert_eq!(picker.status(), "Highlight a person with ↓ first.");
            })
            .unwrap();
        cx.simulate_keystrokes(window.into(), "down enter");
        cx.run_until_parked();
        assert_eq!(
            events.borrow().last(),
            Some(&PersonPickerEvent::Picked(PersonChoice::Typed(
                "Carla Nova".into()
            )))
        );
    }

    #[gpui::test]
    fn option_layout_and_tab_order_share_the_picker_search(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, _, synthetic) = state(cx, dir.path());
        let picker = Rc::new(RefCell::new(None));
        let capture = picker.clone();
        let (_, visual) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| PersonPicker::new(state, "Name for Speaker 1", window, cx));
            *capture.borrow_mut() = Some(view.clone());
            gpui::component::Root::new(view, window, cx)
        });
        let picker = picker.borrow().clone().unwrap();
        visual.run_until_parked();
        picker.update_in(visual, |picker, _, cx| {
            let mut voice = vm(&synthetic.jose, 0.74);
            voice.likely = true;
            voice.current_suggestion = true;
            picker.apply_voices(
                picker.voice_generation,
                Ok(vec![voice, vm(&synthetic.ana, 0.70)]),
                cx,
            );
        });
        visual.refresh().unwrap();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            let list = tree.find("person-picker-list");
            assert_eq!(tree.read(list), ("ListBox", "People"));
            let first = tree
                .nodes
                .iter()
                .find(|node| node.role == "ListBoxOption")
                .unwrap();
            assert!(
                first
                    .name
                    .contains("voice similarity 0.74, likely, current suggestion")
            );
        });
        let first = visual.debug_bounds("person-picker-option-0").unwrap();
        let top_score = visual.debug_bounds("picker-voice-score-0").unwrap();
        let second_score = visual.debug_bounds("picker-voice-score-1").unwrap();
        assert_eq!(top_score.right(), second_score.right());
        visual.update(|window, _| assert_eq!(first.size.height, window.rem_size() * 2.75));

        // Suggested and Likely are both unconfirmed: dashed, on no fill.
        for badge in ["picker-likely-0", "picker-suggested-0"] {
            let bounds = visual.debug_bounds(badge).unwrap();
            assert!(bounds.top() >= first.top() && bounds.bottom() <= first.bottom());
            visual.update(|_, cx| {
                let theme = cx.theme();
                let mut badge = kit::badge(BadgeTone::Suggested, "", cx);
                let style = badge.style();
                assert_eq!(style.background, Some(gpui::transparent_black().into()));
                assert_eq!(style.border_color, Some(theme.input));
                assert_eq!(style.border_style, Some(gpui::BorderStyle::Dashed));
            });
        }

        // A partial query offers both existing and typed options, but neither takes a Tab.
        picker.update_in(visual, |picker, window, cx| {
            picker.set_query("Jose", window, cx)
        });
        visual.refresh().unwrap();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            let list = tree.find("person-picker-list");
            let options = tree
                .subtree(list)
                .into_iter()
                .filter(|&node| tree.read(node).0 == "ListBoxOption")
                .collect::<Vec<_>>();
            assert_eq!(
                options.last().copied(),
                Some(tree.find("person-picker-use-typed"))
            );
            assert_eq!(
                tree.read(*options.last().unwrap()),
                ("ListBoxOption", "Add “Jose” as a new person")
            );
        });
        visual.simulate_keystrokes("tab");
        visual.update(|window, _| {
            let cancel = gpui::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| node.label() == Some("Cancel naming"))
                .unwrap();
            assert_eq!(cancel.focused(), Some(true));
        });
        visual.simulate_keystrokes("shift-tab");
        assert!(picker.update_in(visual, |picker, window, cx| {
            picker.search_focused(window, cx)
        }));
    }

    #[gpui::test]
    fn closest_voices_reserve_three_rows_and_retry_after_failure(cx: &mut TestAppContext) {
        let (window, _, _, synthetic, _, _, _dir) = open_picker(cx);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let generation = window
            .update(&mut visual, |picker, _, cx| {
                picker.voice_target = Some(target(&synthetic, &voice(1.)));
                picker.voice_generation += 1;
                cx.notify();
                picker.voice_generation
            })
            .unwrap();
        visual.refresh().unwrap();
        let reserved = visual
            .debug_bounds("person-picker-voice-placeholder")
            .unwrap();
        visual.update(|window, cx| {
            assert_eq!(reserved.size.height, window.rem_size() * 8.25);
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("person-picker-voices")),
                ("Group", "Closest voices")
            );
            assert_eq!(
                tree.read(tree.find("person-picker-comparing")),
                ("Status", "Comparing voices…")
            );
        });
        window
            .update(&mut visual, |picker, _, cx| {
                assert!(!picker.apply_voices(
                    generation - 1,
                    Ok(vec![vm(&synthetic.ana, 0.9)]),
                    cx
                ));
                assert!(picker.apply_voices(generation, Err("io".into()), cx));
            })
            .unwrap();
        visual.refresh().unwrap();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(tree.all("person-picker-voices").is_empty());
            assert_eq!(
                tree.read(tree.find("person-picker-voice-error")),
                ("Alert", "Couldn't compare voices.")
            );
        });
        let retry = visual
            .debug_bounds("person-picker-voice-error-retry")
            .unwrap();
        visual.simulate_click(retry.center(), Default::default());
        assert!(
            window
                .update(&mut visual, |picker, _, _| picker.voice_generation
                    > generation)
                .unwrap()
        );
    }

    /// ↓ in the search field names the highlighted person in its own
    /// non-live label while focus stays in the field (which is not an
    /// ancestor of the options, so no active descendant is claimed); the live
    /// status line keeps its words.
    #[gpui::test]
    fn arrow_keys_name_the_highlight_in_a_label_not_the_live_status(cx: &mut TestAppContext) {
        use crate::ui::spoken_log::{self, Spoken};

        let (window, _app, _lib, _s, _events, _sub, _dir) = open_picker(cx);
        let named = |cx: &mut TestAppContext| -> Vec<Spoken> {
            window
                .update(cx, |picker, window, cx| {
                    spoken_log::while_building(|| Render::render(picker, window, cx)).1
                })
                .unwrap()
        };
        let find = |named: &[Spoken], id: &str| named.iter().find(|(i, _, _)| i == id).cloned();
        let live = |named: &[Spoken]| -> Vec<String> {
            named
                .iter()
                .filter(|(_, role, _)| matches!(role, Role::Status | Role::Alert))
                .map(|(id, _, _)| id.clone())
                .collect()
        };
        let name_of = |ix: usize, cx: &mut TestAppContext| {
            window
                .update(cx, |picker, _, _| {
                    picker.candidates().nth(ix).unwrap().name.clone()
                })
                .unwrap()
        };
        assert!(
            window
                .update(cx, |picker, window, cx| picker.search_focused(window, cx))
                .unwrap()
        );
        let before = named(cx);
        let status = find(&before, "person-picker-status").expect("the status line");
        assert_eq!(status.1, Role::Status);
        assert_eq!(find(&before, "person-picker-highlight"), None);

        for ix in 0..2 {
            cx.simulate_keystrokes(window.into(), "down");
            cx.run_until_parked();
            assert_eq!(
                window.update(cx, |p, _, _| p.highlighted()).unwrap(),
                Some(ix)
            );
            let after = named(cx);
            let (_, role, text) =
                find(&after, "person-picker-highlight").expect("the highlight label");
            assert_eq!(role, Role::Label, "not live");
            let name = name_of(ix, cx);
            assert_eq!(text, name);
            assert_eq!(find(&after, "person-picker-status"), Some(status.clone()));
            assert_eq!(live(&after), ["person-picker-status"]);
        }
        assert!(
            window
                .update(cx, |picker, window, cx| picker.search_focused(window, cx))
                .unwrap(),
            "typing goes on in the field"
        );
        // Typing puts the highlight on the first matching person.
        window
            .update(cx, |picker, window, cx| picker.set_query("ana", window, cx))
            .unwrap();
        assert!(find(&named(cx), "person-picker-highlight").is_some());
    }

    /// PPL-34: typed-name hints are part of the picker status.
    #[gpui::test]
    fn typed_name_hints_are_part_of_the_status(cx: &mut TestAppContext) {
        use crate::ui::spoken_log::{self, has, node};

        let (window, _app, _lib, _s, _events, _sub, _dir) = open_picker(cx);
        window
            .update(cx, |picker, window, cx| {
                for (query, hint) in [
                    ("jose pereira", "José Pereira is already in the list."),
                    (
                        "new@example.com",
                        "Emails and IDs only match people already in the list.",
                    ),
                ] {
                    picker.set_query(query, window, cx);
                    let (_, named) =
                        spoken_log::while_building(|| Render::render(picker, window, cx));
                    assert_eq!(
                        node(&named, "person-picker-status"),
                        (Role::Status, hint),
                        "{query}"
                    );
                }
                picker.set_query("Joana Nova", window, cx);
                let (_, named) = spoken_log::while_building(|| Render::render(picker, window, cx));
                assert!(!has(&named, "person-picker-typed-hint"));
            })
            .unwrap();
    }

    #[gpui::test]
    fn typed_name_needs_the_explicit_action(cx: &mut TestAppContext) {
        let (window, _app, _lib, s, events, _sub, _dir) = open_picker(cx);
        window
            .update(cx, |picker, window, cx| {
                // Registered under accent folding: hidden, the person is listed.
                picker.set_query("jose pereira", window, cx);
                assert_eq!(picker.typed(), TypedName::Registered("José Pereira".into()));
                assert_eq!(picker.candidates().next().unwrap().id, s.jose);
                picker.use_typed(cx);
                picker.set_query("Joana Nova", window, cx);
                assert_eq!(picker.typed(), TypedName::Offer("Joana Nova".into()));
                picker.use_typed(cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            [PersonPickerEvent::Picked(PersonChoice::Typed(
                "Joana Nova".into()
            ))]
        );
    }

    #[gpui::test]
    fn placeholders_are_listed_and_people_changes_refresh(cx: &mut TestAppContext) {
        let (window, app, lib, s, _events, _sub, _dir) = open_picker(cx);
        window
            .update(cx, |picker, window, cx| {
                picker.set_query("me", window, cx);
                assert_eq!(picker.candidates().next().unwrap().id, s.me);
                picker.set_query("caption", window, cx);
                assert_eq!(picker.candidates().next().unwrap().id, s.placeholder);
                // A hexadecimal fragment can match a random person ID.
                picker.set_query("Bea Nova", window, cx);
                assert_eq!(picker.candidates().count(), 0);
                // A stale load changes nothing.
                let stale = picker.generation;
                picker.reload(cx);
                assert!(!picker.apply_people(stale, Ok(PeopleIndex::default()), cx));
            })
            .unwrap();
        cx.run_until_parked();
        let bea = Person::new(PersonId::random(), "Bea Nova");
        lib.write(|tx| {
            tx.save_person(&bea)?;
            broscribe_core::index::Index::rebuild(tx)?;
            Ok(())
        })
        .unwrap();
        app.update(cx, |s, cx| {
            s.library_changed(
                &Changes {
                    people: true,
                    ..Changes::default()
                },
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |picker, _, _| {
                assert!(
                    picker
                        .candidates()
                        .map(|p| &p.id)
                        .eq(std::iter::once(&bea.id))
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn unknown_catalog_cannot_submit_a_typed_name(cx: &mut TestAppContext) {
        let (window, _app, _lib, _s, events, _sub, _dir) = open_picker(cx);
        window
            .update(cx, |picker, window, cx| {
                picker.set_query("New Person Example", window, cx);
                picker.reload(cx);
                picker.use_typed(cx);
                let generation = picker.generation;
                picker.apply_people(
                    generation,
                    Err(Error::InvalidInput("synthetic catalog unavailable".into())),
                    cx,
                );
                picker.use_typed(cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(events.borrow().is_empty());
    }

    #[gpui::test]
    fn stale_rendered_identity_never_selects_its_replacement(cx: &mut TestAppContext) {
        let (window, _app, _lib, s, events, _sub, _dir) = open_picker(cx);
        let replacement = summary("Ana Replacement", &[], &[], 0);
        let new_id = replacement.id.clone();
        window
            .update(cx, |picker, window, cx| {
                picker.set_query("ana", window, cx);
                assert!(picker.candidates().any(|person| person.id == s.ana));
                picker.apply_people(picker.generation, Ok(index_people(vec![replacement])), cx);
                // A click queued by the old frame cannot use the new row at index zero.
                picker.pick_id(&s.ana, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(events.borrow().is_empty());
        window
            .update(cx, |picker, _, cx| picker.pick_id(&new_id, cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            [PersonPickerEvent::Picked(PersonChoice::Existing(new_id))]
        );
    }

    #[gpui::test]
    fn catalog_reloads_keep_the_highlight_on_its_person(cx: &mut TestAppContext) {
        let (window, app, lib, s, events, _sub, _dir) = open_picker(cx);
        let people_changed = |app: &Entity<AppState>, cx: &mut TestAppContext| {
            app.update(cx, |st, cx| {
                st.library_changed(
                    &Changes {
                        people: true,
                        ..Changes::default()
                    },
                    cx,
                )
            })
        };
        let before = window
            .update(cx, |picker, _, _| {
                let ix = picker.candidates().position(|p| p.id == s.jose).unwrap();
                picker.highlighted = Some(ix);
                ix
            })
            .unwrap();
        // Someone is added, someone removed: rows reorder under the highlight.
        lib.write(|tx| {
            for name in ["Aaron Early", "Abel Early", "Ada Early"] {
                tx.save_person(&Person::new(PersonId::random(), name))?;
            }
            broscribe_core::index::Index::rebuild(tx)
        })
        .unwrap();
        ops::with_index(&lib, |tx, index| {
            ops::forget_person(tx, index, s.ana_other.as_str())
        })
        .unwrap();
        people_changed(&app, cx);
        cx.run_until_parked();
        window
            .update(cx, |picker, _, cx| {
                assert!(picker.candidates().any(|p| p.name == "Aaron Early"));
                assert!(picker.candidates().all(|p| p.id != s.ana_other));
                let ix = picker.highlighted.unwrap();
                assert_eq!(picker.candidates().nth(ix).unwrap().id, s.jose);
                assert_ne!(ix, before, "José's row moved");
                picker.confirm_highlighted(cx);
            })
            .unwrap();
        cx.run_until_parked();
        // The highlighted person is removed: the highlight clears.
        ops::with_index(&lib, |tx, index| {
            ops::forget_person(tx, index, s.jose.as_str())
        })
        .unwrap();
        people_changed(&app, cx);
        cx.run_until_parked();
        window
            .update(cx, |picker, _, cx| {
                assert_eq!(picker.highlighted, None);
                picker.confirm_highlighted(cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            [PersonPickerEvent::Picked(PersonChoice::Existing(
                s.jose.clone()
            ))],
            "Enter never chose anyone else"
        );
    }

    fn vm(person: &PersonId, score: f32) -> VoiceMatch {
        VoiceMatch {
            person_id: person.clone(),
            score,
            likely: false,
            current_suggestion: false,
        }
    }

    #[test]
    fn closest_voices_lead_once_and_queries_keep_search_order() {
        let people = index_people(vec![
            summary("Ana Silva", &[], &[], 1),
            summary("Bruno", &[], &[], 5),
            summary("Carla", &[], &[], 3),
            summary("Dora", &[], &[], 0),
        ]);
        let id = |i: usize| people[i].id.clone();
        let voices = [
            // Not a listed person (removed meanwhile): skipped.
            vm(&PersonId::random(), 0.99),
            vm(&id(3), 0.9),
            vm(&id(0), 0.8),
            vm(&id(2), 0.7),
            vm(&id(1), 0.6),
        ];
        let m = compose(&people, "", &voices);
        assert_eq!(m.voice, VOICE_ROWS);
        assert_eq!(m.total, 4);
        let shown: Vec<PersonId> = m.shown.iter().map(|&i| id(i)).collect();
        // The three closest, then everyone else in search order, once.
        assert_eq!(shown, [id(3), id(0), id(2), id(1)]);
        assert_eq!(compose(&people, " ", &voices).shown, m.shown);
        // A query keeps search order and has no section.
        assert_eq!(compose(&people, "a", &voices), search(&people, "a"));
        assert_eq!(compose(&people, "", &[]), search(&people, ""));
        assert_eq!(voice_score(0.874), "voice 0.87");
    }

    /// A keystroke orders only the people it shows: the shown candidates
    /// and the match count are exactly those of ranking every match.
    #[test]
    fn partial_selection_shows_what_a_full_ranking_would() {
        let first = ["Ana", "Anabel", "Bruno"];
        let people = index_people(
            (0..60usize)
                .map(|i| {
                    summary(
                        &format!("Synthetic {} {i}", first[i % 3]),
                        &[],
                        &[],
                        (i * 7 % 11) as u32,
                    )
                })
                .collect(),
        );
        for query in ["", " ", "ana", "synthetic", "bru", "anabel 4", "nobody"] {
            let tokens = query_tokens(query);
            let folded = tokens.join(" ");
            let mut every: Vec<(u8, Reverse<u32>, usize)> = people
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    p.rank(&tokens, &folded)
                        .map(|r| (r, Reverse(p.meetings), i))
                })
                .collect();
            every.sort_unstable();
            let m = search(&people, query);
            assert_eq!(m.total, every.len(), "{query:?}");
            let expected: Vec<usize> = every.iter().take(MAX_RESULTS).map(|h| h.2).collect();
            assert_eq!(m.shown, expected, "{query:?}");
        }
        assert_eq!(
            typed_name(&people, " SYNTHETIC ana 0 "),
            TypedName::Registered("Synthetic Ana 0".into())
        );
        assert!(people.iter().all(|p| !p.duplicate_name));
    }

    /// Speaker 1 of the synthetic meeting with voice `centroid`, suggested
    /// as José.
    fn target(s: &Synthetic, centroid: &[f32]) -> Arc<VoiceTarget> {
        let mut speaker = Speaker::new(super::fixtures::sid(1), "t0");
        speaker.centroid = Some(Centroid::new(voice_model(), centroid, 3));
        speaker.assignment = Assignment::Suggested;
        speaker.suggestion = Some(Suggestion {
            person_id: s.jose.clone(),
            score: 0.8,
        });
        let mut meeting = broscribe_core::schema::Meeting::new(
            s.meeting.clone(),
            broscribe_core::schema::Source::Recording,
            broscribe_core::schema::now(),
        );
        meeting.speakers = vec![speaker];
        Arc::new(VoiceTarget {
            meeting,
            speaker_id: super::fixtures::sid(1),
            roster: BTreeSet::new(),
        })
    }

    fn closest(
        window: &gpui::WindowHandle<PersonPicker>,
        cx: &mut TestAppContext,
    ) -> Vec<PersonId> {
        window
            .update(cx, |picker, _, _| {
                picker.closest_voices().map(|p| p.id.clone()).collect()
            })
            .unwrap()
    }

    #[gpui::test]
    fn late_voices_keep_the_highlight_and_enter_picks_by_id(cx: &mut TestAppContext) {
        let (window, _app, lib, s, events, _sub, _dir) = open_picker(cx);
        enroll(&lib, &s.ana, &voice(1.0));
        enroll(&lib, &s.jose, &voice(0.8));
        window
            .update(cx, |picker, _, cx| {
                // Someone without a voice: Ana and José arrive above them.
                let ix = picker.candidates().position(|p| p.id == s.me).unwrap();
                picker.highlighted = Some(ix);
                picker.set_voice_target(Some(target(&s, &voice(1.0))), cx);
                assert_eq!(picker.closest_voices().count(), 0, "not computed yet");
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(closest(&window, cx), [s.ana.clone(), s.jose.clone()]);
        window
            .update(cx, |picker, window, cx| {
                let shown: Vec<&PersonId> = picker.candidates().map(|p| &p.id).collect();
                let unique: BTreeSet<&PersonId> = shown.iter().copied().collect();
                assert_eq!(unique.len(), shown.len(), "nobody listed twice");
                let ix = picker.highlighted.unwrap();
                assert_eq!(picker.candidates().nth(ix).unwrap().id, s.me);
                let jose = picker.candidates().nth(1).unwrap().clone();
                let v = picker.option_voice(1, &jose).unwrap();
                assert_eq!(v.score, "voice 0.80");
                assert_eq!(v.aria, "voice similarity 0.80, current suggestion");
                assert!(v.current && !v.likely, "no thresholds: no badge");
                assert!(
                    !picker.status().contains("highlighted"),
                    "highlight changes stay out of the live status line: {}",
                    picker.status()
                );
                // Below the section nobody shows a score.
                let me = picker.candidates().nth(ix).unwrap().clone();
                assert!(ix >= 2 && picker.option_voice(ix, &me).is_none());
                picker.confirm_highlighted(cx);
                // With a query, matching people keep search order and their score.
                picker.set_query("jose", window, cx);
                assert_eq!(picker.closest_voices().count(), 0);
                let jose = picker.candidates().next().unwrap().clone();
                assert_eq!(jose.id, s.jose);
                assert_eq!(picker.option_voice(0, &jose).unwrap().score, "voice 0.80");
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            [PersonPickerEvent::Picked(PersonChoice::Existing(
                s.me.clone()
            ))]
        );
    }

    #[gpui::test]
    fn stale_voices_after_forget_voice_show_no_score(cx: &mut TestAppContext) {
        let (window, app, lib, s, _events, _sub, _dir) = open_picker(cx);
        enroll(&lib, &s.ana, &voice(1.0));
        enroll(&lib, &s.jose, &voice(0.8));
        window
            .update(cx, |picker, _, cx| {
                picker.set_voice_target(Some(target(&s, &voice(1.0))), cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(closest(&window, cx), [s.ana.clone(), s.jose.clone()]);
        // New thresholds: scores clear at once, then Ana gets the badge.
        app.update(cx, |st, cx| {
            let mut config = st.config().clone();
            config.identity.insert(
                voice_model(),
                Thresholds {
                    auto_name: Some(0.9),
                    suggest: Some(0.5),
                    margin: Some(0.1),
                    merge: None,
                    auto_merge: None,
                },
            );
            st.set_config(config, cx);
        });
        assert!(closest(&window, cx).is_empty());
        cx.run_until_parked();
        window
            .update(cx, |picker, _, _| {
                let ana = picker.candidates().next().unwrap().clone();
                assert_eq!(ana.id, s.ana);
                assert!(picker.option_voice(0, &ana).unwrap().likely);
                assert_eq!(
                    picker.option_voice(0, &ana).unwrap().aria,
                    "voice similarity 1.00, likely"
                );
            })
            .unwrap();

        let stale = window
            .update(cx, |picker, _, _| picker.voice_generation)
            .unwrap();
        ops::with_index(&lib, |tx, index| {
            ops::forget_voice(tx, index, s.ana.as_str())
        })
        .unwrap();
        app.update(cx, |st, cx| {
            st.library_changed(
                &Changes {
                    people: true,
                    ..Changes::default()
                },
                cx,
            )
        });
        window
            .update(cx, |picker, _, cx| {
                assert!(picker.voice_of(&s.ana).is_none(), "cleared at once");
                // A late result of the old request changes nothing.
                assert!(!picker.apply_voices(stale, Ok(vec![vm(&s.ana, 0.99)]), cx));
                assert!(picker.voice_of(&s.ana).is_none());
                assert_eq!(picker.closest_voices().count(), 0);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(closest(&window, cx), std::slice::from_ref(&s.jose));
        window
            .update(cx, |picker, _, _| {
                assert!(picker.voice_of(&s.ana).is_none())
            })
            .unwrap();
    }

    #[gpui::test]
    fn voices_of_people_no_longer_listed_are_hidden(cx: &mut TestAppContext) {
        let (window, app, lib, s, _events, _sub, _dir) = open_picker(cx);
        enroll(&lib, &s.ana, &voice(1.0));
        enroll(&lib, &s.jose, &voice(0.8));
        window
            .update(cx, |picker, _, cx| {
                picker.set_voice_target(Some(target(&s, &voice(1.0))), cx);
                // A result naming someone the loaded list no longer has.
                let generation = picker.voice_generation;
                assert!(picker.apply_voices(
                    generation,
                    Ok(vec![vm(&PersonId::random(), 0.99), vm(&s.jose, 0.8)]),
                    cx
                ));
                assert!(
                    picker
                        .closest_voices()
                        .map(|p| &p.id)
                        .eq(std::iter::once(&s.jose))
                );
                // An error hides the section; search keeps working.
                picker.request_voices(cx);
                let generation = picker.voice_generation;
                assert!(picker.apply_voices(generation, Err("unreadable".into()), cx));
                assert_eq!(picker.closest_voices().count(), 0);
                assert!(picker.candidates().count() > 0);
                picker.request_voices(cx);
            })
            .unwrap();
        // Ana is removed before the request completes.
        ops::with_index(&lib, |tx, index| {
            ops::forget_person(tx, index, s.ana.as_str())
        })
        .unwrap();
        app.update(cx, |st, cx| {
            st.library_changed(
                &Changes {
                    people: true,
                    ..Changes::default()
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(closest(&window, cx), std::slice::from_ref(&s.jose));
        window
            .update(cx, |picker, _, _| {
                assert!(picker.candidates().all(|p| p.id != s.ana));
            })
            .unwrap();
    }
}
