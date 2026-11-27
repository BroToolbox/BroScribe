//! Library file schema: `meeting.json` (v2), `person.md` frontmatter and
//! `voiceprints.json` (v1). Unknown fields are ignored on read.
//!
//! `meeting.json` v2 adds `tags_removed`, `enrichment` and the recording's
//! retention fields. v1 files are read as is (every new field has a default)
//! and migrated in memory by [`migrate_meeting`]; saving writes v2.

use std::collections::BTreeMap;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, FixedOffset, NaiveDate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, MeetingId, PersonId, Result, SpeakerId};

/// Version of `meeting.json` written by this crate.
pub const SCHEMA_VERSION: u32 = 2;

/// Version of `voiceprints.json` written by this crate.
pub const VOICEPRINTS_SCHEMA_VERSION: u32 = 1;

/// Files without a `schema_version` predate versioning: v1.
fn v1() -> u32 {
    1
}

/// `schema` error when a `meeting.json` was written by a newer BroScribe.
pub fn check_schema_version(version: u32) -> Result<()> {
    check_version(version, SCHEMA_VERSION)
}

/// `schema` error when a `voiceprints.json` was written by a newer BroScribe.
pub fn check_voiceprints_schema_version(version: u32) -> Result<()> {
    check_version(version, VOICEPRINTS_SCHEMA_VERSION)
}

fn check_version(version: u32, supported: u32) -> Result<()> {
    if version > supported {
        return Err(Error::Schema(format!(
            "written by a newer BroScribe (schema {version} > {supported})"
        )));
    }
    Ok(())
}

/// Brings a `meeting.json` read from disk to [`SCHEMA_VERSION`] in memory;
/// `schema` error if a newer BroScribe wrote it. v1 → v2 only adds fields
/// with defaults, so the record is kept and its version raised.
pub fn migrate_meeting(mut meeting: Meeting) -> Result<Meeting> {
    check_schema_version(meeting.schema_version)?;
    meeting.schema_version = SCHEMA_VERSION;
    Ok(meeting)
}

/// Current local time with its UTC offset.
pub fn now() -> DateTime<FixedOffset> {
    chrono::Local::now().fixed_offset()
}

/// Provenance of a meeting title; legacy file-derived titles remain replaceable by calendar data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    #[default]
    File,
    User,
    Calendar,
}

fn file_title_source() -> Option<TitleSource> {
    Some(TitleSource::File)
}

/// Precedence of a title: user over calendar over file; between user titles
/// a timestamped explicit edit beats a legacy one, and the later edit wins.
fn title_rank(
    source: Option<TitleSource>,
    set_at: Option<DateTime<FixedOffset>>,
) -> (u8, Option<DateTime<FixedOffset>>) {
    match source.unwrap_or_default() {
        TitleSource::User => (2, set_at),
        TitleSource::Calendar => (1, None),
        TitleSource::File => (0, None),
    }
}

/// `meeting.json`, the canonical meeting record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meeting {
    #[serde(default = "v1")]
    pub schema_version: u32,
    pub id: MeetingId,
    pub source: Source,
    /// The other tool's ID for imports from a manifest.
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default = "file_title_source")]
    pub title_source: Option<TitleSource>,
    /// When the user last set the title explicitly (`title_source = user`);
    /// absent for titles typed before this was recorded. Between two user
    /// titles the newer explicit edit wins (see [`Meeting::offer_user_title`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_set_at: Option<DateTime<FixedOffset>>,
    pub started_at: DateTime<FixedOffset>,
    #[serde(default)]
    pub duration_s: f64,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub setup: Option<String>,
    #[serde(default)]
    pub languages: Vec<LanguageSpan>,
    #[serde(default)]
    pub bookmarks: Vec<Bookmark>,
    #[serde(default)]
    pub notes: Vec<Note>,
    /// Note on how the language choice affected recognition.
    #[serde(default)]
    pub language_effective: Option<String>,
    #[serde(default)]
    pub participants: Vec<Participant>,
    /// Tags the user added.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Tags the user removed: hidden even when enrichment proposes them
    /// (see [`Meeting::effective_tags`]).
    #[serde(default)]
    pub tags_removed: Vec<String>,
    #[serde(default)]
    pub hotwords: Vec<String>,
    #[serde(default)]
    pub recording: Option<Recording>,
    #[serde(default)]
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub speakers: Vec<Speaker>,
    /// Identifies the speaker numbering of the current analysis. Learned
    /// samples record it next to their speaker ID, which counts only while
    /// the two agree (see `ops::migrate_sample_speakers`); empty until the
    /// first renumbering, and in meetings older than this field.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub speaker_namespace: String,
    /// Over-split speakers shown as one (design §6.4): a display overlay
    /// that never rewrites speakers, segments or samples. A speaker is in at
    /// most one group; read through `ops::effective_speaker`. Omitted when
    /// empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub speaker_groups: Vec<SpeakerGroup>,
    /// Speaker pairs (sorted) the user separated: never grouped
    /// automatically nor suggested as duplicates. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_same: Vec<[SpeakerId; 2]>,
    #[serde(default)]
    pub segments: Vec<Segment>,
    /// "Only this line" corrections: the person the user attributed one
    /// transcript segment to, overriding its speaker's person for that
    /// segment only (see `ops::effective_attribution`). Keyed by the
    /// segment's fingerprint, never its ID; omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub utterance_people: Vec<UtterancePerson>,
    #[serde(default)]
    pub models: ModelsUsed,
    #[serde(default)]
    pub processing: Processing,
    /// LLM enrichment; `None` until a provider ran.
    #[serde(default)]
    pub enrichment: Option<Enrichment>,
    /// Results of meeting templates (design §8.4), one per template ID, in
    /// the order they were first produced; omitted when empty. They outlive
    /// the template definitions they came from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub template_results: Vec<TemplateResult>,
    /// One replaceable predecessor per generation target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_enrichment: Option<Enrichment>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub previous_template_results: BTreeMap<String, TemplateResult>,
    /// Last published run per generation job. A queue write can fail after
    /// its result was saved; cancellation still has to see publication.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub published_generation_runs: BTreeMap<String, String>,
    /// References enrichment or reprocessing could not map (e.g. ambiguous
    /// speakers, old confirmations without a new speaker) that the user
    /// should check; omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_notes: Vec<ReviewNote>,
    /// The user's live-preview attribution decisions carried into this
    /// meeting with how final processing reconciled each; omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub live_attributions: Vec<crate::live_identity::LiveAttribution>,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

