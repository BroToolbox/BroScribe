//! Import manifests, versions 1 and 2 (design §6.5). The table in the design is the
//! contract: a different `format` or a higher `version` is rejected, unknown
//! fields are ignored, and problems in one meeting are collected without
//! aborting the batch.

use std::path::{Path, PathBuf};

use broscribe_core::config::SetupProgress;
use broscribe_core::names::validate_name;
use broscribe_core::schema::sha256_hex;
use broscribe_core::schema::{Person, TrackRole};
use broscribe_core::{Error, Result};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use broscribe_core::import_files::MANIFEST_FORMAT as FORMAT;
pub const VERSION: u64 = 2;

/// Largest `|offset_s|` of a v2 source on the meeting timeline: four hours.
/// An offset only places a source, but every analysis, encode and playback
/// of the meeting pays for the silence it adds, so an offset never costs
/// more than a four-hour recording does (16 kHz f32: about 0.92 GB per
/// padded track). Longer gaps between sources are not one meeting.
pub const MAX_SOURCE_OFFSET_S: f64 = 4.0 * 3600.0;

/// A parsed manifest: valid meetings plus the problems found in the others.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    /// Batch ID: the first 16 hex digits of SHA-256 over the manifest's
    /// canonical path and content ([`read`]), or its content alone
    /// ([`parse`]). Re-running the same file resumes the same batch.
    pub id: String,
    pub generated_at: Option<DateTime<FixedOffset>>,
    pub people: Vec<ManifestPerson>,
    pub meetings: Vec<ManifestMeeting>,
    pub issues: Vec<ManifestIssue>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ManifestPerson {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ManifestMeeting {
    /// Position in `meetings[]`.
    pub index: usize,
    pub audio_path: PathBuf,
    pub sources: Vec<ManifestSource>,
    /// Expand the container's audio tracks after probing, before batch preflight.
    pub per_participant: bool,
    pub source_id: Option<String>,
    pub title: Option<String>,
    pub started_at: Option<DateTime<FixedOffset>>,
    pub ended_at: Option<DateTime<FixedOffset>>,
    pub duration_s: Option<f64>,
    pub language: Option<String>,
    pub participants: Vec<ManifestParticipant>,
    pub hints: Vec<SpeakerHint>,
}

impl ManifestMeeting {
    pub(crate) fn expand_tracks(
        &mut self,
        decoders: &broscribe_platform::DecoderChain,
    ) -> Result<()> {
        if !self.per_participant || !self.sources.is_empty() {
            return Ok(());
        }
        let info = decoders.probe(&self.audio_path)?;
        let count = usize::try_from(info.audio_tracks)
            .unwrap_or(0)
            .max(info.tracks.len());
        if count == 0 {
            return Err(Error::InvalidManifest(
                "per-participant container has no audio tracks".into(),
            ));
        }
        self.sources = (0..count)
            .map(|i| ManifestSource {
                path: self.audio_path.clone(),
                audio_track_index: Some(i),
                role: TrackRole::Participant,
                person_hint: None,
                offset_s: 0.0,
            })
            .collect();
        Ok(())
    }
}

/// One independently decoded source on the meeting timeline (manifest v2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestSource {
    pub path: PathBuf,
    #[serde(default)]
    pub audio_track_index: Option<usize>,
    pub role: TrackRole,
    #[serde(default)]
    pub person_hint: Option<SourcePersonHint>,
    #[serde(default)]
    pub offset_s: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SourcePersonHint {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub is_self: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ManifestParticipant {
    pub name: Option<String>,
    pub email: Option<String>,
    pub is_self: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SpeakerHint {
    pub name: Option<String>,
    pub email: Option<String>,
    pub is_self: bool,
    /// `(start, end)` seconds from the start of the file, `0 ≤ start < end`.
    pub ranges: Vec<(f64, f64)>,
}

impl SpeakerHint {
    /// Key used to group one person's hints before they are matched to a
    /// person: the folded e-mail, else the folded name, else `self`.
    pub fn person_key(&self) -> String {
        person_key(self.name.as_deref(), self.email.as_deref(), self.is_self)
    }
}

/// Folded e-mail, else folded name, else `self` (else empty).
pub fn person_key(name: Option<&str>, email: Option<&str>, is_self: bool) -> String {
    if let Some(e) = email.map(fold).filter(|e| !e.is_empty()) {
        return e;
    }
    if let Some(n) = name.map(fold).filter(|n| !n.is_empty()) {
        return n;
    }
    if is_self {
        "self".to_owned()
    } else {
        String::new()
    }
}

/// A problem with one entry; the rest of the manifest is still imported.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ManifestIssue {
    /// Index in `meetings[]`, or `None` for a `people[]` entry.
    pub meeting: Option<usize>,
    /// Index in `people[]` for people entries.
    pub person: Option<usize>,
    pub source_id: Option<String>,
    /// `invalid_manifest`, or `unsupported` for features of later milestones.
    pub code: &'static str,
    pub message: String,
}

impl Manifest {
    /// Marks the participants and hints that are the user by the name and
    /// e-mails of `setup` (`setup --name/--email`) as `is_self`, and drops
    /// the user from `people[]` so no second, non-self person is created.
    pub fn mark_self(&mut self, setup: &SetupProgress) {
        if setup.name.is_none() && setup.emails.is_empty() {
            return;
        }
        self.people
            .retain(|p| !is_user(setup, p.name.as_deref(), p.email.as_deref()));
        for m in &mut self.meetings {
            for p in &mut m.participants {
                p.is_self |= is_user(setup, p.name.as_deref(), p.email.as_deref());
            }
            for h in &mut m.hints {
                h.is_self |= is_user(setup, h.name.as_deref(), h.email.as_deref());
            }
            for h in m.sources.iter_mut().filter_map(|s| s.person_hint.as_mut()) {
                h.is_self |= is_user(setup, h.name.as_deref(), h.email.as_deref());
            }
        }
    }
}

/// Reads and parses a manifest file.
pub fn read(path: &Path) -> Result<Manifest> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io_at(path, e))?;
    let mut manifest = parse(&text)?;
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    manifest.id = batch_id(Some(&abs), &text);
    Ok(manifest)
}

