//! Markdown files: rendered `meeting.md` and `person.md` (frontmatter + notes).

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::schema::{
    ActionStatus, Handling, Meeting, ModelsUsed, Person, RecordingState, RetentionOverride, Source,
    TemplateResult,
};
use crate::{Error, PersonId, Result, SpeakerId};

/// People by ID, for naming speakers when rendering.
#[derive(Clone, Debug, Default)]
pub struct PeopleLookup {
    people: HashMap<PersonId, Person>,
}

impl PeopleLookup {
    pub fn new(people: impl IntoIterator<Item = Person>) -> Self {
        Self {
            people: people.into_iter().map(|p| (p.id.clone(), p)).collect(),
        }
    }

    pub fn get(&self, id: &PersonId) -> Option<&Person> {
        self.people.get(id)
    }

    /// The person's name; "Me" for the self person without a name.
    pub fn display_name(&self, id: &PersonId) -> Option<&str> {
        let p = self.people.get(id)?;
        if !p.name.trim().is_empty() {
            Some(p.name.as_str())
        } else if p.is_self {
            Some("Me")
        } else {
            None
        }
    }
}

/// Name shown for a speaker: its effective speaker's (a grouped member
/// shows as its group's kept speaker) named person, else
/// [`unnamed_speaker_label`].
pub fn speaker_label(
    meeting: &Meeting,
    speaker: Option<SpeakerId>,
    people: &PeopleLookup,
) -> String {
    let Some(id) = speaker else {
        return "Unknown".to_owned();
    };
    let id = crate::ops::effective_speaker(meeting, id);
    meeting
        .speaker(id)
        .and_then(|s| s.named_person())
        .and_then(|p| people.display_name(p))
        .map_or_else(|| unnamed_speaker_label(meeting, id), str::to_owned)
}

/// Label of shown speaker `id` when it has no named person: `Unknown (mic)`
/// for the paired microphone track outside room setups (`hybrid_room`,
/// `in_person`), else `Speaker N`. The one fallback the transcript, the
/// frontmatter, exports, the index and search share.
pub fn unnamed_speaker_label(meeting: &Meeting, id: SpeakerId) -> String {
    if meeting.speaker(id).is_some_and(|s| s.track == "mic")
        && !matches!(meeting.setup.as_deref(), Some("hybrid_room" | "in_person"))
    {
        "Unknown (mic)".to_owned()
    } else {
        format!("Speaker {}", id.number())
    }
}

/// How a name (a person's, a speaker label, a provider-written owner) is
/// written into rendered output. Names may predate validation (imported or
/// legacy records), so every rendered form neutralises what could break
/// out of its place; the stored records and transcript text are never
/// rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameStyle {
    /// As stored: JSON values, the app's plain fields, the index.
    Raw,
    /// One line: every [`crate::names::is_name_control`] character becomes
    /// a space, then the name is trimmed. Plain-text output.
    Plain,
    /// [`NameStyle::Plain`], with Markdown punctuation backslash-escaped so
    /// a name can never open a heading, list, quote, link, HTML or
    /// emphasis: Markdown output.
    Markdown,
}

impl NameStyle {
    pub fn apply(self, name: &str) -> String {
        match self {
            Self::Raw => name.to_owned(),
            Self::Plain => plain_name(name),
            Self::Markdown => markdown_name(&plain_name(name)),
        }
    }
}