impl Meeting {
    /// An empty record with schema defaults; `created_at`/`updated_at` are now.
    pub fn new(id: MeetingId, source: Source, started_at: DateTime<FixedOffset>) -> Self {
        let now = now();
        Self {
            schema_version: SCHEMA_VERSION,
            id,
            source,
            source_id: None,
            title: None,
            title_source: Some(TitleSource::File),
            title_set_at: None,
            started_at,
            duration_s: 0.0,
            app: None,
            setup: None,
            languages: Vec::new(),
            bookmarks: Vec::new(),
            notes: Vec::new(),
            language_effective: None,
            participants: Vec::new(),
            tags: Vec::new(),
            tags_removed: Vec::new(),
            hotwords: Vec::new(),
            recording: None,
            tracks: Vec::new(),
            speakers: Vec::new(),
            speaker_namespace: String::new(),
            speaker_groups: Vec::new(),
            not_same: Vec::new(),
            segments: Vec::new(),
            utterance_people: Vec::new(),
            models: ModelsUsed::default(),
            processing: Processing::default(),
            enrichment: None,
            template_results: Vec::new(),
            previous_enrichment: None,
            previous_template_results: BTreeMap::new(),
            published_generation_runs: BTreeMap::new(),
            review_notes: Vec::new(),
            live_attributions: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Records an explicitly supplied title now, which calendar matching and
    /// detected titles must not replace.
    pub fn set_user_title(&mut self, title: Option<String>) {
        self.title = title;
        self.title_source = Some(TitleSource::User);
        self.title_set_at = Some(now());
    }

    /// Applies a user title set at `at` unless the meeting holds a newer
    /// explicit user title: a user title beats calendar and file titles, a
    /// timestamped one beats a legacy one without a timestamp, and between
    /// two timestamped ones the later edit wins. Returns whether it applied.
    pub fn offer_user_title(&mut self, title: &str, at: DateTime<FixedOffset>) -> bool {
        if title_rank(self.title_source, self.title_set_at)
            > title_rank(Some(TitleSource::User), Some(at))
        {
            return false;
        }
        self.title = Some(title.to_owned());
        self.title_source = Some(TitleSource::User);
        self.title_set_at = Some(at);
        true
    }

    /// Keeps the stronger of this meeting's title and `other`'s (a newer
    /// analysis pass reconciling with the one it replaces): user over
    /// calendar over file, the later explicit user edit between two user
    /// titles. An equal non-file title is taken from `other`, the title the
    /// user has seen so far.
    pub fn keep_stronger_title(&mut self, other: &Meeting) {
        let mine = title_rank(self.title_source, self.title_set_at);
        let theirs = title_rank(other.title_source, other.title_set_at);
        let other_source = other.title_source.unwrap_or_default();
        if theirs > mine || (theirs == mine && other_source != TitleSource::File) {
            self.title.clone_from(&other.title);
            self.title_source = other.title_source;
            self.title_set_at = other.title_set_at;
        }
    }

    /// People whose attendance is supported independently of voice/caption matching.
    pub fn roster_person_ids(&self) -> std::collections::BTreeSet<PersonId> {
        self.participants
            .iter()
            .filter(|p| {
                p.evidence
                    .iter()
                    .any(|e| matches!(e.as_str(), "roster" | "calendar" | "user"))
            })
            .filter_map(|p| p.person_id.clone())
            .collect()
    }

    pub fn speaker(&self, id: SpeakerId) -> Option<&Speaker> {
        self.speakers.iter().find(|s| s.id == id)
    }

    pub fn speaker_mut(&mut self, id: SpeakerId) -> Option<&mut Speaker> {
        self.speakers.iter_mut().find(|s| s.id == id)
    }

    /// Deterministic hash of the grouping overlay (`speaker_groups` and
    /// `not_same`); empty without either. Naming sessions capture it so a
    /// write prepared before a regrouping is refused (`ops::SpeakerFence`).
    /// Independent of `speaker_namespace`, which tracks the raw numbering.
    pub fn grouping_revision(&self) -> String {
        if self.speaker_groups.is_empty() && self.not_same.is_empty() {
            return String::new();
        }
        let mut groups: Vec<&SpeakerGroup> = self.speaker_groups.iter().collect();
        groups.sort_by_key(|g| g.kept);
        let mut pairs = self.not_same.clone();
        pairs.sort();
        let mut h = Sha256::new();
        for g in groups {
            h.update(g.id.as_bytes());
            h.update(b"\t");
            h.update(g.kept.number().to_le_bytes());
            for m in &g.members {
                h.update(m.number().to_le_bytes());
            }
            match g.origin {
                GroupOrigin::User => h.update(b"\tuser"),
                GroupOrigin::Auto { score } => {
                    h.update(b"\tauto");
                    h.update(score.to_bits().to_le_bytes());
                }
            }
            h.update(b"\n");
        }
        h.update(b"not_same\n");
        for [a, b] in pairs {
            h.update(a.number().to_le_bytes());
            h.update(b.number().to_le_bytes());
        }
        hex(&h.finalize())
    }

    /// The microphone track whose speakers the paired mic/system analysis
    /// already decided (segment-level Me verification, Unknown mic speech):
    /// only with exactly one mic and one system track and a setup that is
    /// not a room (`hybrid_room`, `in_person`; an unknown or missing setup
    /// counts as the fallback `speakers_mic`). Its speakers are never
    /// re-resolved from track-wide centroids nor grouped.
    pub fn paired_mic_track(&self) -> Option<&str> {
        let mut mic = None;
        let (mut mics, mut systems) = (0, 0);
        for t in &self.tracks {
            match t.role {
                TrackRole::Mic => {
                    mics += 1;
                    mic = Some(t.id.as_str());
                }
                TrackRole::System => systems += 1,
                TrackRole::Mixed | TrackRole::Participant => {}
            }
        }
        let room = matches!(self.setup.as_deref(), Some("hybrid_room" | "in_person"));
        (mics == 1 && systems == 1 && !room)
            .then_some(mic)
            .flatten()
    }

    /// Roster size used as the grouping floor: distinct participants (by
    /// person, folded e-mail, else folded name) plus the user when no
    /// participant is the user. `None` without participants.
    pub fn roster_floor(&self) -> Option<usize> {
        if self.participants.is_empty() {
            return None;
        }
        let fold = |s: &str| s.trim().to_lowercase();
        let keys: std::collections::BTreeSet<String> = self
            .participants
            .iter()
            .map(|p| {
                if p.is_self {
                    "self".to_owned()
                } else if let Some(id) = &p.person_id {
                    id.to_string()
                } else if let Some(e) = p.email.as_deref().map(fold).filter(|e| !e.is_empty()) {
                    e
                } else {
                    p.name.as_deref().map(fold).unwrap_or_default()
                }
            })
            .filter(|k| !k.is_empty())
            .collect();
        let me = usize::from(!self.participants.iter().any(|p| p.is_self));
        Some(keys.len() + me)
    }

    /// Tags shown and indexed: user tags, then enrichment tags, minus the
    /// user's removals; case-insensitive, first spelling kept, blanks dropped.
    pub fn effective_tags(&self) -> Vec<String> {
        let removed: Vec<String> = self.tags_removed.iter().map(|t| fold_tag(t)).collect();
        let llm = self.enrichment.iter().flat_map(|e| e.tags.iter());
        let mut out: Vec<String> = Vec::new();
        for t in self.tags.iter().chain(llm) {
            let t = t.trim();
            let f = fold_tag(t);
            if f.is_empty() || removed.contains(&f) || out.iter().any(|o| fold_tag(o) == f) {
                continue;
            }
            out.push(t.to_owned());
        }
        out
    }

    /// Adds a user tag and clears an earlier removal of it. `false` when
    /// nothing changed (blank, or already a user tag and not removed).
    pub fn add_tag(&mut self, tag: &str) -> bool {
        let t = tag.trim();
        let f = fold_tag(t);
        if f.is_empty() {
            return false;
        }
        let before = self.tags_removed.len();
        self.tags_removed.retain(|r| fold_tag(r) != f);
        let mut changed = self.tags_removed.len() != before;
        if !self.tags.iter().any(|x| fold_tag(x) == f) {
            self.tags.push(t.to_owned());
            changed = true;
        }
        changed
    }

    /// Removes a tag: drops it from the user tags and records the removal so
    /// enrichment cannot bring it back. `false` when nothing changed.
    pub fn remove_tag(&mut self, tag: &str) -> bool {
        let t = tag.trim();
        let f = fold_tag(t);
        if f.is_empty() {
            return false;
        }
        let before = self.tags.len();
        self.tags.retain(|x| fold_tag(x) != f);
        let mut changed = self.tags.len() != before;
        if !self.tags_removed.iter().any(|r| fold_tag(r) == f) {
            self.tags_removed.push(t.to_owned());
            changed = true;
        }
        changed
    }

    /// Replaces the review notes `source` raised with `texts`; other
    /// sources' notes stay.
    pub fn set_review_notes(
        &mut self,
        source: ReviewSource,
        texts: impl IntoIterator<Item = String>,
    ) {
        self.review_notes.retain(|n| n.source != source);
        self.add_review_notes(source, texts);
    }

    /// Appends review notes from `source`, skipping ones already present.
    pub fn add_review_notes(
        &mut self,
        source: ReviewSource,
        texts: impl IntoIterator<Item = String>,
    ) {
        for text in texts {
            let note = ReviewNote { source, text };
            if !self.review_notes.contains(&note) {
                self.review_notes.push(note);
            }
        }
    }

    /// The stored result of template `id`, if any.
    pub fn template_result(&self, id: &str) -> Option<&TemplateResult> {
        self.template_results.iter().find(|r| r.template_id == id)
    }

    /// Publishes a new summary and retains its immediate predecessor.
    pub fn publish_enrichment(&mut self, result: Enrichment) {
        self.previous_enrichment = self.enrichment.replace(result);
    }

    /// Publishes a template result and retains the immediate predecessor.
    pub fn publish_template_result(&mut self, result: TemplateResult) {
        if let Some(previous) = self.template_result(&result.template_id).cloned() {
            self.previous_template_results
                .insert(result.template_id.clone(), previous);
        }
        self.set_template_result(result);
    }

    /// Stores `result`, replacing the earlier result of the same template.
    pub fn set_template_result(&mut self, result: TemplateResult) {
        match self
            .template_results
            .iter_mut()
            .find(|r| r.template_id == result.template_id)
        {
            Some(slot) => *slot = result,
            None => self.template_results.push(result),
        }
    }

    /// Every piece of generated content: the enrichment, then each template
    /// result.
    pub fn generated(&self) -> impl Iterator<Item = &dyn GeneratedContent> {
        self.enrichment
            .iter()
            .chain(self.previous_enrichment.iter())
            .map(|e| e as &dyn GeneratedContent)
            .chain(
                self.template_results
                    .iter()
                    .chain(self.previous_template_results.values())
                    .map(|r| r as &dyn GeneratedContent),
            )
    }

    /// [`Self::generated`], mutable.
    pub fn generated_mut(&mut self) -> impl Iterator<Item = &mut dyn GeneratedContent> {
        self.enrichment
            .iter_mut()
            .chain(self.previous_enrichment.iter_mut())
            .map(|e| e as &mut dyn GeneratedContent)
            .chain(
                self.template_results
                    .iter_mut()
                    .chain(self.previous_template_results.values_mut())
                    .map(|r| r as &mut dyn GeneratedContent),
            )
    }

    /// Marks every piece of generated content stale (a change that alters
    /// what every reference shows, such as grouping or separating speakers).
    pub fn set_generated_stale(&mut self) {
        for g in self.generated_mut() {
            g.set_stale(true);
        }
    }
}

/// What raised a [`ReviewNote`]; each run replaces only its own source's notes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSource {
    /// Enrichment, including enrichment references that reprocessing could
    /// not map; the next enrichment replaces them.
    Enrichment,
    /// Template results: references reprocessing or a template run could
    /// not map, prefixed `template <id>:`; the next run of that template
    /// replaces its own.
    Template,
    /// Speaker identity reprocessing could not carry over; the next
    /// reprocessing replaces them.
    Reprocess,
    /// Speaker groups dissolved because they broke an invariant (a
    /// hand-edited file, or an identity change since); the user should
    /// check the separated speakers.
    Grouping,
    /// Live-preview names final processing could not fully support; each
    /// recheck replaces them.
    LivePreview,
    /// User decisions that conflict with a speaker's proposed identity.
    SpeakerIdentity,
}

/// Something the user should check on a meeting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewNote {
    pub source: ReviewSource,
    pub text: String,
}