fn batch_id(path: Option<&Path>, text: &str) -> String {
    let mut bytes = Vec::with_capacity(text.len() + 256);
    if let Some(p) = path {
        bytes.extend_from_slice(p.as_os_str().as_encoded_bytes());
        bytes.push(0);
    }
    bytes.extend_from_slice(text.as_bytes());
    sha256_hex(&bytes)[..16].to_owned()
}

#[derive(Deserialize)]
struct RawPerson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Deserialize)]
struct RawParticipant {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    is_self: bool,
}

#[derive(Deserialize)]
struct RawHint {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    is_self: bool,
    #[serde(default)]
    ranges: Vec<(f64, f64)>,
}

#[derive(Deserialize)]
struct RawMeeting {
    #[serde(default)]
    audio_path: Option<String>,
    #[serde(default)]
    sources: Vec<ManifestSource>,
    #[serde(default)]
    source_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    ended_at: Option<String>,
    #[serde(default)]
    duration_s: Option<f64>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    tracks: Option<String>,
    #[serde(default)]
    participants: Vec<RawParticipant>,
    #[serde(default)]
    speaker_hints: Vec<RawHint>,
}

/// Parses manifest JSON. Errors (`invalid_manifest`) only for the file as a
/// whole: not JSON, wrong `format`, missing or higher `version`, no
/// `meetings[]`.
pub fn parse(text: &str) -> Result<Manifest> {
    let root: Value = serde_json::from_str(text)
        .map_err(|e| Error::InvalidManifest(format!("not valid JSON: {e}")))?;
    let obj = root
        .as_object()
        .ok_or_else(|| Error::InvalidManifest("the manifest must be a JSON object".into()))?;
    match obj.get("format").and_then(Value::as_str) {
        Some(FORMAT) => {}
        Some(other) => {
            return Err(Error::InvalidManifest(format!(
                "format {other:?} is not {FORMAT:?}"
            )));
        }
        None => {
            return Err(Error::InvalidManifest(format!(
                "missing \"format\": {FORMAT:?}"
            )));
        }
    }
    let version = obj
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::InvalidManifest("missing integer \"version\"".into()))?;
    if version > VERSION {
        return Err(Error::InvalidManifest(format!(
            "manifest version {version} is newer than this BroScribe reads ({VERSION})"
        )));
    }
    if version < 1 {
        return Err(Error::InvalidManifest(format!(
            "unknown manifest version {version}"
        )));
    }
    let generated_at = match obj.get("generated_at") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .ok_or_else(|| {
                    Error::InvalidManifest("\"generated_at\" is not an RFC 3339 time".into())
                })?,
        ),
    };
    let meetings = obj
        .get("meetings")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidManifest("missing \"meetings\" array".into()))?;

    let mut issues = Vec::new();
    let mut people = Vec::new();
    match obj.get("people") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            for (i, v) in list.iter().enumerate() {
                let issue = |message: String| ManifestIssue {
                    meeting: None,
                    person: Some(i),
                    source_id: None,
                    code: "invalid_manifest",
                    message,
                };
                match serde_json::from_value::<RawPerson>(v.clone()) {
                    Ok(p) => {
                        let name = non_empty(p.name);
                        let email = non_empty(p.email);
                        if name.is_none() && email.is_none() {
                            issues.push(issue("person has neither name nor email".into()));
                        } else {
                            people.push(ManifestPerson { name, email });
                        }
                    }
                    Err(e) => issues.push(issue(format!("people[{i}]: {e}"))),
                }
            }
        }
        Some(_) => {
            return Err(Error::InvalidManifest("\"people\" must be an array".into()));
        }
    }
    // A person this list would create under an invalid name refuses the
    // whole manifest, before any person is written.
    people_names(&people)?;

    let mut out = Vec::new();
    let mut source_ids = std::collections::BTreeSet::new();
    for (i, v) in meetings.iter().enumerate() {
        let source_id = v
            .get("source_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match parse_meeting(i, v, version) {
            Ok(m) => {
                if let Some(id) = &m.source_id
                    && !source_ids.insert(id.clone())
                {
                    issues.push(ManifestIssue {
                        meeting: Some(i),
                        person: None,
                        source_id: Some(id.clone()),
                        code: "invalid_manifest",
                        message: format!("meetings[{i}]: duplicate source_id {id:?}"),
                    });
                } else {
                    out.push(m);
                }
            }
            Err((code, message)) => issues.push(ManifestIssue {
                meeting: Some(i),
                person: None,
                source_id,
                code,
                message,
            }),
        }
    }
    Ok(Manifest {
        id: batch_id(None, text),
        generated_at,
        people,
        meetings: out,
        issues,
    })
}