fn plain_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if crate::names::is_name_control(c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// `name` (one line) with inline Markdown punctuation escaped everywhere,
/// and what would start a block (list `-`/`+`/`1.`, setext `=`) escaped at
/// its start, since a placeholder may begin a line.
fn markdown_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    let digits = name.bytes().take_while(u8::is_ascii_digit).count();
    for (i, c) in name.char_indices() {
        let block_marker = (i == 0 && matches!(c, '-' | '+' | '='))
            || (i == digits && digits > 0 && matches!(c, '.' | ')'));
        if block_marker
            || matches!(
                c,
                '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' | '~' | '&' | '!'
            )
        {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Whether `s` is an enrichment line label: `P` and a number without
/// leading zeros (`P1`, `P12`), as the prompt names corrected lines.
pub fn is_line_label(s: &str) -> bool {
    s.strip_prefix('P').is_some_and(|d| {
        !d.is_empty() && !d.starts_with('0') && d.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Name shown for a person reference of enrichment output: the person's
/// name, else `Unknown` (forgotten, unnamed or unknown).
fn person_name(person: Option<&PersonId>, people: &PeopleLookup) -> String {
    person
        .and_then(|p| people.display_name(p))
        .unwrap_or("Unknown")
        .to_owned()
}

/// Name shown for line label `label` through the generated content's own
/// `line_people`; `Unknown` when it has no entry (the person was forgotten).
fn line_label_name(
    line_people: &BTreeMap<String, PersonId>,
    label: &str,
    people: &PeopleLookup,
) -> String {
    person_name(line_people.get(label), people)
}

/// `text` with each `{{inner}}` that `f` maps replaced by its value; other
/// text (including unmapped braces) is kept verbatim.
pub(crate) fn map_placeholders(text: &str, mut f: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after
            .find("}}")
            .and_then(|end| f(&after[..end]).map(|r| (r, end)))
        {
            Some((r, end)) => {
                out.push_str(&r);
                rest = &after[end + 2..];
            }
            None => {
                out.push('{');
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Replaces placeholders in generated prose with current names: `{{S#}}`
/// with the speaker's ([`speaker_label`]), `{{P#}}` with the line person's
/// through `line_people` (the map of the enrichment or template result the
/// text belongs to: labels are per result, never per meeting), and
/// `{{<person ID>}}` (how the index stores resolved line references) with
/// that person's; anything else is kept verbatim. Used at render and index
/// time only: `meeting.json` keeps the placeholders. Names go in as stored
/// ([`NameStyle::Raw`]); see [`substitute_speakers_as`].
pub fn substitute_speakers(
    meeting: &Meeting,
    line_people: &BTreeMap<String, PersonId>,
    text: &str,
    people: &PeopleLookup,
) -> String {
    substitute_speakers_as(meeting, line_people, text, people, NameStyle::Raw)
}

/// [`substitute_speakers`] writing each substituted name in `style`; the
/// surrounding prose is kept verbatim.
pub fn substitute_speakers_as(
    meeting: &Meeting,
    line_people: &BTreeMap<String, PersonId>,
    text: &str,
    people: &PeopleLookup,
    style: NameStyle,
) -> String {
    map_placeholders(text, |inner| {
        let name = if let Ok(id) = SpeakerId::parse(inner) {
            speaker_label(meeting, Some(id), people)
        } else if is_line_label(inner) {
            line_label_name(line_people, inner, people)
        } else {
            person_name(Some(&PersonId::parse(inner).ok()?), people)
        };
        Some(style.apply(&name))
    })
}

/// Who a decision or action refers to: the speaker's name when it has a
/// speaker, else the provider's name, where a line label (`P1`) shows the
/// line person's name through `line_people` and placeholders are
/// substituted.
pub fn enrichment_who(
    meeting: &Meeting,
    line_people: &BTreeMap<String, PersonId>,
    speaker: Option<SpeakerId>,
    name: Option<&str>,
    people: &PeopleLookup,
) -> Option<String> {
    if let Some(id) = speaker {
        return Some(speaker_label(meeting, Some(id), people));
    }
    let name = name.map(str::trim).filter(|n| !n.is_empty())?;
    Some(if is_line_label(name) {
        line_label_name(line_people, name, people)
    } else {
        substitute_speakers(meeting, line_people, name, people)
    })
}

/// `hh:mm:ss`.
pub fn format_hms(seconds: f64) -> String {
    let t = seconds.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
}

/// `mm:ss` with unbounded minutes, as in the frontmatter language spans.
fn format_ms(seconds: f64) -> String {
    let t = seconds.max(0.0) as u64;
    format!("{:02}:{:02}", t / 60, t % 60)
}

#[derive(Serialize)]
struct MeetingFrontmatter<'a> {
    id: &'a str,
    source: Source,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    started: String,
    duration_min: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    languages: Vec<FmLanguage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    participants: Vec<FmParticipant>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recording: Option<FmRecording<'a>>,
    models: &'a ModelsUsed,
    provenance: FmProvenance,
}

#[derive(Serialize)]
struct FmLanguage<'a> {
    lang: &'a str,
    from: String,
    to: String,
}

#[derive(Serialize)]
struct FmParticipant {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    talk_share: Option<f64>,
}

#[derive(Serialize)]
struct FmRecording<'a> {
    state: RecordingState,
    handling: Handling,
    path: String,
    sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    retention_override: Option<RetentionOverride>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delete_after: Option<String>,
}

/// Version of the `provenance` block of the `meeting.md` frontmatter this
/// build writes. Version 1 is complete: together with `recording.sha256` it
/// names every identity a library lookup matches a meeting by (the
/// recording's hash, its original multi-source content/mapping fingerprint
/// and its `source_id`), an absent field meaning the meeting has none.
/// `pending: true` instead marks a `meeting.md` that no longer vouches for
/// any identity, written before `meeting.json` changes one
/// ([`crate::WriteTx::save_meeting`]).
pub const MEETING_PROVENANCE_VERSION: u32 = 1;

/// The identities of a meeting beyond its recording's hash, so a lookup can
/// still tell whether a meeting whose `meeting.json` cannot be read may be
/// the one it seeks ([`parse_meeting_md_identity`]). Derived like the rest
/// of the frontmatter; the `source_id` is kept only as its SHA-256
/// ([`source_id_sha256`]), so no external ID is copied into the Markdown.
#[derive(Serialize)]
struct FmProvenance {
    version: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_id_sha256: Option<String>,
}

/// The SHA-256 (hex) the `meeting.md` frontmatter records for a meeting's
/// `source_id`.
pub fn source_id_sha256(source_id: &str) -> String {
    crate::schema::sha256_hex(source_id.as_bytes())
}

/// The identities a `meeting.md` frontmatter records
/// ([`parse_meeting_md_identity`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetingMdIdentity {
    /// `recording.sha256`; `None`: the meeting had no recording. Every
    /// version of the frontmatter writes it.
    pub recording_sha256: Option<String>,
    /// The complete `provenance` block; `None` when an earlier build wrote
    /// the file without it, which proves nothing about the other
    /// identities.
    pub provenance: Option<MeetingMdProvenance>,
}

impl MeetingMdIdentity {
    /// What the complete `meeting.md` of `meeting` records.
    pub fn of(meeting: &Meeting) -> Self {
        Self {
            recording_sha256: meeting.recording.as_ref().map(|r| r.sha256.clone()),
            provenance: Some(MeetingMdProvenance {
                source_sha256: meeting
                    .recording
                    .as_ref()
                    .and_then(|r| r.source_sha256.clone()),
                source_id_sha256: meeting.source_id.as_deref().map(source_id_sha256),
            }),
        }
    }

    /// Whether everything this (older) record vouches for is still true of
    /// a meeting whose identity is `now`: the same recording hash and, when
    /// it has the block, the same provenance. A record without the block
    /// vouches for the recording hash alone.
    pub(crate) fn holds_for(&self, now: &Self) -> bool {
        self.recording_sha256 == now.recording_sha256
            && self
                .provenance
                .as_ref()
                .is_none_or(|p| now.provenance.as_ref() == Some(p))
    }
}

/// A complete `provenance` block: absent fields are identities the meeting
/// does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetingMdProvenance {
    /// `recording.source_sha256`: the original multi-source content/mapping
    /// fingerprint, which differs from `recording_sha256` for an encoded copy.
    pub source_sha256: Option<String>,
    /// [`source_id_sha256`] of the meeting's `source_id`.
    pub source_id_sha256: Option<String>,
}

/// How much of a `meeting.md` [`read_meeting_md_identity`] may read: enough
/// for any real frontmatter, which comes first; one that does not close
/// within it proves nothing.
pub const MEETING_MD_IDENTITY_LIMIT: u64 = 1 << 20;

/// [`parse_meeting_md_identity`] of the `meeting.md` at `path`. Processes
/// only through the closing frontmatter line, at most
/// [`MEETING_MD_IDENTITY_LIMIT`] bytes; buffering may prefetch a body prefix.
/// `Err` when the file cannot be read (`NotFound` when there is none).
pub fn read_meeting_md_identity(
    path: &std::path::Path,
) -> std::io::Result<Option<MeetingMdIdentity>> {
    meeting_md_identity_from(std::fs::File::open(path)?)
}

/// [`read_meeting_md_identity`] of what `source` holds: whole lines, at
/// most [`MEETING_MD_IDENTITY_LIMIT`] bytes, stopping after the first line
/// when it does not open a frontmatter, after the line that closes it, or
/// at a line cut short (end of file or of the limit). What was read is
/// judged by [`parse_meeting_md_identity`] alone.
fn meeting_md_identity_from(
    source: impl std::io::Read,
) -> std::io::Result<Option<MeetingMdIdentity>> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(source.take(MEETING_MD_IDENTITY_LIMIT));
    let mut head = Vec::new();
    loop {
        let start = head.len();
        reader.read_until(b'\n', &mut head)?;
        let line = &head[start..];
        let done = if start == 0 {
            line != b"---\n" && line != b"---\r\n"
        } else {
            line.trim_ascii_end() == b"---"
        };
        if done || !line.ends_with(b"\n") {
            break;
        }
    }
    Ok(parse_meeting_md_identity(&head))
}

/// Reads back only the identities from the start of a `meeting.md` (`head`
/// may be cut short: frontmatter that does not close within it gives
/// `None`). Refusal-only evidence about a meeting whose `meeting.json`
/// cannot be read: it never yields a meeting, names or any other field, and
/// `None` proves nothing: no frontmatter, not a meeting's, invalid, a
/// `provenance` block of an unknown version, or one marked `pending` (an
/// identity change may have reached `meeting.json` but not this file, so
/// not even its recording hash still holds).
pub fn parse_meeting_md_identity(head: &[u8]) -> Option<MeetingMdIdentity> {
    #[derive(Deserialize)]
    struct Read {
        // Required, never used: only a meeting's frontmatter counts.
        #[serde(rename = "id")]
        _id: crate::MeetingId,
        #[serde(rename = "started")]
        _started: chrono::DateTime<chrono::FixedOffset>,
        #[serde(default)]
        recording: Option<ReadRecording>,
        #[serde(default)]
        provenance: Option<ReadProvenance>,
    }
    #[derive(Deserialize)]
    struct ReadRecording {
        sha256: String,
    }
    #[derive(Deserialize)]
    struct ReadProvenance {
        version: u32,
        #[serde(default)]
        pending: bool,
        #[serde(default)]
        source_sha256: Option<String>,
        #[serde(default)]
        source_id_sha256: Option<String>,
    }
    let rest = head
        .strip_prefix(b"---\n")
        .or_else(|| head.strip_prefix(b"---\r\n"))?;
    let mut end = None;
    let mut offset = 0;
    for line in rest.split_inclusive(|b| *b == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        if line.trim_ascii_end() == b"---" {
            end = Some(offset);
            break;
        }
        offset += line.len();
    }
    let yaml = std::str::from_utf8(&rest[..end?]).ok()?;
    let read: Read = serde_saphyr::from_str(yaml).ok()?;
    let provenance = match read.provenance {
        None => None,
        Some(p) if p.pending || p.version != MEETING_PROVENANCE_VERSION => return None,
        Some(p) => Some(MeetingMdProvenance {
            source_sha256: p.source_sha256,
            source_id_sha256: p.source_id_sha256,
        }),
    };
    Some(MeetingMdIdentity {
        recording_sha256: read.recording.map(|r| r.sha256),
        provenance,
    })
}

fn to_yaml<T: Serialize>(value: &T) -> Result<String> {
    let mut yaml = serde_saphyr::to_string(value)
        .map_err(|e| Error::Internal(format!("rendering YAML: {e}")))?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    Ok(yaml)
}

/// Participants with their share of talk time: named people (shown speakers
/// merged by person; a group counts once with its total talk), then unnamed
/// shown speakers, then people only line corrections name, then roster
/// participants who were not heard.
fn participants(
    meeting: &Meeting,
    attribution: &[crate::ops::SegmentAttribution],
    people: &PeopleLookup,
) -> Vec<FmParticipant> {
    let total: f64 = meeting.speakers.iter().map(|s| s.talk_s).sum();
    let share = |t: f64| (total > 0.0).then(|| (t / total * 100.0).round() / 100.0);
    let talk = |s: &crate::schema::Speaker| crate::ops::effective_talk_s(meeting, s.id);
    let mut out: Vec<FmParticipant> = Vec::new();
    let mut by_person: Vec<(&PersonId, f64)> = Vec::new();
    for s in crate::ops::effective_speakers(meeting) {
        if let Some(p) = s.named_person() {
            match by_person.iter_mut().find(|(id, _)| *id == p) {
                Some((_, t)) => *t += talk(s),
                None => by_person.push((p, talk(s))),
            }
        }
    }
    for (pid, talk) in &by_person {
        let person = people.get(pid);
        out.push(FmParticipant {
            name: people.display_name(pid).unwrap_or(pid.as_str()).to_owned(),
            email: person.and_then(|p| p.emails.first().cloned()),
            talk_share: share(*talk),
        });
    }
    for s in crate::ops::effective_speakers(meeting).filter(|s| s.named_person().is_none()) {
        out.push(FmParticipant {
            name: unnamed_speaker_label(meeting, s.id),
            email: None,
            talk_share: share(talk(s)),
        });
    }
    for pid in attribution.iter().filter_map(|a| a.line_person()) {
        if by_person.iter().any(|(b, _)| *b == pid) {
            continue;
        }
        by_person.push((pid, 0.0));
        out.push(FmParticipant {
            name: people.display_name(pid).unwrap_or(pid.as_str()).to_owned(),
            email: people.get(pid).and_then(|p| p.emails.first().cloned()),
            talk_share: None,
        });
    }
    for p in &meeting.participants {
        if p.person_id
            .as_ref()
            .is_some_and(|id| by_person.iter().any(|(b, _)| *b == id))
        {
            continue;
        }
        let name = p
            .person_id
            .as_ref()
            .and_then(|id| people.display_name(id))
            .map(str::to_owned)
            .or_else(|| p.name.clone())
            .or_else(|| p.is_self.then(|| "Me".to_owned()))
            .or_else(|| p.email.clone());
        let Some(name) = name else { continue };
        if out.iter().any(|o| o.name == name) {
            continue;
        }
        out.push(FmParticipant {
            name,
            email: p.email.clone(),
            talk_share: None,
        });
    }
    out
}

/// `meeting.md`: YAML frontmatter (design §11 "Meeting record"), the
/// enrichment sections (Summary, Decisions, Actions) when present, and a
/// `## Transcript` with one `[hh:mm:ss] Name: text` line per turn: a run of
/// segments with one shown speaker (a group counts once) and one
/// attribution (a line correction splits the run and names its person, see
/// `ops::effective_attribution`).
pub fn render_meeting_md(meeting: &Meeting, people: &PeopleLookup) -> Result<String> {
    render_meeting_md_as(meeting, people, false)
}

/// [`render_meeting_md`] with its `provenance` marked `pending`: written
/// over a `meeting.md` whose identity `meeting.json` is about to change, so
/// an interruption before the complete file replaces it leaves a file that
/// vouches for nothing ([`parse_meeting_md_identity`]).
pub(crate) fn render_pending_meeting_md(
    meeting: &Meeting,
    people: &PeopleLookup,
) -> Result<String> {
    render_meeting_md_as(meeting, people, true)
}

fn render_meeting_md_as(meeting: &Meeting, people: &PeopleLookup, pending: bool) -> Result<String> {
    let attribution = crate::ops::effective_attribution(meeting);
    let provenance = MeetingMdIdentity::of(meeting)
        .provenance
        .filter(|_| !pending)
        .unwrap_or(MeetingMdProvenance {
            source_sha256: None,
            source_id_sha256: None,
        });
    let fm = MeetingFrontmatter {
        id: meeting.id.as_str(),
        source: meeting.source,
        title: meeting.title.as_deref(),
        started: meeting.started_at.to_rfc3339(),
        duration_min: (meeting.duration_s / 60.0).round() as u64,
        languages: meeting
            .languages
            .iter()
            .map(|l| FmLanguage {
                lang: &l.lang,
                from: format_ms(l.from_s),
                to: format_ms(l.to_s),
            })
            .collect(),
        participants: participants(meeting, &attribution, people),
        tags: meeting.effective_tags(),
        recording: meeting.recording.as_ref().map(|r| FmRecording {
            state: r.state,
            handling: r.handling,
            path: r.path.display().to_string(),
            sha256: &r.sha256,
            retention_override: r.retention_override,
            delete_after: r.delete_after.map(|t| t.to_rfc3339()),
        }),
        models: &meeting.models,
        provenance: FmProvenance {
            version: MEETING_PROVENANCE_VERSION,
            pending,
            source_sha256: provenance.source_sha256,
            source_id_sha256: provenance.source_id_sha256,
        },
    };
    let mut out = String::from("---\n");
    out.push_str(&to_yaml(&fm)?);
    out.push_str("---\n\n");
    render_enrichment(&mut out, meeting, people);
    if !meeting.notes.is_empty() || !meeting.bookmarks.is_empty() {
        out.push_str("## Notes\n\n");
        let mut notes: Vec<(f64, String)> = meeting
            .notes
            .iter()
            .map(|n| (n.at_s, n.text.clone()))
            .chain(meeting.bookmarks.iter().map(|b| {
                (
                    b.at_s,
                    b.label
                        .as_ref()
                        .map_or_else(|| "Bookmark".to_owned(), |l| format!("Bookmark: {l}")),
                )
            }))
            .collect();
        notes.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (at_s, text) in notes {
            out.push_str(&format!("- [{}] {}\n", format_hms(at_s), text));
        }
        out.push('\n');
    }
    out.push_str("## Transcript\n\n");
    for turn in transcript_turns(meeting, &attribution, people) {
        out.push_str(&format!(
            "[{}] {}: {}\n",
            format_hms(turn.start),
            turn.display_name(NameStyle::Markdown),
            turn.text
        ));
    }
    Ok(out)
}

/// One transcript turn as shown: a run of segments with one shown speaker
/// and one attribution.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Turn {
    /// Seconds from the meeting start.
    pub start: f64,
    pub end: f64,
    /// The shown speaker (a grouped member shows as its group), if any.
    pub speaker: Option<SpeakerId>,
    /// The person a line correction attributes the turn to, else the shown
    /// speaker's named person.
    pub person_id: Option<PersonId>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub overlapping_speech: bool,
    pub name: String,
    pub text: String,
}

impl Turn {
    pub(crate) fn display_name(&self, style: NameStyle) -> String {
        crate::transcript::display_label(
            &self.name,
            self.overlapping_speech && self.person_id.is_some(),
            style,
        )
    }
}

/// The transcript as turns in time order: consecutive non-empty segments
/// with one shown speaker (a group counts once) and one attribution merge;
/// a line correction splits the run and names its person. `attribution` is
/// `ops::effective_attribution(meeting)`.
pub fn transcript_turns(
    meeting: &Meeting,
    attribution: &[crate::ops::SegmentAttribution],
    people: &PeopleLookup,
) -> Vec<Turn> {
    let mut segments: Vec<_> = meeting
        .segments
        .iter()
        .zip(attribution)
        .filter(|(s, _)| !s.text.trim().is_empty())
        .collect();
    segments.sort_by(|a, b| a.0.start.total_cmp(&b.0.start));
    let shown = |s: &crate::schema::Segment| {
        s.speaker
            .map(|id| crate::ops::effective_speaker(meeting, id))
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < segments.len() {
        let (first, who) = segments[i];
        let overlapping_speech = !first.speech_overlap.is_empty();
        let mut text = first.text.trim().to_owned();
        let mut end = first.end;
        let mut j = i + 1;
        while j < segments.len()
            && shown(segments[j].0) == shown(first)
            && segments[j].1.line_person() == who.line_person()
            && segments[j].0.speech_overlap.is_empty() == first.speech_overlap.is_empty()
        {
            text.push(' ');
            text.push_str(segments[j].0.text.trim());
            end = end.max(segments[j].0.end);
            j += 1;
        }
        let name = who
            .line_person()
            .and_then(|p| people.display_name(p))
            .map_or_else(
                || {
                    if overlapping_speech {
                        crate::transcript::OVERLAP_LABEL.to_owned()
                    } else {
                        speaker_label(meeting, first.speaker, people)
                    }
                },
                str::to_owned,
            );
        let speaker = shown(first);
        let person_id = who.line_person().cloned().or_else(|| {
            speaker
                .and_then(|s| meeting.speaker(s))
                .and_then(|s| s.named_person())
                .cloned()
        });
        out.push(Turn {
            start: first.start,
            end,
            speaker,
            person_id,
            overlapping_speech,
            name,
            text,
        });
        i = j;
    }
    out
}

/// A template result's Markdown with its placeholders resolved through the
/// result's own `line_people` (never the enrichment's), names written as
/// [`NameStyle::Markdown`].
pub fn template_result_md(
    meeting: &Meeting,
    result: &TemplateResult,
    people: &PeopleLookup,
) -> String {
    substitute_speakers_as(
        meeting,
        &result.line_people,
        result.markdown.trim(),
        people,
        NameStyle::Markdown,
    )
}

/// `templates/<id>.md` beside `meeting.json`: a derived copy of one result
/// with a short header, regenerated on every save.
pub fn render_template_file(
    meeting: &Meeting,
    result: &TemplateResult,
    people: &PeopleLookup,
) -> String {
    let mut out = format!(
        "# {}\n\n",
        NameStyle::Markdown.apply(result.template_name.trim())
    );
    if crate::ops::template_is_stale(meeting, result) {
        out.push_str("> This result predates changes to the transcript.\n\n");
    }
    out.push_str(&template_result_md(meeting, result, people));
    out.push('\n');
    out
}

/// `## Summary`, `## Decisions` and `## Actions`, each only when non-empty.
fn render_enrichment(out: &mut String, meeting: &Meeting, people: &PeopleLookup) {
    let Some(e) = meeting.enrichment.as_ref() else {
        return;
    };
    let lp = &e.line_people;
    let sub = |t: &str| substitute_speakers_as(meeting, lp, t.trim(), people, NameStyle::Markdown);
    let who = |speaker, name: Option<&str>| {
        enrichment_who(meeting, lp, speaker, name, people).map(|n| NameStyle::Markdown.apply(&n))
    };
    // Derived like the index and the app: marked, or for another revision.
    let stale = crate::ops::enrichment_is_stale(meeting);
    if !e.summary.trim().is_empty() || stale {
        out.push_str("## Summary\n\n");
        if stale {
            out.push_str("> This summary predates changes to the transcript.\n\n");
        }
        if !e.summary.trim().is_empty() {
            out.push_str(&sub(&e.summary));
            out.push_str("\n\n");
        }
    }
    if !e.decisions.is_empty() {
        out.push_str("## Decisions\n\n");
        for d in &e.decisions {
            out.push_str(&format!("- [{}] {}", format_hms(d.at_s), sub(&d.text)));
            if let Some(name) = who(d.by_speaker, d.by_name.as_deref()) {
                out.push_str(&format!(" ({name})"));
            }
            out.push('\n');
        }
        out.push('\n');
    }
    if !e.actions.is_empty() {
        out.push_str("## Actions\n\n");
        for a in &e.actions {
            let text = sub(&a.text);
            match a.status {
                ActionStatus::Open => out.push_str(&format!("- [ ] {text}")),
                ActionStatus::Done => out.push_str(&format!("- [x] {text}")),
                ActionStatus::Dropped => out.push_str(&format!("- [ ] ~~{text}~~ (dropped)")),
            }
            let mut details = Vec::new();
            if let Some(name) = who(a.owner_speaker, a.owner_name.as_deref()) {
                details.push(name);
            }
            if let Some(due) = a.due {
                details.push(format!("due {due}"));
            }
            details.push(format_hms(a.at_s));
            if a.stale {
                details.push("no longer in the latest summary".to_owned());
            }
            out.push_str(&format!(" — {}\n", details.join(", ")));
        }
        out.push('\n');
    }
}

/// `person.md`: frontmatter, then the notes after one blank line.
pub fn render_person_md(person: &Person, notes: &str) -> Result<String> {
    let mut out = String::from("---\n");
    out.push_str(&to_yaml(person)?);
    out.push_str("---\n");
    if !notes.is_empty() {
        out.push('\n');
        out.push_str(notes);
    }
    Ok(out)
}

/// Inverse of [`render_person_md`]: the person and the notes body.
pub fn parse_person_md(text: &str) -> Result<(Person, String)> {
    let bad = |why: &str| Error::Schema(format!("person.md: {why}"));
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or_else(|| bad("missing frontmatter"))?;
    let mut offset = 0;
    let mut end = None;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            end = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let (yaml_end, body_start) = end.ok_or_else(|| bad("unterminated frontmatter"))?;
    let person: Person =
        serde_saphyr::from_str(&rest[..yaml_end]).map_err(|e| bad(&e.to_string()))?;
    let body = &rest[body_start..];
    let notes = body
        .strip_prefix("\r\n")
        .or_else(|| body.strip_prefix('\n'))
        .unwrap_or(body);
    Ok((person, notes.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::tests::sample_meeting;

    fn ana() -> Person {
        let mut p = Person::new(PersonId::parse("p-a1a1a1").unwrap(), "Ana Example");
        p.emails.push("ana@example.com".into());
        p
    }

    #[test]
    fn transcript_lines_merge_turns_and_name_speakers() {
        let md = render_meeting_md(&sample_meeting(), &PeopleLookup::new([ana()])).unwrap();
        assert!(md.starts_with("---\n"));
        let transcript = md.split("## Transcript\n\n").nth(1).unwrap();
        let lines: Vec<&str> = transcript.lines().collect();
        assert_eq!(
            lines,
            vec![
                "[00:00:01] Ana Example: Hello there. Shall we start?",
                "[01:01:01] Speaker 2: Yes.",
            ]
        );
    }

    #[test]
    fn unknown_person_and_self_labels() {
        let mut m = sample_meeting();
        let md = render_meeting_md(&m, &PeopleLookup::default()).unwrap();
        assert!(md.contains("[00:00:01] Speaker 1: Hello there."));

        let mut me = Person::new(PersonId::parse("p-a1a1a1").unwrap(), "");
        me.is_self = true;
        let md = render_meeting_md(&m, &PeopleLookup::new([me])).unwrap();
        assert!(md.contains("[00:00:01] Me: Hello there."));

        m.segments[2].speaker = None;
        let md = render_meeting_md(&m, &PeopleLookup::default()).unwrap();
        assert!(md.contains("[01:01:01] Unknown: Yes."));
    }

    #[test]
    fn frontmatter_is_valid_yaml() {
        let md = render_meeting_md(&sample_meeting(), &PeopleLookup::new([ana()])).unwrap();
        let yaml = md
            .strip_prefix("---\n")
            .unwrap()
            .split("\n---\n")
            .next()
            .unwrap();
        let v: serde_json::Value = serde_saphyr::from_str(yaml).unwrap();
        assert_eq!(v["title"], "Roadmap review");
        assert_eq!(v["source"], "import");
        assert_eq!(v["participants"][0]["name"], "Ana Example");
        assert_eq!(v["recording"]["path"], "audio.m4a");
        assert_eq!(v["models"]["asr"], "parakeet-tdt-0.6b-v3");
        assert_eq!(v["id"], "m-7f3a2c");
        assert_eq!(v["duration_min"], 2);
        assert_eq!(v["participants"][0]["talk_share"], 0.6);
        assert_eq!(v["participants"][0]["email"], "ana@example.com");
        assert_eq!(v["participants"][1]["name"], "Speaker 2");
        assert_eq!(v["languages"][0]["to"], "02:05");
    }

    #[test]
    fn frontmatter_provenance_reads_back_every_lookup_identity() {
        let mut m = sample_meeting();
        m.source_id = Some("calendar:ana@example.com/42".into());
        m.recording.as_mut().unwrap().source_sha256 = Some("cd".repeat(32));
        let md = render_meeting_md(&m, &PeopleLookup::new([ana()])).unwrap();
        // The external ID itself never reaches the Markdown.
        assert!(!md.contains("calendar:ana"), "{md}");
        let read = parse_meeting_md_identity(md.as_bytes()).unwrap();
        assert_eq!(
            read,
            MeetingMdIdentity {
                recording_sha256: Some("ab".repeat(32)),
                provenance: Some(MeetingMdProvenance {
                    source_sha256: Some("cd".repeat(32)),
                    source_id_sha256: Some(source_id_sha256("calendar:ana@example.com/42")),
                }),
            }
        );
        // A meeting without those identities says so: the block is still
        // there, complete, with nothing in it.
        let mut bare = sample_meeting();
        bare.recording = None;
        let md = render_meeting_md(&bare, &PeopleLookup::default()).unwrap();
        assert_eq!(
            parse_meeting_md_identity(md.as_bytes()).unwrap(),
            MeetingMdIdentity {
                recording_sha256: None,
                provenance: Some(MeetingMdProvenance {
                    source_sha256: None,
                    source_id_sha256: None,
                }),
            }
        );
    }

    #[test]
    fn frontmatter_without_complete_provenance_proves_nothing_more() {
        let md = render_meeting_md(&sample_meeting(), &PeopleLookup::new([ana()])).unwrap();
        let (head, body) = md.split_once("provenance:\n").unwrap();
        let after: String = body
            .lines()
            .skip_while(|l| l.starts_with(' '))
            .map(|l| format!("{l}\n"))
            .collect();
        // Written before the block existed: only the recording hash.
        let legacy = format!("{head}{after}");
        assert_eq!(
            parse_meeting_md_identity(legacy.as_bytes()).unwrap(),
            MeetingMdIdentity {
                recording_sha256: Some("ab".repeat(32)),
                provenance: None,
            }
        );
        // An unknown version vouches for nothing, not even the recording.
        let future = md.replace("version: 1\n", "version: 2\n");
        assert_eq!(future.matches("version: 2\n").count(), 1, "{future}");
        assert_eq!(parse_meeting_md_identity(future.as_bytes()), None);
        // Nor does a pending one, whatever else it still says.
        let mut m = sample_meeting();
        m.source_id = Some("x".into());
        let pending = render_pending_meeting_md(&m, &PeopleLookup::new([ana()])).unwrap();
        assert!(pending.contains("pending: true"), "{pending}");
        assert!(!pending.contains(&source_id_sha256("x")), "{pending}");
        assert_eq!(parse_meeting_md_identity(pending.as_bytes()), None);
        // The same transcript as the complete file.
        let complete = render_meeting_md(&m, &PeopleLookup::new([ana()])).unwrap();
        assert_eq!(
            pending.split_once("\n---\n").unwrap().1,
            complete.split_once("\n---\n").unwrap().1
        );
        // Cut short, damaged, or not a meeting's: no identity at all.
        let cut = &md.as_bytes()[..md.find("provenance:").unwrap()];
        assert_eq!(parse_meeting_md_identity(cut), None);
        assert_eq!(parse_meeting_md_identity(b"---\nid: [\n---\n"), None);
        let person = render_person_md(&ana(), "notes").unwrap();
        assert_eq!(parse_meeting_md_identity(person.as_bytes()), None);
        assert_eq!(parse_meeting_md_identity(b""), None);
    }

    /// Serves its bytes, then fails every further read.
    struct Only<'a>(&'a [u8]);

    impl std::io::Read for Only<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Err(std::io::Error::other("read past what was served"));
            }
            let n = buf.len().min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    /// Reading a `meeting.md`'s identity stops at the line closing its
    /// frontmatter (nothing after it is read) and at
    /// [`MEETING_MD_IDENTITY_LIMIT`]: a frontmatter closing exactly at the
    /// limit counts, one closing a byte later proves nothing, and a
    /// transcript longer than the limit changes nothing.
    #[test]
    fn the_identity_read_stops_at_the_closing_line_within_the_limit() {
        let mut m = sample_meeting();
        m.source_id = Some("calendar:ana@example.com/42".into());
        let md = render_meeting_md(&m, &PeopleLookup::new([ana()])).unwrap();
        let expected = parse_meeting_md_identity(md.as_bytes()).unwrap();
        let close = md.find("\n---\n").unwrap() + "\n---\n".len();
        let (front, body) = md.split_at(close);
        assert!(body.contains("## Transcript"), "{body}");
        assert_eq!(
            meeting_md_identity_from(Only(front.as_bytes())).unwrap(),
            Some(expected.clone())
        );
        // Not a frontmatter: nothing past the first line is read.
        assert_eq!(meeting_md_identity_from(Only(b"# Notes\n")).unwrap(), None);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::paths::MEETING_MD);
        let limit = usize::try_from(MEETING_MD_IDENTITY_LIMIT).unwrap();
        let mut long = md.clone();
        while long.len() <= limit {
            long.push_str("[00:00:01] Ana Example: and one more thing\n");
        }
        std::fs::write(&path, &long).unwrap();
        assert_eq!(
            read_meeting_md_identity(&path).unwrap(),
            Some(expected.clone())
        );

        // A YAML comment line pads the frontmatter so its closing line ends
        // exactly at the limit, then one byte past it.
        let open = &front[..front.len() - "---\n".len()];
        let padded = |extra: usize| {
            let width = limit - open.len() - "---\n".len() + extra;
            format!("{open}{}\n---\n{body}", "#".repeat(width - 1))
        };
        let at_limit = padded(0);
        assert_eq!(at_limit.find("\n---\n").unwrap() + "\n---\n".len(), limit);
        std::fs::write(&path, &at_limit).unwrap();
        assert_eq!(
            read_meeting_md_identity(&path).unwrap(),
            Some(expected.clone())
        );
        let past = padded(1);
        assert_eq!(parse_meeting_md_identity(past.as_bytes()), Some(expected));
        std::fs::write(&path, &past).unwrap();
        assert_eq!(read_meeting_md_identity(&path).unwrap(), None);

        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            read_meeting_md_identity(&path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    fn enriched() -> Meeting {
        use crate::schema::{Action, Decision, Enrichment, StatusSource};
        let mut m = sample_meeting();
        let s = |n| Some(SpeakerId::new(n).unwrap());
        m.enrichment = Some(Enrichment {
            provider: "claude-cli".into(),
            model: None,
            requested_model: None,
            model_reported: false,
            created_at: crate::schema::now(),
            stale: false,
            revision: crate::index::meeting_window_revision(&m),
            attribution_revision: String::new(),
            line_people: Default::default(),
            summary: "{{S1}} asked {{S2}} and {{S9}} about {{X}} {{S0}}.".into(),
            topics: vec![],
            keywords: vec![],
            tags: vec!["roadmap".into()],
            decisions: vec![Decision {
                id: "d1".into(),
                text: "Start now".into(),
                at_s: 3.0,
                by_speaker: s(1),
                by_name: Some("Someone".into()),
            }],
            actions: vec![
                Action {
                    id: "a1".into(),
                    text: "{{S2}} sends the notes".into(),
                    owner_speaker: s(2),
                    owner_name: None,
                    due: chrono::NaiveDate::from_ymd_opt(2026, 11, 20),
                    at_s: 3661.0,
                    status: ActionStatus::Open,
                    status_source: StatusSource::Llm,
                    status_updated_at: None,
                    original_owner: None,
                    due_user_set: false,
                    stale: false,
                },
                Action {
                    id: "a2".into(),
                    text: "Book a room".into(),
                    owner_speaker: None,
                    owner_name: Some("Bo".into()),
                    due: None,
                    at_s: 1.0,
                    status: ActionStatus::Done,
                    status_source: StatusSource::User,
                    status_updated_at: None,
                    original_owner: None,
                    due_user_set: false,
                    stale: true,
                },
            ],
            instructions: None,
        });
        m
    }

    #[test]
    fn enrichment_sections_resolve_speakers_at_render_time() {
        let m = enriched();
        let md = render_meeting_md(&m, &PeopleLookup::new([ana()])).unwrap();
        let body = md.split("\n---\n\n").nth(1).unwrap();
        let summary = body.find("## Summary").unwrap();
        let decisions = body.find("## Decisions").unwrap();
        let actions = body.find("## Actions").unwrap();
        let transcript = body.find("## Transcript").unwrap();
        assert!(summary < decisions && decisions < actions && actions < transcript);
        assert!(
            !body.contains("predates"),
            "a current enrichment is not stale"
        );
        assert!(
            body.contains("Ana Example asked Speaker 2 and Speaker 9 about {{X}} {{S0}}."),
            "{body}"
        );
        assert!(
            body.contains("- [00:00:03] Start now (Ana Example)\n"),
            "{body}"
        );
        assert!(
            body.contains(
                "- [ ] Speaker 2 sends the notes — Speaker 2, due 2026-11-20, 01:01:01\n"
            ),
            "{body}"
        );
        assert!(
            body.contains("- [x] Book a room — Bo, 00:00:01, no longer in the latest summary\n"),
            "{body}"
        );
        // Placeholders stay in the record.
        assert!(
            m.enrichment
                .as_ref()
                .unwrap()
                .summary
                .starts_with("{{S1}} asked")
        );

        // Naming S2 later changes the rendering without touching the record.
        let mut named = m.clone();
        let s2 = named.speaker_mut(SpeakerId::new(2).unwrap()).unwrap();
        s2.person_id = Some(ana().id);
        s2.assignment = crate::schema::Assignment::Confirmed;
        let md = render_meeting_md(&named, &PeopleLookup::new([ana()])).unwrap();
        assert!(
            md.contains("- [ ] Ana Example sends the notes — Ana Example,"),
            "{md}"
        );
    }

    #[test]
    fn stale_summary_dropped_action_and_frontmatter() {
        let mut m = enriched();
        {
            let e = m.enrichment.as_mut().unwrap();
            e.stale = true;
            e.actions[0].status = ActionStatus::Dropped;
        }
        m.tags = vec!["planning".into()];
        m.tags_removed = vec!["ROADMAP".into()];
        let rec = m.recording.as_mut().unwrap();
        rec.delete_after =
            Some(chrono::DateTime::parse_from_rfc3339("2026-12-13T14:00:00-03:00").unwrap());
        rec.retention_override = Some(RetentionOverride::Keep);
        let md = render_meeting_md(&m, &PeopleLookup::default()).unwrap();
        assert!(
            md.contains("> This summary predates changes to the transcript.\n"),
            "{md}"
        );
        assert!(
            md.contains("- [ ] ~~Speaker 2 sends the notes~~ (dropped) — "),
            "{md}"
        );
        let yaml = md
            .strip_prefix("---\n")
            .unwrap()
            .split("\n---\n")
            .next()
            .unwrap();
        let v: serde_json::Value = serde_saphyr::from_str(yaml).unwrap();
        assert_eq!(v["tags"], serde_json::json!(["planning"]));
        assert_eq!(v["recording"]["delete_after"], "2026-12-13T14:00:00-03:00");
        assert_eq!(v["recording"]["retention_override"], "keep");

        // Without enrichment there are no extra sections.
        let md = render_meeting_md(&sample_meeting(), &PeopleLookup::default()).unwrap();
        assert!(!md.contains("## Summary") && !md.contains("## Actions"));
    }

    fn bo() -> Person {
        Person::new(PersonId::parse("p-b2b2b2").unwrap(), "Bo Example")
    }

    /// Segment 1 ("Shall we start?", said by S1 = Ana) corrected to Bo.
    fn corrected() -> Meeting {
        let mut m = sample_meeting();
        let f = crate::ops::segment_fingerprint(&m.segments[1]);
        m.utterance_people.push(crate::schema::UtterancePerson {
            track: f.track,
            start: f.start,
            end: f.end,
            text_sha256: f.text_sha256,
            person_id: bo().id,
            at: crate::schema::now(),
            live_decision: None,
        });
        m
    }

    #[test]
    fn line_corrections_split_turns_and_name_their_person() {
        let people = PeopleLookup::new([ana(), bo()]);
        let md = render_meeting_md(&corrected(), &people).unwrap();
        let transcript = md.split("## Transcript\n\n").nth(1).unwrap();
        let lines: Vec<&str> = transcript.lines().collect();
        assert_eq!(
            lines,
            vec![
                "[00:00:01] Ana Example: Hello there.",
                "[00:00:03] Bo Example: Shall we start?",
                "[01:01:01] Speaker 2: Yes.",
            ]
        );
        // A correction whose text no longer matches applies nowhere.
        let mut changed = corrected();
        changed.segments[1].text = "Shall we begin?".into();
        let md = render_meeting_md(&changed, &people).unwrap();
        assert!(md.contains("[00:00:01] Ana Example: Hello there. Shall we begin?"));
    }

    #[test]
    fn line_labels_resolve_through_line_people() {
        let mut m = enriched();
        let e = m.enrichment.as_mut().unwrap();
        e.line_people.insert("P1".into(), bo().id);
        e.summary = "{{P1}} agreed; {{P2}} and {{S1}} left; {{p-b2b2b2}} too.".into();
        e.decisions[0].by_speaker = None;
        e.decisions[0].by_name = Some("P1".into());
        e.actions[1].owner_name = Some("P2".into());
        let people = PeopleLookup::new([ana(), bo()]);
        let md = render_meeting_md(&m, &people).unwrap();
        assert!(
            md.contains("Bo Example agreed; Unknown and Ana Example left; Bo Example too."),
            "{md}"
        );
        assert!(md.contains("- [00:00:03] Start now (Bo Example)\n"), "{md}");
        assert!(md.contains("- [x] Book a room — Unknown, 00:00:01"), "{md}");
        assert!(is_line_label("P12") && !is_line_label("P0") && !is_line_label("P"));
        assert!(!is_line_label("P1x") && !is_line_label("S1"));
    }

    #[test]
    fn person_md_round_trip() {
        let mut p = ana();
        p.aliases = vec!["Ana E.".into(), "yes".into()];
        p.is_self = true;
        for notes in ["", "Likes tea.\n\n- second line: with colon\n"] {
            let text = render_person_md(&p, notes).unwrap();
            let (back, back_notes) = parse_person_md(&text).unwrap();
            assert_eq!(back, p);
            assert_eq!(back_notes, notes);
        }
        assert!(parse_person_md("no frontmatter").is_err());
        assert!(parse_person_md("---\nid: p-000001\n").is_err());
    }
}

#[cfg(test)]
mod name_safety_tests {
    use super::*;
    use crate::export::{ExportFormat, actions_table_markdown, available_sections, render_export};
    use crate::schema::Assignment;
    use crate::schema::tests::sample_meeting;

    /// A name an older or imported record carries, never validated: it
    /// tries to open a heading, a list item and HTML.
    const FORGED: &str = "Ana\n## Decisions\n- [x] <b>forged</b>";
    /// [`FORGED`] as Markdown output writes it.
    const SHOWN: &str = r"Ana \#\# Decisions - \[x\] \<b\>forged\</b\>";

    fn legacy_people() -> PeopleLookup {
        PeopleLookup::new([Person::new(PersonId::parse("p-a1a1a1").unwrap(), FORGED)])
    }

    /// The sample meeting with an enrichment naming S1 (the legacy name).
    fn enriched() -> Meeting {
        use crate::schema::{Action, Decision, Enrichment, StatusSource};
        let mut m = sample_meeting();
        m.enrichment = Some(Enrichment {
            provider: "claude-cli".into(),
            model: None,
            requested_model: None,
            model_reported: false,
            created_at: crate::schema::now(),
            stale: false,
            revision: crate::index::meeting_window_revision(&m),
            attribution_revision: String::new(),
            line_people: Default::default(),
            summary: "{{S1}} asked {{S2}}.".into(),
            topics: vec![],
            keywords: vec![],
            tags: vec![],
            decisions: vec![Decision {
                id: "d1".into(),
                text: "Start now".into(),
                at_s: 3.0,
                by_speaker: SpeakerId::new(1).ok(),
                by_name: None,
            }],
            actions: vec![Action {
                id: "a1".into(),
                text: "{{S1}} sends the notes".into(),
                owner_speaker: SpeakerId::new(1).ok(),
                owner_name: None,
                due: None,
                at_s: 3.0,
                status: ActionStatus::Open,
                status_source: StatusSource::Llm,
                status_updated_at: None,
                original_owner: None,
                due_user_set: false,
                stale: false,
            }],
            instructions: None,
        });
        m
    }

    fn headings(text: &str) -> Vec<&str> {
        text.lines().filter(|l| l.starts_with('#')).collect()
    }

    #[test]
    fn name_styles_neutralise_controls_and_markdown() {
        assert_eq!(NameStyle::Raw.apply(FORGED), FORGED);
        assert_eq!(
            NameStyle::Plain.apply(FORGED),
            "Ana ## Decisions - [x] <b>forged</b>"
        );
        assert_eq!(NameStyle::Markdown.apply(FORGED), SHOWN);
        // Bidirectional overrides and separators are controls too.
        assert_eq!(NameStyle::Plain.apply("A\u{202E}B\u{2028}C\t"), "A B C");
        // Block markers only matter where a line may start.
        assert_eq!(NameStyle::Markdown.apply("1. Bo"), r"1\. Bo");
        assert_eq!(NameStyle::Markdown.apply("12) Bo"), r"12\) Bo");
        assert_eq!(NameStyle::Markdown.apply("- Bo-Ana"), r"\- Bo-Ana");
        assert_eq!(NameStyle::Markdown.apply("=== Bo"), r"\=== Bo");
        assert_eq!(NameStyle::Markdown.apply("Ana E. & Co"), r"Ana E. \& Co");
        // Ordinary names are unchanged.
        assert_eq!(NameStyle::Markdown.apply("Ana Example"), "Ana Example");
    }

    #[test]
    fn legacy_names_cannot_forge_meeting_md_structure() {
        let m = enriched();
        let md = render_meeting_md(&m, &legacy_people()).unwrap();
        let body = md.split("\n---\n\n").nth(1).unwrap();
        assert_eq!(
            headings(body),
            ["## Summary", "## Decisions", "## Actions", "## Transcript"],
            "{body}"
        );
        assert!(
            body.contains(&format!(
                "[00:00:01] {SHOWN}: Hello there. Shall we start?\n"
            )),
            "{body}"
        );
        assert!(
            body.contains(&format!("{SHOWN} asked Speaker 2.")),
            "{body}"
        );
        assert!(
            body.contains(&format!("- [00:00:03] Start now ({SHOWN})\n")),
            "{body}"
        );
        assert!(
            body.contains(&format!(
                "- [ ] {SHOWN} sends the notes — {SHOWN}, 00:00:03\n"
            )),
            "{body}"
        );
        // The frontmatter is YAML: the name keeps its exact value there.
        let yaml = md
            .strip_prefix("---\n")
            .unwrap()
            .split("\n---\n")
            .next()
            .unwrap();
        let v: serde_json::Value = serde_saphyr::from_str(yaml).unwrap();
        assert_eq!(v["participants"][0]["name"], FORGED);
        // The record and the transcript evidence are untouched.
        assert_eq!(m.segments[0].text, "Hello there.");
        assert!(m.enrichment.as_ref().unwrap().summary.starts_with("{{S1}}"));
    }

    #[test]
    fn template_results_escape_names_only() {
        let mut m = enriched();
        m.template_results.push(TemplateResult {
            template_id: "notes".into(),
            template_name: "Notes\n# Forged".into(),
            instructions_sha256: String::new(),
            provider: "agent-a".into(),
            model: None,
            requested_model: None,
            model_reported: false,
            created_at: crate::schema::now(),
            revision: crate::index::meeting_window_revision(&m),
            attribution_revision: String::new(),
            line_people: Default::default(),
            stale: false,
            markdown: "## Lead\n\n{{S1}} **owns** it.".into(),
        });
        let r = &m.template_results[0];
        let people = legacy_people();
        assert_eq!(
            template_result_md(&m, r, &people),
            format!("## Lead\n\n{SHOWN} **owns** it.")
        );
        let file = render_template_file(&m, r, &people);
        assert_eq!(headings(&file), [r"# Notes \# Forged", "## Lead"]);
    }

    #[test]
    fn legacy_names_cannot_forge_export_structure() {
        let m = enriched();
        let people = legacy_people();
        let all = available_sections(&m);
        let md = render_export(&m, &people, &all, ExportFormat::Markdown, true).unwrap();
        assert_eq!(
            headings(&md.content),
            [
                "# Roadmap review",
                "## Summary",
                "## Decisions",
                "## Action items",
                "## Transcript"
            ],
            "{}",
            md.content
        );
        assert!(
            md.content
                .contains(&format!("[00:00:01] {SHOWN}: Hello there."))
        );
        assert!(
            md.content
                .contains(&format!("- [00:00:03] Start now ({SHOWN})\n"))
        );
        let one_line = "Ana ## Decisions - [x] <b>forged</b>";
        let txt = render_export(&m, &people, &all, ExportFormat::Text, true).unwrap();
        assert!(
            txt.content.contains(&format!(
                "[00:00:01] {one_line}: Hello there. Shall we start?\n"
            )),
            "{}",
            txt.content
        );
        assert!(
            !txt.content
                .lines()
                .any(|l| l == "## Decisions" || l == "- [x] <b>forged</b>")
        );
        // JSON carries the name as stored.
        let json = render_export(&m, &people, &all, ExportFormat::Json, true).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json.content).unwrap();
        assert_eq!(v["transcript"][0]["speaker"], FORGED);
        assert_eq!(v["decisions"]["items"][0]["by"], FORGED);
    }

    #[test]
    fn table_cells_escape_a_name_pipe_once() {
        let m = enriched();
        let people = PeopleLookup::new([Person::new(
            PersonId::parse("p-a1a1a1").unwrap(),
            "Bo | Ops",
        )]);
        let table = actions_table_markdown(&m, &people);
        assert!(
            table.contains(r"| Open | Bo \| Ops sends the notes | Bo \| Ops |  | 00:00:03 |"),
            "{table}"
        );
    }

    #[test]
    fn unnamed_mic_speakers_keep_their_label_everywhere() {
        let mut m = sample_meeting();
        let s2 = m.speaker_mut(SpeakerId::new(2).unwrap()).unwrap();
        s2.track = "mic".into();
        assert_eq!(s2.assignment, Assignment::None);
        let people = PeopleLookup::default();
        let md = render_meeting_md(&m, &people).unwrap();
        assert!(md.contains("[01:01:01] Unknown (mic): Yes."), "{md}");
        let yaml = md
            .strip_prefix("---\n")
            .unwrap()
            .split("\n---\n")
            .next()
            .unwrap();
        let v: serde_json::Value = serde_saphyr::from_str(yaml).unwrap();
        assert_eq!(v["participants"][1]["name"], "Unknown (mic)");
        assert_eq!(
            unnamed_speaker_label(&m, SpeakerId::new(2).unwrap()),
            "Unknown (mic)"
        );
        // In a room the microphone hears everyone: a numbered speaker.
        m.setup = Some("in_person".into());
        assert_eq!(
            unnamed_speaker_label(&m, SpeakerId::new(2).unwrap()),
            "Speaker 2"
        );
        let md = render_meeting_md(&m, &people).unwrap();
        assert!(md.contains("[01:01:01] Speaker 2: Yes."), "{md}");
    }
}