/// Case folding for tag comparisons.
pub fn fold_tag(s: &str) -> String {
    s.trim().to_lowercase()
}

/// A user's attribution of one transcript segment to a person, overriding
/// its speaker's person for that segment only. The segment is identified by
/// its fingerprint: track, exact start and end, and the SHA-256 of its text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UtterancePerson {
    pub track: String,
    pub start: f64,
    pub end: f64,
    pub text_sha256: String,
    pub person_id: PersonId,
    /// When the user set it.
    pub at: DateTime<FixedOffset>,
    /// Set when the final publication created this line from the live
    /// decision with this number (`Meeting::live_attributions`); such a
    /// line is retracted when that decision stops being fully supported.
    /// `None` for every line the user set on the meeting itself, which a
    /// live reconciliation never removes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_decision: Option<u64>,
}

/// LLM enrichment of one meeting (plan §§9.4–9.5, 10.1–10.4). Prose fields
/// may hold `{{S#}}` speaker placeholders and `{{P#}}` line-person
/// placeholders (see [`Enrichment::line_people`]), substituted with current
/// names only when rendering or indexing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Enrichment {
    /// Registry provider ID.
    pub provider: String,
    /// The model that generated this: the one the provider reported when
    /// `model_reported`, else the one requested (`None`: the provider's
    /// default, unknown). Records written before `model_reported` existed
    /// load with it `false`: `model` is then what was requested.
    #[serde(default)]
    pub model: Option<String>,
    /// The model requested for the run (`None`: the provider's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    /// `model` is the one the provider reported for the reply.
    #[serde(default)]
    pub model_reported: bool,
    pub created_at: DateTime<FixedOffset>,
    /// The transcript changed (reprocessing) since this was produced.
    #[serde(default)]
    pub stale: bool,
    /// Window-hash revision of the transcript the provider saw.
    #[serde(default)]
    pub revision: String,
    /// `ops::attribution_revision` of the transcript the provider saw (empty
    /// without line corrections); the enrichment is stale when it differs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub attribution_revision: String,
    /// Opaque labels (`P1`, `P2`, …) the prompt used for lines attributed to
    /// a person by a line correction, mapped to that person. `{{P#}}`
    /// placeholders and `by_name`/`owner_name` equal to a label resolve
    /// through it; a label without an entry (person forgotten) renders
    /// anonymously.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub line_people: BTreeMap<String, PersonId>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Proposed tags; shown through [`Meeting::effective_tags`].
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<Decision>,
    #[serde(default)]
    pub actions: Vec<Action>,
    /// The user's instructions the prompt carried (`[enrichment]
    /// instructions` or the run's own); `None` without any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// The stored output of one meeting template (design §8.4). `markdown`
/// keeps `{{S#}}` / `{{P#}}` placeholders, resolved through this result's
/// own `line_people` only when rendering or indexing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemplateResult {
    pub template_id: String,
    /// The template's name when it ran (kept when the definition goes).
    pub template_name: String,
    /// SHA-256 of the instructions the prompt carried (template body and
    /// any per-run instructions).
    pub instructions_sha256: String,
    /// Registry provider ID.
    pub provider: String,
    /// As [`Enrichment::model`].
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    #[serde(default)]
    pub model_reported: bool,
    pub created_at: DateTime<FixedOffset>,
    /// Window-hash revision of the transcript the provider saw.
    #[serde(default)]
    pub revision: String,
    /// As [`Enrichment::attribution_revision`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub attribution_revision: String,
    /// As [`Enrichment::line_people`], for this result's prompt.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub line_people: BTreeMap<String, PersonId>,
    /// Marked stale (transcript, attribution, grouping or people changed).
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub markdown: String,
}