type Problem = (&'static str, String);

fn invalid(message: impl Into<String>) -> Problem {
    ("invalid_manifest", message.into())
}

fn parse_meeting(
    index: usize,
    v: &Value,
    version: u64,
) -> std::result::Result<ManifestMeeting, Problem> {
    let raw: RawMeeting = serde_json::from_value(v.clone())
        .map_err(|e| invalid(format!("meetings[{index}]: {e}")))?;
    if version < 2 && !raw.sources.is_empty() {
        return Err(invalid(format!(
            "meetings[{index}]: sources requires version 2"
        )));
    }
    for (j, source) in raw.sources.iter().enumerate() {
        if !source.path.is_absolute()
            || !source.offset_s.is_finite()
            || !matches!(
                source.role,
                TrackRole::Mic | TrackRole::System | TrackRole::Participant
            )
        {
            return Err(invalid(format!(
                "meetings[{index}].sources[{j}]: needs absolute path, finite offset and mic/system/participant role"
            )));
        }
        if source.offset_s.abs() > MAX_SOURCE_OFFSET_S {
            return Err(invalid(format!(
                "meetings[{index}].sources[{j}]: offset_s {} is outside ±{MAX_SOURCE_OFFSET_S} s",
                source.offset_s
            )));
        }
        if source.person_hint.as_ref().is_some_and(|h| {
            !h.is_self
                && h.name.as_deref().is_none_or(|n| n.trim().is_empty())
                && h.email.as_deref().is_none_or(|e| e.trim().is_empty())
        }) {
            return Err(invalid(format!(
                "meetings[{index}].sources[{j}]: empty person_hint"
            )));
        }
    }
    let audio_path = non_empty(raw.audio_path)
        .map(PathBuf::from)
        .or_else(|| raw.sources.first().map(|s| s.path.clone()))
        .ok_or_else(|| invalid(format!("meetings[{index}]: missing audio_path or sources")))?;
    if !audio_path.is_absolute() {
        return Err(invalid(format!(
            "meetings[{index}]: \"audio_path\" must be absolute: {}",
            audio_path.display()
        )));
    }
    match raw.tracks.as_deref() {
        None | Some("mixed") => {}
        Some("per-participant") => {}
        Some(other) => {
            return Err(invalid(format!(
                "meetings[{index}]: unknown \"tracks\" value {other:?}"
            )));
        }
    }
    let time = |field: &str, s: Option<String>| -> std::result::Result<_, Problem> {
        match non_empty(s) {
            None => Ok(None),
            Some(s) => DateTime::parse_from_rfc3339(&s).map(Some).map_err(|e| {
                invalid(format!(
                    "meetings[{index}]: \"{field}\" is not RFC 3339 with a time zone: {e}"
                ))
            }),
        }
    };
    let started_at = time("started_at", raw.started_at)?;
    let ended_at = time("ended_at", raw.ended_at)?;
    if let Some(d) = raw.duration_s
        && !(d.is_finite() && d >= 0.0)
    {
        return Err(invalid(format!(
            "meetings[{index}]: invalid \"duration_s\""
        )));
    }
    let mut participants = Vec::new();
    for (j, p) in raw.participants.into_iter().enumerate() {
        let name = non_empty(p.name);
        let email = non_empty(p.email);
        if name.is_none() && email.is_none() {
            return Err(invalid(format!(
                "meetings[{index}].participants[{j}]: needs a name or an email"
            )));
        }
        participants.push(ManifestParticipant {
            name,
            email,
            is_self: p.is_self,
        });
    }
    let mut hints = Vec::new();
    for (j, h) in raw.speaker_hints.into_iter().enumerate() {
        let name = non_empty(h.name);
        let email = non_empty(h.email);
        if !h.is_self && name.is_none() {
            return Err(invalid(format!(
                "meetings[{index}].speaker_hints[{j}]: \"name\" is required unless is_self"
            )));
        }
        for &(a, b) in &h.ranges {
            if !(a.is_finite() && b.is_finite() && a >= 0.0 && a < b) {
                return Err(invalid(format!(
                    "meetings[{index}].speaker_hints[{j}]: range [{a}, {b}] needs 0 ≤ start < end"
                )));
            }
        }
        hints.push(SpeakerHint {
            name,
            email,
            is_self: h.is_self,
            ranges: h.ranges,
        });
    }
    let per_participant =
        raw.tracks.as_deref() == Some("per-participant") && raw.sources.is_empty();
    let meeting = ManifestMeeting {
        index,
        audio_path,
        sources: raw.sources,
        per_participant,
        source_id: non_empty(raw.source_id),
        title: non_empty(raw.title),
        started_at,
        ended_at,
        duration_s: raw.duration_s,
        language: non_empty(raw.language),
        participants,
        hints,
    };
    check_hint_names(&meeting).map_err(invalid)?;
    Ok(meeting)
}

/// The name a `people[]` entry, speaker hint or source `person_hint` gives
/// a person it creates: its name, else its e-mail unless it is the user
/// (who is named by `setup`, else "Me"). Checked as every person name is
/// ([`validate_name`]) and trimmed, never rewritten; `Ok(None)` when it
/// brings neither. `Err` is the reason.
fn new_person_name<'a>(
    name: Option<&'a str>,
    email: Option<&'a str>,
    is_self: bool,
) -> std::result::Result<Option<&'a str>, String> {
    let given = |s: Option<&'a str>| s.filter(|s| !s.trim().is_empty());
    given(name)
        .or_else(|| given(email).filter(|_| !is_self))
        .map(|n| {
            validate_name(n).map_err(|e| match e {
                Error::InvalidInput(reason) => reason,
                e => e.to_string(),
            })
        })
        .transpose()
}