/// What enrichment and template results share (design §8.4): prose with
/// placeholders, the line labels they resolve through, and the revisions
/// that decide staleness. Every site that rewrites references or decides
/// staleness runs over each piece through this trait.
pub trait GeneratedContent {
    fn line_people(&self) -> &BTreeMap<String, PersonId>;
    fn line_people_mut(&mut self) -> &mut BTreeMap<String, PersonId>;
    /// The stored `stale` flag.
    fn marked_stale(&self) -> bool;
    fn set_stale(&mut self, stale: bool);
    fn revision(&self) -> &str;
    fn attribution_revision(&self) -> &str;
    /// Calls `f` with each prose text that may hold placeholders and what it
    /// is (for review notes).
    fn for_each_text_mut(&mut self, f: &mut dyn FnMut(&str, &mut String));
    /// Calls `f` with each speaker reference field and the provider name
    /// beside it (decisions' `by_*`, actions' `owner_*`) and what it is.
    fn for_each_speaker_ref_mut(&mut self, f: &mut SpeakerRefVisitor<'_>);
}

/// Visits one speaker reference: what it is, the speaker field and the
/// provider-written name beside it.
pub type SpeakerRefVisitor<'a> = dyn FnMut(&str, &mut Option<SpeakerId>, &mut Option<String>) + 'a;

impl GeneratedContent for Enrichment {
    fn line_people(&self) -> &BTreeMap<String, PersonId> {
        &self.line_people
    }
    fn line_people_mut(&mut self) -> &mut BTreeMap<String, PersonId> {
        &mut self.line_people
    }
    fn marked_stale(&self) -> bool {
        self.stale
    }
    fn set_stale(&mut self, stale: bool) {
        self.stale = stale;
    }
    fn revision(&self) -> &str {
        &self.revision
    }
    fn attribution_revision(&self) -> &str {
        &self.attribution_revision
    }
    fn for_each_text_mut(&mut self, f: &mut dyn FnMut(&str, &mut String)) {
        f("summary", &mut self.summary);
        for d in &mut self.decisions {
            f(&format!("decision {}", d.id), &mut d.text);
        }
        for a in &mut self.actions {
            f(&format!("action {}", a.id), &mut a.text);
        }
    }
    fn for_each_speaker_ref_mut(&mut self, f: &mut SpeakerRefVisitor<'_>) {
        for d in &mut self.decisions {
            f(
                &format!("decision {} by", d.id),
                &mut d.by_speaker,
                &mut d.by_name,
            );
        }
        for a in &mut self.actions {
            f(
                &format!("action {} owner", a.id),
                &mut a.owner_speaker,
                &mut a.owner_name,
            );
            if let Some(owner) = &mut a.original_owner {
                f(
                    &format!("action {} original owner", a.id),
                    &mut owner.speaker,
                    &mut owner.name,
                );
            }
        }
    }
}

impl GeneratedContent for TemplateResult {
    fn line_people(&self) -> &BTreeMap<String, PersonId> {
        &self.line_people
    }
    fn line_people_mut(&mut self) -> &mut BTreeMap<String, PersonId> {
        &mut self.line_people
    }
    fn marked_stale(&self) -> bool {
        self.stale
    }
    fn set_stale(&mut self, stale: bool) {
        self.stale = stale;
    }
    fn revision(&self) -> &str {
        &self.revision
    }
    fn attribution_revision(&self) -> &str {
        &self.attribution_revision
    }
    fn for_each_text_mut(&mut self, f: &mut dyn FnMut(&str, &mut String)) {
        let what = format!("template {}", self.template_id);
        f(&what, &mut self.markdown);
    }
    fn for_each_speaker_ref_mut(&mut self, _f: &mut SpeakerRefVisitor<'_>) {}
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// See [`enrichment_item_id`].
    pub id: String,
    pub text: String,
    /// Seconds from the meeting start.
    #[serde(default)]
    pub at_s: f64,
    #[serde(default)]
    pub by_speaker: Option<SpeakerId>,
    /// Name as the provider wrote it, shown when no speaker is resolved.
    #[serde(default)]
    pub by_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Action {
    /// See [`enrichment_item_id`].
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub owner_speaker: Option<SpeakerId>,
    /// Name as the provider wrote it, shown when no speaker is resolved.
    #[serde(default)]
    pub owner_name: Option<String>,
    /// The provider's owner, kept for matching when the user corrects it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_owner: Option<ActionOwner>,
    #[serde(default)]
    pub due_user_set: bool,
    #[serde(default)]
    pub due: Option<NaiveDate>,
    /// Seconds from the meeting start.
    #[serde(default)]
    pub at_s: f64,
    #[serde(default)]
    pub status: ActionStatus,
    #[serde(default)]
    pub status_source: StatusSource,
    #[serde(default)]
    pub status_updated_at: Option<DateTime<FixedOffset>>,
    /// A user-set action the latest enrichment no longer produced.
    #[serde(default)]
    pub stale: bool,
}

impl Action {
    pub fn is_user_edited(&self) -> bool {
        self.status_source == StatusSource::User
            || self.original_owner.is_some()
            || self.due_user_set
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionOwner {
    pub speaker: Option<SpeakerId>,
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    #[default]
    Open,
    Done,
    Dropped,
}

/// Who set an action's status; re-enrichment preserves only `user`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSource {
    User,
    #[default]
    Llm,
}

/// Deterministic ID of a decision or action: the first 12 hex digits of a
/// SHA-256 over its normalised text (lowercase alphanumeric words) and
/// `at_s` rounded to 30 s, so re-enrichment finds the same item again.
pub fn enrichment_item_id(text: &str, at_s: f64) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    let slot = (at_s.max(0.0) / 30.0).round() as i64;
    let mut id = sha256_hex(format!("{}\n{slot}", words.join(" ")).as_bytes());
    id.truncate(12);
    id
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Import,
    Recording,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LanguageSpan {
    /// BCP 47 tag.
    pub lang: String,
    pub from_s: f64,
    pub to_s: f64,
}

/// A user marker on the pause-mapped recording timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    pub at_s: f64,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub at_s: f64,
    pub text: String,
}

/// Someone known to have attended (roster, manifest), whether or not they spoke.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub is_self: bool,
    #[serde(default)]
    pub person_id: Option<PersonId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recording {
    pub state: RecordingState,
    pub handling: Handling,
    /// Relative to the meeting folder when copied or moved; absolute when referenced.
    pub path: PathBuf,
    /// Where the file was imported from.
    #[serde(default)]
    pub original_path: Option<PathBuf>,
    pub sha256: String,
    pub bytes: u64,
    /// Original multi-source content/mapping fingerprint, before AAC encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
    /// External track-addressable sources for reference imports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<RecordingSource>,
    /// The user's per-meeting retention choice, over the configured policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_override: Option<RetentionOverride>,
    /// When retention will delete the recording (`delete_after_days`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_after: Option<DateTime<FixedOffset>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordingSource {
    pub path: PathBuf,
    pub track_index: usize,
    pub offset_s: f64,
    pub sha256: String,
}

/// Per-meeting retention override (`retention keep|delete-now <meeting>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionOverride {
    Keep,
    DeleteNow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Available,
    Missing,
    Deleted,
}

/// What happens to an imported file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Handling {
    #[default]
    Copy,
    Move,
    Reference,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub role: TrackRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackRole {
    Mixed,
    Mic,
    System,
    Participant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Speaker {
    pub id: SpeakerId,
    pub track: String,
    #[serde(default)]
    pub talk_s: f64,
    #[serde(default)]
    pub centroid: Option<Centroid>,
    /// Set when `assignment` is `confirmed` or `auto`.
    #[serde(default)]
    pub person_id: Option<PersonId>,
    #[serde(default)]
    pub assignment: Assignment,
    #[serde(default)]
    pub score: Option<f32>,
    #[serde(default)]
    pub suggestion: Option<Suggestion>,
    /// When the current suggestion came from a name given while recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_suggestion_lines: Option<usize>,
    /// The evidence behind a speaker name carried over from live decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_promotion: Option<crate::live_identity::LivePromotion>,
    /// An explicit speaker edit takes precedence over recording-time names.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub live_speaker_override: bool,
    /// User evidence that prevented automatic naming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_conflict: Option<crate::ops::IdentityConflict>,
    #[serde(default)]
    pub rejected_person_ids: Vec<PersonId>,
    /// When the user chose "Can't tell" for this speaker in review (design
    /// §6.4): it leaves the review queue with no identity effect. Cleared by
    /// naming or confirming it, or by restoring it; a group is dismissed
    /// only while every one of its speakers carries it
    /// ([`crate::ops::review_dismissed`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_dismissed_at: Option<DateTime<FixedOffset>>,
}

/// How a [`SpeakerGroup`] came to be.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GroupOrigin {
    /// Grouped automatically: the voices were at least `auto_merge` similar
    /// (`score`, the lowest similarity at which the group was joined).
    Auto { score: f32 },
    /// Merged by the user.
    User,
}

/// Speakers of one meeting shown as one voice (design §6.4): `members`
/// display as `kept`. Holds speaker IDs only, never centroids, names or
/// people; speakers, segments and samples stay as diarized.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeakerGroup {
    /// `G` and the kept speaker's number (unique within the meeting).
    pub id: String,
    pub kept: SpeakerId,
    /// The absorbed speakers, sorted; never empty, never `kept`.
    pub members: Vec<SpeakerId>,
    pub origin: GroupOrigin,
    pub at: DateTime<FixedOffset>,
}

impl SpeakerGroup {
    /// A group of `kept` and `members` (sorted, deduplicated, without `kept`).
    pub fn new(
        kept: SpeakerId,
        members: impl IntoIterator<Item = SpeakerId>,
        origin: GroupOrigin,
    ) -> Self {
        let mut members: Vec<SpeakerId> = members.into_iter().filter(|m| *m != kept).collect();
        members.sort();
        members.dedup();
        Self {
            id: Self::id_for(kept),
            kept,
            members,
            origin,
            at: now(),
        }
    }