/// The names `people` create their persons under ([`new_person_name`]), in
/// order. One invalid name, or an entry naming nobody, refuses them all.
pub(crate) fn people_names(people: &[ManifestPerson]) -> Result<Vec<&str>> {
    let mut names = Vec::with_capacity(people.len());
    for (i, p) in people.iter().enumerate() {
        let name = new_person_name(p.name.as_deref(), p.email.as_deref(), false)
            .map_err(|reason| Error::InvalidManifest(format!("people[{i}]: {reason}")))?
            .ok_or_else(|| {
                Error::InvalidManifest(format!("people[{i}]: person has neither name nor email"))
            })?;
        names.push(name);
    }
    Ok(names)
}

/// Whether one meeting's speaker hints and source `person_hint`s would
/// create every person under a valid name ([`new_person_name`]); `Err`
/// names the first that would not.
pub(crate) fn check_hint_names(entry: &ManifestMeeting) -> std::result::Result<(), String> {
    let index = entry.index;
    for (j, h) in entry.hints.iter().enumerate() {
        new_person_name(h.name.as_deref(), h.email.as_deref(), h.is_self)
            .map_err(|reason| format!("meetings[{index}].speaker_hints[{j}]: {reason}"))?;
    }
    for (j, source) in entry.sources.iter().enumerate() {
        if let Some(h) = &source.person_hint {
            new_person_name(h.name.as_deref(), h.email.as_deref(), h.is_self).map_err(
                |reason| format!("meetings[{index}].sources[{j}].person_hint: {reason}"),
            )?;
        }
    }
    Ok(())
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// Case folding for name and e-mail matching (same rule as the library).
pub fn fold(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Matches a hint, participant or `people[]` entry to a known person: by
/// e-mail (case-insensitive) when both sides have one, otherwise by
/// case-folded name or alias.
pub fn match_person<'p>(
    people: &'p [Person],
    name: Option<&str>,
    email: Option<&str>,
) -> Option<&'p Person> {
    let email = email.map(fold).filter(|e| !e.is_empty());
    let name = name.map(fold).filter(|n| !n.is_empty());
    people.iter().find(|p| match (&email, p.emails.is_empty()) {
        (Some(e), false) => p.emails.iter().any(|pe| fold(pe) == *e),
        _ => name
            .as_ref()
            .is_some_and(|n| fold(&p.name) == *n || p.aliases.iter().any(|a| fold(a) == *n)),
    })
}