    /// The ID of the group kept as `kept`.
    pub fn id_for(kept: SpeakerId) -> String {
        format!("G{}", kept.number())
    }

    /// The kept speaker, then the members.
    pub fn speakers(&self) -> impl Iterator<Item = SpeakerId> + '_ {
        std::iter::once(self.kept).chain(self.members.iter().copied())
    }

    /// Whether `speaker` is the kept speaker or a member.
    pub fn contains(&self, speaker: SpeakerId) -> bool {
        self.kept == speaker || self.members.contains(&speaker)
    }
}

impl Speaker {
    pub fn new(id: SpeakerId, track: impl Into<String>) -> Self {
        Self {
            id,
            track: track.into(),
            talk_s: 0.0,
            centroid: None,
            person_id: None,
            assignment: Assignment::None,
            score: None,
            suggestion: None,
            live_suggestion_lines: None,
            live_promotion: None,
            live_speaker_override: false,
            identity_conflict: None,
            rejected_person_ids: Vec::new(),
            review_dismissed_at: None,
        }
    }

    /// The person this speaker is named as (`confirmed` or `auto`), if any.
    pub fn named_person(&self) -> Option<&PersonId> {
        match self.assignment {
            Assignment::Confirmed | Assignment::Auto => self.person_id.as_ref(),
            Assignment::Suggested | Assignment::None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Assignment {
    Confirmed,
    Auto,
    Suggested,
    #[default]
    None,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub person_id: PersonId,
    pub score: f32,
}

/// A speaker's mean embedding (L2-normalised) for one embedding model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Centroid {
    pub model: String,
    pub dim: usize,
    pub vector_b64: String,
    pub n_turns: u32,
    /// Exact clean turns that contributed embeddings; absent in legacy records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<[f32; 2]>,
}

impl Centroid {
    pub fn new(model: impl Into<String>, vector: &[f32], n_turns: u32) -> Self {
        Self {
            model: model.into(),
            dim: vector.len(),
            vector_b64: encode_vector(vector),
            n_turns,
            ranges: Vec::new(),
        }
    }

    pub fn vector(&self) -> Result<Vec<f32>> {
        decode_vector(&self.vector_b64, self.dim)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub id: u32,
    pub track: String,
    #[serde(default)]
    pub speaker: Option<SpeakerId>,
    pub start: f64,
    pub end: f64,
    pub text: String,
    #[serde(default)]
    pub words: Vec<Word>,
    /// Concurrent voices on this track, clipped to the segment in meeting time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub speech_overlap: Vec<[f64; 2]>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

/// Model IDs used to process the meeting.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsUsed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vad: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_embedding: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Processing {
    #[serde(default)]
    pub status: ProcessingStatus,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
    /// Serialized DSP EchoReport; absent for single-track imports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo_report: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingStatus {
    #[default]
    Queued,
    /// Transcribed and diarized; identities not resolved yet.
    Analyzed,
    /// Durable process-now revision; never eligible for retention.
    Provisional,
    Done,
    Failed,
}

/// `person.md` frontmatter; the notes body is kept separately.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Person {
    pub id: PersonId,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub emails: Vec<String>,
    #[serde(default)]
    pub is_self: bool,
    /// Caption-only identities require a user decision before they are trusted.
    #[serde(default)]
    pub needs_review: bool,
    /// The person's colour, 1 to [`PERSON_HUES`], drawn on their avatar disc.
    /// Assigned when the person is first saved (the least-used hue) and never
    /// changed afterwards; a file written before hues existed reads as
    /// absent and [`Person::hue`] derives a stable one from the ID.
    #[serde(
        default,
        deserialize_with = "deserialize_person_hue",
        skip_serializing_if = "Option::is_none"
    )]
    pub hue: Option<u8>,
    pub created_at: DateTime<FixedOffset>,
}

/// How many person hues there are.
pub const PERSON_HUES: u8 = 7;

fn deserialize_person_hue<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u8>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Hue {
        Number(u8),
        Other(serde::de::IgnoredAny),
    }

    match Hue::deserialize(d)? {
        Hue::Number(hue @ 1..=PERSON_HUES) => Ok(Some(hue)),
        _ => Ok(None),
    }
}

impl Person {
    pub fn new(id: PersonId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            aliases: Vec::new(),
            emails: Vec::new(),
            is_self: false,
            needs_review: false,
            hue: None,
            created_at: now(),
        }
    }

    /// The person's hue, 1 to [`PERSON_HUES`]: the stored one, or for a
    /// person saved before hues existed (or holding a value out of range)
    /// one derived from the ID, the same on every read.
    pub fn hue(&self) -> u8 {
        match self.hue {
            Some(hue @ 1..=PERSON_HUES) => hue,
            _ => derived_hue(&self.id),
        }
    }
}

/// A stable hue for `id` (FNV-1a over its text), for people saved before
/// hues were stored.
pub fn derived_hue(id: &PersonId) -> u8 {
    let hash = id.as_str().bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    (hash % u32::from(PERSON_HUES)) as u8 + 1
}

/// The hue for a new person: the least used among `people` (by their
/// effective hue), the lowest on a tie.
pub fn least_used_hue<'a>(people: impl IntoIterator<Item = &'a Person>) -> u8 {
    let mut counts = [0usize; PERSON_HUES as usize];
    for person in people {
        counts[usize::from(person.hue() - 1)] += 1;
    }
    let (index, _) = counts
        .iter()
        .enumerate()
        .min_by_key(|(index, count)| (**count, *index))
        .expect("there are hues");
    index as u8 + 1
}

/// `people/<id>/voiceprints.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Voiceprints {
    #[serde(default = "v1")]
    pub schema_version: u32,
    pub person_id: PersonId,
    #[serde(default)]
    pub embeddings: Vec<Voiceprint>,
}

impl Voiceprints {
    pub fn empty(person_id: PersonId) -> Self {
        Self {
            schema_version: VOICEPRINTS_SCHEMA_VERSION,
            person_id,
            embeddings: Vec::new(),
        }
    }