/// Whether a hint, participant or `people[]` entry is the user, by the name
/// and e-mails saved by `setup`; same rules as [`match_person`].
pub fn is_user(setup: &SetupProgress, name: Option<&str>, email: Option<&str>) -> bool {
    let email = email.map(fold).filter(|e| !e.is_empty());
    match (&email, setup.emails.is_empty()) {
        (Some(e), false) => setup.emails.iter().any(|x| fold(x) == *e),
        _ => name
            .map(fold)
            .filter(|n| !n.is_empty())
            .is_some_and(|n| setup.name.as_deref().is_some_and(|m| fold(m) == n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_core::PersonId;

    fn wrap(meetings: serde_json::Value) -> String {
        serde_json::json!({"format": FORMAT, "version": 1, "meetings": meetings}).to_string()
    }

    fn audio_path(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"synthetic media").unwrap();
        path
    }

    #[test]
    fn duplicate_source_id_is_a_per_meeting_issue() {
        let dir = tempfile::tempdir().unwrap();
        let m = parse(&wrap(serde_json::json!([
            {"audio_path": audio_path(dir.path(), "a.wav"), "source_id": "same"},
            {"audio_path": audio_path(dir.path(), "b.wav"), "source_id": "same"},
            {"audio_path": audio_path(dir.path(), "c.wav"), "source_id": "other"},
        ])))
        .unwrap();
        assert_eq!(m.meetings.len(), 2);
        assert_eq!(m.issues.len(), 1);
        assert_eq!(m.issues[0].meeting, Some(1));
        assert_eq!(m.issues[0].code, "invalid_manifest");
    }

    #[test]
    fn v1_per_participant_container_is_accepted_for_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let m = parse(&wrap(serde_json::json!([{
            "audio_path": audio_path(dir.path(), "tracks.m4a"),
            "tracks": "per-participant",
        }])))
        .unwrap();
        assert!(m.issues.is_empty());
        assert!(m.meetings[0].per_participant);
    }

    #[test]
    fn source_offsets_beyond_the_explicit_limit_are_per_meeting_issues() {
        let dir = tempfile::tempdir().unwrap();
        let audio = audio_path(dir.path(), "a.wav");
        let source = |offset: f64| serde_json::json!({"sources": [{"path": audio, "role": "participant", "offset_s": offset}]});
        let text = serde_json::json!({
            "format": FORMAT, "version": 2,
            "meetings": [
                source(MAX_SOURCE_OFFSET_S),
                source(-MAX_SOURCE_OFFSET_S),
                source(MAX_SOURCE_OFFSET_S + 0.5),
                source(-1e300),
                source(1e300),
                source(f64::MAX),
            ]
        })
        .to_string();
        let m = parse(&text).unwrap();
        assert_eq!(m.meetings.len(), 2, "{:?}", m.issues);
        let idx: Vec<usize> = m.issues.iter().filter_map(|i| i.meeting).collect();
        assert_eq!(idx, vec![2, 3, 4, 5]);
        assert!(m.issues.iter().all(|i| i.code == "invalid_manifest"));
        assert!(
            m.issues[0].message.contains("offset_s"),
            "{}",
            m.issues[0].message
        );
    }

    #[test]
    fn rejects_other_format_and_newer_version() {
        let e = parse(r#"{"format":"other","version":1,"meetings":[]}"#).unwrap_err();
        assert_eq!(e.code(), "invalid_manifest");
        let e = parse(&format!(
            r#"{{"format":"{FORMAT}","version":3,"meetings":[]}}"#
        ))
        .unwrap_err();
        assert!(e.to_string().contains("newer"));
        assert_eq!(parse("[1]").unwrap_err().code(), "invalid_manifest");
        assert_eq!(
            parse(&format!(r#"{{"format":"{FORMAT}","version":1}}"#))
                .unwrap_err()
                .code(),
            "invalid_manifest"
        );
    }

    #[test]
    fn design_example_parses_and_unknown_fields_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let audio = audio_path(dir.path(), "demo.m4a");
        let text = serde_json::json!({
          "format": "broscribe-import-manifest", "version": 1, "future": {"x": 1},
          "generated_at": "2026-11-14T09:00:00Z",
          "people": [{"name": "Ana Example", "email": "ana@example.com", "team": "x"}],
          "meetings": [{
            "source_id": "demo-1", "audio_path": audio, "title": "Weekly sync",
            "started_at": "2026-11-10T14:00:00Z", "duration_s": 1800.0, "language": "en",
            "tracks": "mixed", "extra": true,
            "participants": [{"name": "Ana Example", "email": "ana@example.com", "is_self": false}],
            "speaker_hints": [
              {"name": null, "email": null, "is_self": true, "ranges": [[12.4, 30.1], [95.0, 121.7]], "confidence": 0.9},
              {"name": "Ana Example", "email": "ana@example.com", "is_self": false, "ranges": [[31.0, 58.2]]}
            ]
          }]
        })
        .to_string();
        let m = parse(&text).unwrap();
        assert!(m.issues.is_empty());
        assert_eq!(m.people.len(), 1);
        let mt = &m.meetings[0];
        assert_eq!(mt.audio_path, audio);
        assert_eq!(mt.source_id.as_deref(), Some("demo-1"));
        assert_eq!(mt.hints.len(), 2);
        assert_eq!(mt.hints[0].person_key(), "self");
        assert_eq!(mt.hints[1].person_key(), "ana@example.com");
        assert_eq!(mt.hints[0].ranges[1], (95.0, 121.7));
    }

    #[test]
    fn per_meeting_errors_do_not_abort() {
        let dir = tempfile::tempdir().unwrap();
        let audio = audio_path(dir.path(), "a.wav");
        let meetings = serde_json::json!([
          {"audio_path": audio},
          {"title": "no path"},
          {"audio_path": "relative.wav"},
          {"audio_path": audio_path(dir.path(), "b.wav"), "tracks": "unknown"},
          {"audio_path": audio_path(dir.path(), "c.wav"), "speaker_hints": [{"is_self": false, "ranges": [[0, 1]]}]},
          {"audio_path": audio_path(dir.path(), "d.wav"), "speaker_hints": [{"name": "X", "ranges": [[2, 1]]}]},
          {"audio_path": audio_path(dir.path(), "e.wav"), "speaker_hints": [{"name": "X", "ranges": [[-1, 1]]}]},
          {"audio_path": audio_path(dir.path(), "f.wav"), "participants": [{"is_self": true}]},
          {"audio_path": audio_path(dir.path(), "g.wav"), "started_at": "2026-11-10 14:00"},
          {"audio_path": 5}
        ]);
        let m = parse(&wrap(meetings)).unwrap();
        assert_eq!(m.meetings.len(), 1);
        assert_eq!(m.meetings[0].audio_path, audio);
        let idx: Vec<usize> = m.issues.iter().filter_map(|i| i.meeting).collect();
        assert_eq!(idx, (1..10).collect::<Vec<_>>());
        assert_eq!(m.issues[2].code, "invalid_manifest");
        assert!(m.issues[2].message.contains("tracks"));
        assert!(
            m.issues
                .iter()
                .filter(|i| i.code == "invalid_manifest")
                .count()
                == 9
        );
    }

    #[test]
    fn person_matching_rules() {
        let mut ana = Person::new(PersonId::parse("p-000001").unwrap(), "Ana Example");
        ana.emails.push("Ana@Example.com".into());
        let mut me = Person::new(PersonId::parse("p-000002").unwrap(), "Bruno Sample");
        me.is_self = true;
        me.aliases.push("Bru".into());
        let people = vec![ana, me];
        let id = |p: Option<&Person>| p.map(|p| p.id.as_str().to_owned());
        assert_eq!(
            id(match_person(&people, None, Some("ana@example.COM"))),
            Some("p-000001".into())
        );
        // Both sides have an e-mail and they differ: no match by name.
        assert_eq!(
            id(match_person(
                &people,
                Some("Ana Example"),
                Some("other@example.com")
            )),
            None
        );
        assert_eq!(
            id(match_person(&people, Some(" ana example "), None)),
            Some("p-000001".into())
        );
        // The person has no e-mail: name decides.
        assert_eq!(
            id(match_person(&people, Some("bru"), Some("b@example.com"))),
            Some("p-000002".into())
        );
        assert_eq!(id(match_person(&people, Some("Nobody"), None)), None);
    }

    #[test]
    fn user_matching_rules() {
        let none = SetupProgress::default();
        assert!(!is_user(&none, Some("Bruno Sample"), None));
        let setup = SetupProgress {
            name: Some("Bruno Sample".into()),
            emails: vec!["bruno@example.com".into()],
            ..SetupProgress::default()
        };
        assert!(is_user(&setup, None, Some(" Bruno@Example.com")));
        assert!(is_user(&setup, Some("bruno sample"), None));
        // Both sides have an e-mail and they differ: no match by name.
        assert!(!is_user(
            &setup,
            Some("Bruno Sample"),
            Some("other@example.com")
        ));
        let name_only = SetupProgress {
            name: Some("Bruno Sample".into()),
            ..SetupProgress::default()
        };
        assert!(is_user(
            &name_only,
            Some("Bruno Sample"),
            Some("b@example.com")
        ));
    }
}