    /// Inserts each voiceprint, replacing one with the same ID.
    pub fn upsert(&mut self, prints: Vec<Voiceprint>) {
        for vp in prints {
            match self.embeddings.iter_mut().find(|e| e.id == vp.id) {
                Some(slot) => *slot = vp,
                None => self.embeddings.push(vp),
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Voiceprint {
    /// Deterministic, see [`voiceprint_id`].
    pub id: String,
    pub model: String,
    pub dim: usize,
    pub vector_b64: String,
    pub source: VoiceprintSource,
    #[serde(default)]
    pub device: Option<String>,
    pub strength: Strength,
    pub created_at: DateTime<FixedOffset>,
}

impl Voiceprint {
    pub fn vector(&self) -> Result<Vec<f32>> {
        decode_vector(&self.vector_b64, self.dim)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VoiceprintSource {
    pub kind: VoiceprintKind,
    #[serde(default)]
    pub meeting_id: Option<MeetingId>,
    /// SHA-256 of the source recording.
    #[serde(default)]
    pub sha256: Option<String>,
    pub start: f64,
    pub end: f64,
    /// Exact contributing audio ranges; `start`/`end` remain their envelope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<[f32; 2]>,
    /// Speaker of `meeting_id` the sample was learned from (confirmation,
    /// auto and caption samples), valid while `speaker_namespace` equals the
    /// meeting's; otherwise (and for older samples) attributed by range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<SpeakerId>,
    /// [`Meeting::speaker_namespace`] that `speaker_id` belongs to.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub speaker_namespace: String,
}

impl VoiceprintSource {
    /// Audio to re-embed. Older samples without exact provenance use their stored envelope.
    pub fn audio_ranges(&self) -> impl Iterator<Item = [f32; 2]> + '_ {
        self.ranges.iter().copied().chain(
            self.ranges
                .is_empty()
                .then_some([self.start as f32, self.end as f32]),
        )
    }

    /// Records that the sample was learned from `speaker` of `meeting`'s
    /// current analysis.
    pub fn set_speaker(&mut self, meeting: &Meeting, speaker: SpeakerId) {
        self.speaker_id = Some(speaker);
        self.speaker_namespace
            .clone_from(&meeting.speaker_namespace);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceprintKind {
    Hint,
    Confirmation,
    LiveConfirmation,
    Enrollment,
    Auto,
    /// Learned from a meeting app's captions naming the speaker (M5);
    /// stored weak until confirmed.
    Captions,
}

impl VoiceprintKind {
    pub fn as_str(self) -> &'static str {
        match self {
            VoiceprintKind::Hint => "hint",
            VoiceprintKind::Confirmation => "confirmation",
            VoiceprintKind::LiveConfirmation => "live_confirmation",
            VoiceprintKind::Enrollment => "enrollment",
            VoiceprintKind::Auto => "auto",
            VoiceprintKind::Captions => "captions",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    Confirmed,
    Weak,
}

/// Little-endian f32 values, base64 (standard alphabet, padded).
pub fn encode_vector(v: &[f32]) -> String {
    let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    BASE64.encode(bytes)
}

/// Inverse of [`encode_vector`]; fails unless the data holds exactly `dim`
/// values, every one finite: a NaN or infinite component would poison every
/// similarity computed from the vector, so it never leaves this boundary.
pub fn decode_vector(s: &str, dim: usize) -> Result<Vec<f32>> {
    let bytes = BASE64
        .decode(s)
        .map_err(|e| Error::Schema(format!("invalid vector base64: {e}")))?;
    let expected = dim
        .checked_mul(4)
        .ok_or_else(|| Error::Schema(format!("vector dim {dim} is out of range")))?;
    if bytes.len() != expected {
        return Err(Error::Schema(format!(
            "vector has {} bytes, expected {expected} for dim {dim}",
            bytes.len(),
        )));
    }
    let values: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect();
    if let Some(i) = values.iter().position(|v| !v.is_finite()) {
        return Err(Error::Schema(format!(
            "vector value {i} of {dim} is not a finite number"
        )));
    }
    Ok(values)
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// What a voiceprint's audio came from, for [`voiceprint_id`]. The recording's
/// SHA-256 whenever the audio is known, so the same audio gives the same ID
/// whichever path (import, batch, confirmation, enrollment, re-embedding)
/// stores it; the meeting ID only when no recording exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceprintOrigin<'a> {
    Recording { sha256: &'a str },
    Meeting { id: &'a MeetingId },
}

impl<'a> VoiceprintOrigin<'a> {
    /// The recording when its SHA-256 is known, else the meeting.
    pub fn new(sha256: Option<&'a str>, meeting: &'a MeetingId) -> Self {
        match sha256 {
            Some(sha256) => Self::Recording { sha256 },
            None => Self::Meeting { id: meeting },
        }
    }

    /// The meeting's recording, else the meeting itself.
    pub fn of_meeting(meeting: &'a Meeting) -> Self {
        Self::new(
            meeting.recording.as_ref().map(|r| r.sha256.as_str()),
            &meeting.id,
        )
    }

    /// The origin recorded in a voiceprint's source; `None` when it names
    /// neither a recording nor a meeting.
    pub fn of_source(source: &'a VoiceprintSource) -> Option<Self> {
        match (&source.sha256, &source.meeting_id) {
            (Some(sha256), _) => Some(Self::Recording { sha256 }),
            (None, Some(id)) => Some(Self::Meeting { id }),
            (None, None) => None,
        }
    }
}

/// Deterministic voiceprint ID: the first 16 hex digits of a SHA-256 over the
/// person, model, origin (see [`VoiceprintOrigin`]), range (ms) and kind, so
/// re-enrolling the same audio upserts instead of duplicating.
pub fn voiceprint_id(
    person: &PersonId,
    model: &str,
    origin: VoiceprintOrigin<'_>,
    start: f64,
    end: f64,
    kind: VoiceprintKind,
) -> String {
    let source: &dyn std::fmt::Display = match &origin {
        VoiceprintOrigin::Recording { sha256 } => sha256,
        VoiceprintOrigin::Meeting { id } => id,
    };
    let key = format!(
        "{person}\n{model}\n{source}\n{}\n{}\n{}",
        (start * 1000.0).round() as i64,
        (end * 1000.0).round() as i64,
        kind.as_str()
    );
    let mut id = sha256_hex(key.as_bytes());
    id.truncate(16);
    id
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    pub(crate) fn sample_meeting() -> Meeting {
        let ana = PersonId::parse("p-a1a1a1").unwrap();
        let mut m = Meeting::new(
            MeetingId::parse("m-7f3a2c").unwrap(),
            Source::Import,
            ts("2026-11-13T14:00:00-03:00"),
        );
        m.title = Some("Roadmap review".into());
        m.duration_s = 125.0;
        m.languages.push(LanguageSpan {
            lang: "en".into(),
            from_s: 0.0,
            to_s: 125.0,
        });
        m.recording = Some(Recording {
            state: RecordingState::Available,
            handling: Handling::Copy,
            path: "audio.m4a".into(),
            original_path: Some("/tmp/in/demo.m4a".into()),
            sha256: "ab".repeat(32),
            bytes: 1234,
            source_sha256: None,
            sources: Vec::new(),
            retention_override: None,
            delete_after: None,
        });
        m.tracks.push(Track {
            id: "t0".into(),
            role: TrackRole::Mixed,
        });
        let mut s1 = Speaker::new(SpeakerId::new(1).unwrap(), "t0");
        s1.talk_s = 60.0;
        s1.person_id = Some(ana.clone());
        s1.assignment = Assignment::Confirmed;
        s1.centroid = Some(Centroid::new("nemo-titanet-large", &[0.6, 0.8], 4));
        let mut s2 = Speaker::new(SpeakerId::new(2).unwrap(), "t0");
        s2.talk_s = 40.0;
        s2.suggestion = Some(Suggestion {
            person_id: ana,
            score: 0.5,
        });
        m.speakers = vec![s1, s2];
        m.segments = vec![
            Segment {
                speech_overlap: Vec::new(),
                id: 0,
                track: "t0".into(),
                speaker: Some(SpeakerId::new(1).unwrap()),
                start: 1.0,
                end: 2.5,
                text: "Hello there.".into(),
                words: vec![
                    Word {
                        text: "Hello".into(),
                        start: 1.0,
                        end: 1.6,
                    },
                    Word {
                        text: "there.".into(),
                        start: 1.7,
                        end: 2.5,
                    },
                ],
            },
            Segment {
                speech_overlap: Vec::new(),
                id: 1,
                track: "t0".into(),
                speaker: Some(SpeakerId::new(1).unwrap()),
                start: 3.0,
                end: 4.0,
                text: "Shall we start?".into(),
                words: vec![],
            },
            Segment {
                speech_overlap: Vec::new(),
                id: 2,
                track: "t0".into(),
                speaker: Some(SpeakerId::new(2).unwrap()),
                start: 3661.0,
                end: 3663.0,
                text: "Yes.".into(),
                words: vec![],
            },
        ];
        m.models.asr = Some("parakeet-tdt-0.6b-v3".into());
        m
    }

    #[test]
    fn meeting_json_round_trip() {
        let m = sample_meeting();
        let json = serde_json::to_string_pretty(&m).unwrap();
        assert!(json.contains("\"started_at\": \"2026-11-13T14:00:00-03:00\""));
        assert!(json.contains("\"assignment\": \"confirmed\""));
        let back: Meeting = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn unknown_fields_ignored_and_defaults_applied() {
        let json = r#"{"id":"m-000001","source":"recording","started_at":"2026-11-13T14:00:00Z",
            "created_at":"2026-11-13T14:00:00Z","updated_at":"2026-11-13T14:00:00Z","future":1}"#;
        let m: Meeting = serde_json::from_str(json).unwrap();
        assert_eq!(m.schema_version, 1);
        assert_eq!(m.processing.status, ProcessingStatus::Queued);
        assert!(m.speakers.is_empty());
        assert_eq!(migrate_meeting(m).unwrap().schema_version, SCHEMA_VERSION);
    }

    /// A `meeting.json` as M1 wrote it (schema v1).
    pub(crate) const V1_MEETING_JSON: &str = r#"{
  "schema_version": 1,
  "id": "m-0a1b2c",
  "source": "import",
  "source_id": null,
  "title": "Weekly sync",
  "started_at": "2026-10-01T09:00:00-03:00",
  "duration_s": 12.0,
  "app": null,
  "setup": null,
  "languages": [{"lang": "en", "from_s": 0.0, "to_s": 12.0}],
  "language_effective": null,
  "participants": [],
  "tags": ["planning"],
  "hotwords": [],
  "recording": {
    "state": "available",
    "handling": "copy",
    "path": "audio.wav",
    "original_path": "/tmp/in/weekly.wav",
    "sha256": "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
    "bytes": 4096
  },
  "tracks": [{"id": "t0", "role": "mixed"}],
  "speakers": [{"id": "S1", "track": "t0", "talk_s": 5.0, "centroid": null,
                "person_id": null, "assignment": "none", "score": null,
                "suggestion": null, "rejected_person_ids": []}],
  "segments": [{"id": 0, "track": "t0", "speaker": "S1", "start": 1.0, "end": 3.0,
                "text": "Budget figures look fine.", "words": []}],
  "models": {"asr": "parakeet-tdt-0.6b-v3"},
  "processing": {"status": "done", "duration_ms": 900, "error": null},
  "created_at": "2026-10-01T09:05:00-03:00",
  "updated_at": "2026-10-01T09:05:00-03:00"
}
"#;

    #[test]
    fn v1_meeting_migrates_in_memory_and_saves_as_v2() {
        let raw: Meeting = serde_json::from_str(V1_MEETING_JSON).unwrap();
        assert_eq!(raw.schema_version, 1);
        let m = migrate_meeting(raw).unwrap();
        assert_eq!(m.schema_version, 2);
        assert!(m.tags_removed.is_empty());
        assert!(m.enrichment.is_none());
        let rec = m.recording.as_ref().unwrap();
        assert_eq!(rec.retention_override, None);
        assert_eq!(rec.delete_after, None);
        assert_eq!(m.effective_tags(), vec!["planning".to_owned()]);

        // Through the library: read as v2, rewritten as v2, indexed.
        let tmp = tempfile::tempdir().unwrap();
        let lib = crate::Library::open(tmp.path()).unwrap();
        let dir = tmp.path().join(crate::paths::meeting_rel_dir(
            &m.started_at,
            m.title.as_deref(),
            &m.id,
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(crate::paths::MEETING_JSON), V1_MEETING_JSON).unwrap();
        assert_eq!(lib.load_meeting(&m.id).unwrap(), m);
        let report = lib.write(|tx| crate::index::Index::rebuild(tx)).unwrap();
        assert_eq!(report.meetings, 1);
        assert!(report.skipped.is_empty());
        let hits = crate::index::Index::open_readonly(tmp.path())
            .unwrap()
            .search(&crate::index::SearchQuery {
                text: Some("budget".into()),
                ..crate::index::SearchQuery::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].meeting_id, m.id);

        lib.write(|tx| tx.update_meeting(&m.id, |_| Ok(())))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(crate::paths::MEETING_JSON)).unwrap(),
        )
        .unwrap();
        assert_eq!(v["schema_version"], 2);
        assert_eq!(v["segments"][0]["text"], "Budget figures look fine.");
    }

    #[test]
    fn newer_meeting_is_refused() {
        let mut m = sample_meeting();
        m.schema_version = SCHEMA_VERSION + 1;
        assert_eq!(migrate_meeting(m).unwrap_err().code(), "schema");
        assert!(check_voiceprints_schema_version(VOICEPRINTS_SCHEMA_VERSION).is_ok());
        assert!(check_voiceprints_schema_version(VOICEPRINTS_SCHEMA_VERSION + 1).is_err());
    }

    fn enrichment() -> Enrichment {
        Enrichment {
            provider: "claude-cli".into(),
            model: None,
            requested_model: None,
            model_reported: false,
            created_at: ts("2026-11-14T10:00:00-03:00"),
            stale: false,
            revision: "r1".into(),
            attribution_revision: String::new(),
            line_people: BTreeMap::new(),
            summary: "{{S1}} opened the meeting.".into(),
            topics: vec!["roadmap".into()],
            keywords: vec!["budget".into()],
            tags: vec!["Roadmap".into(), "Planning".into(), "hiring".into()],
            decisions: vec![Decision {
                id: enrichment_item_id("Ship in March", 3.0),
                text: "Ship in March".into(),
                at_s: 3.0,
                by_speaker: Some(SpeakerId::new(1).unwrap()),
                by_name: None,
            }],
            actions: vec![Action {
                id: enrichment_item_id("{{S2}} drafts the plan", 61.0),
                text: "{{S2}} drafts the plan".into(),
                owner_speaker: Some(SpeakerId::new(2).unwrap()),
                owner_name: Some("Bo".into()),
                due: NaiveDate::from_ymd_opt(2026, 11, 20),
                at_s: 61.0,
                status: ActionStatus::Done,
                status_source: StatusSource::User,
                status_updated_at: Some(ts("2026-11-14T11:00:00-03:00")),
                original_owner: None,
                due_user_set: false,
                stale: false,
            }],
            instructions: None,
        }
    }

    #[test]
    fn enrichment_round_trip_and_defaults() {
        let mut m = sample_meeting();
        m.enrichment = Some(enrichment());
        m.recording.as_mut().unwrap().retention_override = Some(RetentionOverride::DeleteNow);
        let json = serde_json::to_string_pretty(&m).unwrap();
        assert!(json.contains("\"due\": \"2026-11-20\""), "{json}");
        assert!(json.contains("\"status_source\": \"user\""));
        assert!(json.contains("\"retention_override\": \"delete_now\""));
        assert!(json.contains("\"owner_speaker\": \"S2\""));
        let back: Meeting = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);

        let minimal: Action =
            serde_json::from_str(r#"{"id":"x","text":"Call the vendor"}"#).unwrap();
        assert_eq!(minimal.status, ActionStatus::Open);
        assert_eq!(minimal.status_source, StatusSource::Llm);
        assert!(!minimal.stale);
    }

    #[test]
    fn enrichment_model_fields_default_for_old_records() {
        // Written before requested/reported models were recorded: `model`
        // was what was requested.
        let mut old = serde_json::to_value(enrichment()).unwrap();
        let obj = old.as_object_mut().unwrap();
        obj.remove("requested_model");
        obj.remove("model_reported");
        obj.insert("model".into(), "alias-q".into());
        let e: Enrichment = serde_json::from_value(old).unwrap();
        assert_eq!(e.model.as_deref(), Some("alias-q"));
        assert_eq!(e.requested_model, None);
        assert!(!e.model_reported);

        let mut e = enrichment();
        e.model = Some("alias-q-2026".into());
        e.requested_model = Some("alias-q".into());
        e.model_reported = true;
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["requested_model"], "alias-q");
        assert_eq!(json["model_reported"], true);
        assert_eq!(serde_json::from_value::<Enrichment>(json).unwrap(), e);
        // The provider default: no `requested_model` key, `model_reported` always.
        let json = serde_json::to_value(enrichment()).unwrap();
        assert!(json.get("requested_model").is_none());
        assert_eq!(json["model_reported"], false);
    }

    #[test]
    fn speaker_namespaces_default_empty_and_round_trip_when_present() {
        let mut meeting = sample_meeting();
        let json = serde_json::to_value(&meeting).unwrap();
        assert!(json.get("speaker_namespace").is_none());
        let legacy: Meeting = serde_json::from_value(json).unwrap();
        assert!(legacy.speaker_namespace.is_empty());

        let mut source: VoiceprintSource =
            serde_json::from_str(r#"{"kind":"captions","start":0.0,"end":2.0,"speaker_id":"S1"}"#)
                .unwrap();
        assert!(source.speaker_namespace.is_empty());
        assert!(
            serde_json::to_value(&source)
                .unwrap()
                .get("speaker_namespace")
                .is_none()
        );

        meeting.speaker_namespace = "current-analysis".into();
        source.set_speaker(&meeting, SpeakerId::new(1).unwrap());
        let saved: Meeting =
            serde_json::from_value(serde_json::to_value(&meeting).unwrap()).unwrap();
        let saved_source: VoiceprintSource =
            serde_json::from_value(serde_json::to_value(&source).unwrap()).unwrap();
        assert_eq!(saved, meeting);
        assert_eq!(saved_source.speaker_namespace, saved.speaker_namespace);
        assert_eq!(saved_source.speaker_id, Some(SpeakerId::new(1).unwrap()));
    }

    #[test]
    fn review_notes_default_empty_round_trip_and_replace_per_source() {
        let mut m = sample_meeting();
        m.enrichment = Some(enrichment());
        let json = serde_json::to_value(&m).unwrap();
        assert!(json.get("review_notes").is_none());
        let back: Meeting = serde_json::from_value(json).unwrap();
        assert!(back.review_notes.is_empty());

        m.set_review_notes(ReviewSource::Enrichment, ["a".to_owned(), "a".to_owned()]);
        m.set_review_notes(ReviewSource::Reprocess, ["b".to_owned()]);
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["review_notes"][1]["source"], "reprocess");
        let back: Meeting = serde_json::from_value(json).unwrap();
        assert_eq!(back, m);

        m.set_review_notes(ReviewSource::Enrichment, Vec::new());
        assert_eq!(
            m.review_notes,
            vec![ReviewNote {
                source: ReviewSource::Reprocess,
                text: "b".into(),
            }]
        );
    }

    #[test]
    fn meeting_with_notes_on_the_enrichment_still_loads() {
        // Development builds kept review notes on the enrichment.
        let mut json = serde_json::to_value(sample_meeting()).unwrap();
        let mut e = serde_json::to_value(enrichment()).unwrap();
        e["review_notes"] = serde_json::json!(["Check the owner of an action."]);
        json["enrichment"] = e;
        let m = migrate_meeting(serde_json::from_value(json).unwrap()).unwrap();
        assert_eq!(m.enrichment, Some(enrichment()));
        assert!(m.review_notes.is_empty());
    }

    #[test]
    fn effective_tags_merge_user_and_llm_minus_removed() {
        let mut m = sample_meeting();
        m.tags = vec!["planning".into(), " ".into()];
        m.enrichment = Some(enrichment());
        assert_eq!(m.effective_tags(), vec!["planning", "Roadmap", "hiring"]);

        // Removing an enrichment tag hides it, also after re-enrichment.
        assert!(m.remove_tag("ROADMAP"));
        assert!(!m.remove_tag("roadmap"));
        assert_eq!(m.effective_tags(), vec!["planning", "hiring"]);
        m.enrichment.as_mut().unwrap().tags = vec!["roadmap".into()];
        assert_eq!(m.effective_tags(), vec!["planning"]);

        // Removing a user tag drops it; adding clears the removal.
        assert!(m.remove_tag("Planning"));
        assert!(m.effective_tags().is_empty());
        assert!(m.add_tag(" roadmap "));
        assert_eq!(m.effective_tags(), vec!["roadmap"]);
        assert!(m.tags_removed.iter().all(|t| t != "ROADMAP"));
        assert!(!m.add_tag("Roadmap"));
        assert!(!m.add_tag("  "));
    }

    #[test]
    fn item_ids_ignore_case_punctuation_and_small_time_shifts() {
        let a = enrichment_item_id("Ship in March!", 61.0);
        assert_eq!(a.len(), 12);
        assert_eq!(a, enrichment_item_id("  ship IN march", 68.0));
        assert_ne!(a, enrichment_item_id("Ship in April", 61.0));
        assert_ne!(a, enrichment_item_id("Ship in March", 120.0));
    }

    #[test]
    fn vectors_with_non_finite_values_are_rejected_at_decode() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let s = encode_vector(&[0.5, bad]);
            let e = decode_vector(&s, 2).unwrap_err();
            assert_eq!(e.code(), "schema", "{bad}");
        }
        // Stored records go through the same boundary.
        let mut c = Centroid::new("nemo-titanet-large", &[0.6, 0.8], 4);
        assert_eq!(c.vector().unwrap(), vec![0.6, 0.8]);
        c.vector_b64 = encode_vector(&[0.6, f32::NAN]);
        assert_eq!(c.vector().unwrap_err().code(), "schema");
        // A dimension whose byte length overflows is an error, not a panic.
        assert_eq!(decode_vector("", usize::MAX).unwrap_err().code(), "schema");
    }

    #[test]
    fn vector_round_trip() {
        let v = [0.25f32, -1.5, 3.0e-7, 0.0];
        let s = encode_vector(&v);
        assert_eq!(decode_vector(&s, 4).unwrap(), v);
        assert!(decode_vector(&s, 3).is_err());
        assert!(decode_vector("not base64!", 1).is_err());
        assert_eq!(Centroid::new("x", &v, 1).vector().unwrap(), v);
    }

    #[test]
    fn voiceprint_ids_are_deterministic() {
        let p = PersonId::parse("p-000001").unwrap();
        let rec = VoiceprintOrigin::Recording { sha256: "abc" };
        let a = voiceprint_id(&p, "m1", rec, 1.0, 2.0, VoiceprintKind::Hint);
        assert_eq!(a.len(), 16);
        assert_eq!(
            a,
            voiceprint_id(&p, "m1", rec, 1.0, 2.0, VoiceprintKind::Hint)
        );
        assert_ne!(
            a,
            voiceprint_id(&p, "m1", rec, 1.0, 2.0, VoiceprintKind::Confirmation)
        );
        assert_ne!(
            a,
            voiceprint_id(&p, "m2", rec, 1.0, 2.0, VoiceprintKind::Hint)
        );

        // The recording wins over the meeting whenever it is known.
        let mid = MeetingId::parse("m-7f3a2c").unwrap();
        assert_eq!(VoiceprintOrigin::new(Some("abc"), &mid), rec);
        assert_eq!(
            VoiceprintOrigin::new(None, &mid),
            VoiceprintOrigin::Meeting { id: &mid }
        );
        let src = VoiceprintSource {
            kind: VoiceprintKind::Hint,
            meeting_id: Some(mid.clone()),
            sha256: Some("abc".into()),
            start: 1.0,
            end: 2.0,
            ranges: Vec::new(),
            speaker_id: None,
            speaker_namespace: String::new(),
        };
        assert_eq!(VoiceprintOrigin::of_source(&src), Some(rec));
    }

    #[test]
    fn upsert_dedupes_by_id() {
        let p = PersonId::parse("p-000001").unwrap();
        let vp = |id: &str, model: &str| Voiceprint {
            id: id.into(),
            model: model.into(),
            dim: 1,
            vector_b64: encode_vector(&[1.0]),
            source: VoiceprintSource {
                kind: VoiceprintKind::Hint,
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
            created_at: ts("2026-11-13T14:00:00Z"),
        };
        let mut vps = Voiceprints::empty(p);
        vps.upsert(vec![vp("a", "x"), vp("b", "x")]);
        vps.upsert(vec![vp("a", "y")]);
        assert_eq!(vps.embeddings.len(), 2);
        assert_eq!(vps.embeddings[0].model, "y");
    }

    #[test]
    fn legacy_provenance_defaults_and_explicit_title() {
        let mut m = Meeting::new(
            MeetingId::parse("m-000001").unwrap(),
            Source::Import,
            ts("2026-11-13T14:00:00Z"),
        );
        let mut json = serde_json::to_value(&m).unwrap();
        json.as_object_mut().unwrap().remove("title_source");
        m = serde_json::from_value(json).unwrap();
        assert_eq!(m.title_source, Some(TitleSource::File));
        m.set_user_title(Some("Chosen title".into()));
        assert_eq!(m.title_source, Some(TitleSource::User));
        assert_eq!(m.title.as_deref(), Some("Chosen title"));

        let source: VoiceprintSource = serde_json::from_value(serde_json::json!({
            "kind": "auto", "start": 1.0, "end": 9.0
        }))
        .unwrap();
        assert_eq!(source.audio_ranges().collect::<Vec<_>>(), vec![[1.0, 9.0]]);
        assert!(
            !serde_json::to_value(&source)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("ranges")
        );
        let mut source = source;
        source.ranges = vec![[1.0, 3.0], [7.0, 9.0]];
        assert_eq!(
            source.audio_ranges().collect::<Vec<_>>(),
            vec![[1.0, 3.0], [7.0, 9.0]]
        );
        let person: Person = serde_json::from_value(serde_json::json!({
            "id": "p-000001", "created_at": "2026-11-13T14:00:00Z"
        }))
        .unwrap();
        assert!(!person.needs_review);
    }

    #[test]
    fn sha256_hex_known_value() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
