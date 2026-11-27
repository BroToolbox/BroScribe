//! Live-preview speaker attributions the user decided during a recording
//! (plan D): durable records only, no inference.
//!
//! - **Session sidecar** `<library>/.live/<session>.json` (0600 in a 0700
//!   folder): the latest decision of each live turn the user named,
//!   confirmed or cleared, with the exact source range it covered (16 kHz
//!   recording samples and journal anchors). Identifiers, anchors and times
//!   only: never text, names or embeddings. Written and read only under the
//!   library lock, so the final publication (which reads it inside its
//!   locked commit) sees every decision made before it, and forget-person
//!   scrubs it in the same lock.
//! - **Final records** `Meeting::live_attributions`: what the publication
//!   made of each decision (applied as line attributions, partially or
//!   conflictingly supported, unmapped, cleared or forgotten). They never
//!   name a whole speaker by themselves. Final speaker names separately
//!   record which line decisions supported them.
//!
//! A Clear is stored as the turn's latest decision with its own number, so
//! nothing (a retry, a reprocess, a restart) brings an earlier name back.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::schema::{self, Meeting};
use crate::{Error, Library, PersonId, Result, SpeakerId, WriteTx, fsutil};

/// The source of a final speaker name carried over from the recording.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LivePromotion {
    pub person_id: PersonId,
    pub decision_ids: Vec<u64>,
    pub line_count: usize,
    pub required_lines: usize,
    pub at: DateTime<FixedOffset>,
}

/// Identifies the exact promotion offered in speaker history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSpeakerNaming {
    pub speaker_id: SpeakerId,
    pub promotion: LivePromotion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecordedSpeakerNaming {
    pub meeting_id: crate::MeetingId,
    #[serde(flatten)]
    pub naming: LiveSpeakerNaming,
}

pub fn speaker_namings(meeting: &Meeting) -> Vec<LiveSpeakerNaming> {
    meeting
        .speakers
        .iter()
        .filter_map(|speaker| {
            Some(LiveSpeakerNaming {
                speaker_id: speaker.id,
                promotion: speaker.live_promotion.clone()?,
            })
        })
        .collect()
}

/// Folder of live-attribution sidecars inside the library.
pub const LIVE_DIR: &str = ".live";
pub const SIDECAR_VERSION: u32 = 1;
/// Live preview sample rate: anchors count recording time at 16 kHz.
pub const LIVE_SAMPLE_RATE: u32 = 16_000;
/// Most turns one session sidecar holds a decision for.
pub const MAX_DECISIONS: usize = 20_000;
/// Longest live turn ID.
pub const MAX_TURN_ID: usize = 64;
/// Refused: the decision was taken on a turn revision older than the one
/// recorded already.
pub const STALE_REVISION: &str = "stale_revision";

/// A position in the capture journal: frame `frame` (device rate) of chunk
/// `seq` of the range's track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct JournalPoint {
    pub seq: u64,
    pub frame: u64,
}

/// One displayed live range: `[from_sample, to_sample)` of recording time
/// at [`LIVE_SAMPLE_RATE`] on `track` (`mic` or `system`), with the journal
/// anchors of both ends when the chunks holding them were known.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveRange {
    pub track: String,
    pub from_sample: u64,
    pub to_sample: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<JournalPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<JournalPoint>,
}

impl LiveRange {
    pub fn from_s(&self) -> f64 {
        self.from_sample as f64 / f64::from(LIVE_SAMPLE_RATE)
    }

    pub fn to_s(&self) -> f64 {
        self.to_sample as f64 / f64::from(LIVE_SAMPLE_RATE)
    }

    fn validate(&self) -> Result<()> {
        if !matches!(self.track.as_str(), "mic" | "system") {
            return Err(Error::InvalidInput(format!(
                "unknown live track {:?}",
                self.track
            )));
        }
        if self.to_sample <= self.from_sample {
            return Err(Error::InvalidInput("empty live range".into()));
        }
        Ok(())
    }
}

/// What the user did to one live turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveAction {
    /// Named the turn's speaker (an existing or a new name-only person).
    Name,
    /// Accepted the turn's suggested person.
    Confirm,
    /// Removed the manual assertion: the turn goes back to model output.
    Clear,
}

/// The latest decision on one live turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveDecision {
    /// Session-wide decision number, increasing with every decision.
    pub decision: u64,
    pub turn_id: String,
    /// Live-speakers worker epoch the displayed attribution came from (0:
    /// none, track-only preview).
    pub epoch: u64,
    /// Turn revision the user saw when deciding.
    pub turn_revision: u64,
    pub action: LiveAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<PersonId>,
    /// Exactly the displayed range; never extended to later speech.
    pub range: LiveRange,
    pub at: DateTime<FixedOffset>,
    /// The person was forgotten since: the decision reads as cleared.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forgotten: bool,
    /// The person's current forget fence when this decision was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forget_fence: Option<String>,
}

impl LiveDecision {
    /// The person the decision attributes the range to now.
    pub fn person(&self) -> Option<&PersonId> {
        match self.action {
            LiveAction::Clear => None,
            LiveAction::Name | LiveAction::Confirm if self.forgotten => None,
            LiveAction::Name | LiveAction::Confirm => self.person_id.as_ref(),
        }
    }
}

/// `<library>/.live/<session>.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveSidecar {
    pub version: u32,
    pub session_id: String,
    /// Number the next decision gets.
    pub next_decision: u64,
    /// One per turn (its latest decision), in decision order.
    pub decisions: Vec<LiveDecision>,
}

impl LiveSidecar {
    pub fn new(session_id: &str) -> Self {
        Self {
            version: SIDECAR_VERSION,
            session_id: session_id.to_owned(),
            next_decision: 1,
            decisions: Vec::new(),
        }
    }

    /// The latest decision on `turn_id`.
    pub fn current(&self, turn_id: &str) -> Option<&LiveDecision> {
        self.decisions.iter().find(|d| d.turn_id == turn_id)
    }
}

/// A decision to record ([`record_decision`] numbers and stamps it).
#[derive(Clone, Debug, PartialEq)]
pub struct NewDecision {
    pub turn_id: String,
    pub epoch: u64,
    pub turn_revision: u64,
    pub action: LiveAction,
    pub person_id: Option<PersonId>,
    pub range: LiveRange,
}

/// The sidecar path of `session_id` under the library `root`.
pub fn sidecar_path(root: &Path, session_id: &str) -> Result<PathBuf> {
    let dir = root.join(LIVE_DIR);
    // Same identifier rules as a journal folder.
    let path = crate::lifecycle::session_dir(&dir, session_id)?;
    Ok(path.with_extension("json"))
}

/// The session's sidecar, `None` without one.
pub fn read_sidecar(root: &Path, session_id: &str) -> Result<Option<LiveSidecar>> {
    match fsutil::read_json::<LiveSidecar>(&sidecar_path(root, session_id)?) {
        Ok(sidecar) if sidecar.session_id == session_id => Ok(Some(sidecar)),
        Ok(_) => Err(Error::Schema(format!(
            "live sidecar of {session_id} names another session"
        ))),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn write_sidecar(root: &Path, sidecar: &LiveSidecar) -> Result<()> {
    let dir = root.join(LIVE_DIR);
    fsutil::create_dir_durable(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::io_at(&dir, e))?;
    }
    let bytes = serde_json::to_vec(sidecar)
        .map_err(|e| Error::Internal(format!("serializing live sidecar: {e}")))?;
    fsutil::write_atomic_with_mode(
        &sidecar_path(root, &sidecar.session_id)?,
        &bytes,
        Some(0o600),
    )
}

fn check_turn_id(turn_id: &str) -> Result<()> {
    if turn_id.is_empty()
        || turn_id.len() > MAX_TURN_ID
        || !turn_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::InvalidInput(format!(
            "invalid live turn id {turn_id:?}"
        )));
    }
    Ok(())
}

/// Records `new` as the latest decision on its turn, under the library
/// lock. Refused (`stale_revision`) when the turn already has a decision
/// taken on the same or a later revision: a retried or reordered request
/// can never replace a newer decision (a Clear included).
pub fn record_decision(
    tx: &WriteTx<'_>,
    session_id: &str,
    new: NewDecision,
) -> Result<LiveDecision> {
    check_turn_id(&new.turn_id)?;
    new.range.validate()?;
    match (new.action, &new.person_id) {
        (LiveAction::Clear, None) => {}
        (LiveAction::Name | LiveAction::Confirm, Some(person)) => {
            crate::ops::check_profile_write(tx.library(), person)?;
        }
        (LiveAction::Clear, Some(_)) => {
            return Err(Error::InvalidInput("a clear names no person".into()));
        }
        (_, None) => return Err(Error::InvalidInput("no person to attribute".into())),
    }
    let root = tx.root();
    let mut sidecar =
        read_sidecar(root, session_id)?.unwrap_or_else(|| LiveSidecar::new(session_id));
    if let Some(at) = sidecar
        .decisions
        .iter()
        .position(|d| d.turn_id == new.turn_id)
    {
        if sidecar.decisions[at].turn_revision >= new.turn_revision {
            return Err(Error::Service {
                code: STALE_REVISION,
                message: format!(
                    "live turn {} changed since it was shown; reload it",
                    new.turn_id
                ),
            });
        }
        sidecar.decisions.remove(at);
    } else if sidecar.decisions.len() >= MAX_DECISIONS {
        return Err(Error::InvalidInput(
            "too many live decisions in this recording".into(),
        ));
    }
    let forget_fence = new.person_id.as_ref().and_then(|person| {
        // A damaged fence disables learning from the decision. The line
        // name itself still takes effect, as it did before promotion.
        let fence = forget_fence(tx.library(), person.as_str()).unwrap_or_default();
        (fence != NEVER_FORGOTTEN).then_some(fence)
    });
    let decision = LiveDecision {
        decision: sidecar.next_decision,
        turn_id: new.turn_id,
        epoch: new.epoch,
        turn_revision: new.turn_revision,
        action: new.action,
        person_id: new.person_id,
        range: new.range,
        at: schema::now(),
        forgotten: false,
        forget_fence,
    };
    sidecar.next_decision += 1;
    sidecar.decisions.push(decision.clone());
    write_sidecar(root, &sidecar)?;
    Ok(decision)
}

/// The voice evidence live suggestions rest on: every person's stored
/// samples and existence ([`crate::learning::evidence_fingerprint`]) plus
/// the forget tombstones, so forgetting a voice changes it even for a person
/// whose only samples were transient session ones (no `voiceprints.json`).
/// The worker reloads, and the daemon refences pickers, whenever it changes.
/// Recomputed from the files on every call (another process may forget at
/// any time); tombstones are hashed in name order, each streamed.
pub fn evidence_fingerprint(lib: &Library) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut all = Sha256::new();
    all.update(crate::learning::evidence_fingerprint(lib)?.as_bytes());
    let dir = lib.root().join(crate::ops::FORGOTTEN_DIR);
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    match fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                names.push(entry.map_err(|e| Error::io_at(&dir, e))?.file_name());
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io_at(&dir, e)),
    }
    names.sort();
    for name in names {
        // A tombstone removed since the listing is simply absent.
        let Some(digest) = crate::learning::hash_file(&dir.join(&name))? else {
            continue;
        };
        all.update([0]);
        all.update(name.as_encoded_bytes());
        all.update(b"=");
        all.update(digest);
    }
    Ok(schema::hex(&all.finalize()))
}

/// [`forget_fence`] of a person never forgotten.
pub const NEVER_FORGOTTEN: &str = "never";

/// Carry the learning authorization of a live decision across a merge.
pub(crate) fn remap_forget_fence(fence: &mut Option<String>, source: &str, keep: &str) {
    let recorded = fence.as_deref().unwrap_or(NEVER_FORGOTTEN);
    if recorded == source {
        *fence = (keep != NEVER_FORGOTTEN).then(|| keep.to_owned());
    } else if recorded == keep {
        // The decision was stale for the source. A matching destination
        // fence must not reauthorize it (e.g. when keep was never forgotten).
        *fence = Some(String::new());
    }
}

/// An opaque token that changes on every forget of `person_id` (voice or
/// person): the SHA-256 of their id tombstone, whose content carries a
/// fresh random nonce per forget, or [`NEVER_FORGOTTEN`]. Wall-clock order
/// plays no part: read under the library lock with a live decision, it
/// authorizes that decision's transient sample only while it stays equal.
pub fn forget_fence(lib: &Library, person_id: &str) -> Result<String> {
    let path = crate::ops::tombstone_path(lib.root(), person_id);
    match fs::read(&path) {
        Ok(bytes) => Ok(schema::sha256_hex(&bytes)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(NEVER_FORGOTTEN.to_owned()),
        Err(e) => Err(Error::io_at(&path, e)),
    }
}

/// [`forget_fence`] of every existing person ever forgotten (the others are
/// [`NEVER_FORGOTTEN`]).
pub fn forget_fences(lib: &Library) -> Result<std::collections::BTreeMap<PersonId, String>> {
    let mut out = std::collections::BTreeMap::new();
    for person in lib.list_people()? {
        let fence = forget_fence(lib, person.id.as_str())?;
        if fence != NEVER_FORGOTTEN {
            out.insert(person.id, fence);
        }
    }
    Ok(out)
}

/// Removes the session's sidecar (published, discarded); `true` when one
/// existed. Idempotent.
pub fn remove_sidecar(root: &Path, session_id: &str) -> Result<bool> {
    let path = sidecar_path(root, session_id)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io_at(&path, e)),
    }
}

/// Sessions with a sidecar, sorted.
pub fn list_sidecars(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(LIVE_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(&dir, e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(&dir, e))?;
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.strip_suffix(".json"))
            && sidecar_path(root, id).is_ok()
        {
            out.push(id.to_owned());
        }
    }
    out.sort();
    Ok(out)
}

/// Forget-person, inside its library lock: every sidecar decision naming
/// `person` becomes `forgotten` (reads as cleared; never deleted, so an
/// earlier decision on the same turn cannot come back). Returns the count.
pub fn forget_person_in_sidecars(tx: &WriteTx<'_>, person: &PersonId) -> Result<usize> {
    let root = tx.root();
    let mut changed = 0;
    for session in list_sidecars(root)? {
        let Some(mut sidecar) = read_sidecar(root, &session)? else {
            continue;
        };
        let mut touched = false;
        for d in &mut sidecar.decisions {
            if d.person_id.as_ref() == Some(person) {
                d.person_id = None;
                d.forgotten = true;
                d.forget_fence = None;
                touched = true;
                changed += 1;
            }
        }
        if touched {
            write_sidecar(root, &sidecar)?;
        }
    }
    Ok(changed)
}

/// Retarget live decisions during a confirmed profile merge.
pub(crate) fn replace_person_in_sidecars(
    tx: &WriteTx<'_>,
    source: &PersonId,
    keep: &PersonId,
    source_fence: &str,
    keep_fence: &str,
) -> Result<()> {
    let root = tx.root();
    for session in list_sidecars(root)? {
        let Some(mut sidecar) = read_sidecar(root, &session)? else {
            continue;
        };
        let mut changed = false;
        for decision in &mut sidecar.decisions {
            if decision.person_id.as_ref() == Some(source) {
                decision.person_id = Some(keep.clone());
                remap_forget_fence(&mut decision.forget_fence, source_fence, keep_fence);
                changed = true;
            }
        }
        if changed {
            write_sidecar(root, &sidecar)?;
        }
    }
    Ok(())
}

/// What the final publication made of one live decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveOutcome {
    /// Every final segment the range overlaps lies inside the user's
    /// reviewed ranges for that person (exactly, up to sample rounding):
    /// each became a line attribution carrying this decision's number.
    Applied,
    /// The range covers final segments only in part; `supported` holds the
    /// covered time. Nothing applied (an earlier overlay is retracted); the
    /// user should check.
    Partial,
    /// Another decision names someone else for the same final segment.
    /// Nothing applied; the user should check.
    Conflicting,
    /// No final segment overlaps the range (silence, or audio cut).
    Unmapped,
    /// The user cleared the turn: model output stands.
    Cleared,
    /// The person was forgotten or no longer exists.
    Forgotten,
}

impl LiveOutcome {
    /// The record still asks for the user's review.
    pub fn unresolved(self) -> bool {
        matches!(self, Self::Partial | Self::Conflicting | Self::Unmapped)
    }
}

/// A live decision carried into the meeting it was recorded in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveAttribution {
    pub turn_id: String,
    pub decision: u64,
    pub action: LiveAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<PersonId>,
    pub range: LiveRange,
    /// The range on the meeting's timeline (seconds); `None` when its
    /// anchors no longer map (audio cut by a cutoff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_s: Option<f64>,
    /// Final time the decision covers: whole segments when applied, the
    /// overlapped parts when partial or conflicting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported: Vec<[f64; 2]>,
    pub outcome: LiveOutcome,
    pub at: DateTime<FixedOffset>,
    /// The user explicitly edited or removed a final line this decision
    /// claimed (created, or overlaps): their correction stands, this
    /// decision never adds a line again (its remaining overlays are still
    /// retracted when unsupported) and it leaves review.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user_override: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forget_fence: Option<String>,
}

impl LiveAttribution {
    /// The record still asks for the user's review: unresolved
    /// ([`LiveOutcome::unresolved`]) and not since settled by the user's
    /// own correction of its lines ([`Self::user_override`]).
    pub fn needs_review(&self) -> bool {
        self.outcome.unresolved() && !self.user_override
    }

    /// Its range on the meeting's timeline: the mapped one, else recording
    /// time.
    pub fn span_s(&self) -> (f64, f64) {
        (
            self.from_s.unwrap_or(self.range.from_s()),
            self.to_s.unwrap_or(self.range.to_s()),
        )
    }
}

/// Numeric slack only: two live samples of rounding between the live clock,
/// the journal anchors and the final timeline. Never speech.
pub const ROUNDING_S: f64 = 2.0 / LIVE_SAMPLE_RATE as f64;

fn clock(s: f64) -> String {
    let s = s.max(0.0).round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// The one generator of [`ReviewSource::LivePreview`] notes, derived from
/// `meeting.live_attributions` alone: one note per record that
/// [`LiveAttribution::needs_review`], naming its person through `names`
/// ("someone" for a person not in it — forgotten or gone). Replaces every
/// earlier LivePreview note, so a name that is no longer valid never stays
/// in a note; other sources' notes are kept. Outcomes are read, never
/// recomputed.
pub fn refresh_review_notes(
    meeting: &mut Meeting,
    names: &std::collections::BTreeMap<PersonId, String>,
) {
    let notes: Vec<String> = meeting
        .live_attributions
        .iter()
        .filter(|r| r.needs_review())
        .map(|r| {
            let who = r
                .person_id
                .as_ref()
                .and_then(|p| names.get(p))
                .map_or("someone", String::as_str);
            let (a, b) = r.span_s();
            let why = match r.outcome {
                LiveOutcome::Partial => "covers only part of the final lines there",
                LiveOutcome::Conflicting => "conflicts with another name for the same lines",
                _ => "matches no final line",
            };
            format!(
                "Check the name you gave while recording ({who}, {}–{}). It {why}.",
                clock(a),
                clock(b)
            )
        })
        .collect();
    meeting.set_review_notes(schema::ReviewSource::LivePreview, notes);
}

/// Recording-time names needing attention in the saved recording.
pub fn review_count(meeting: &Meeting) -> usize {
    meeting
        .live_attributions
        .iter()
        .filter(|decision| decision.needs_review())
        .count()
        + meeting
            .speakers
            .iter()
            .filter(|speaker| {
                speaker.live_suggestion_lines.is_some()
                    || speaker.identity_conflict.as_ref().is_some_and(|conflict| {
                        conflict
                            .lines
                            .iter()
                            .any(|line| line.live_decision.is_some())
                    })
            })
            .count()
}

/// One live decision awaiting the user's structured review: what it named,
/// where (the audio to play is `[from_s, to_s)` of `track`), why it was not
/// applied, and the final lines it touches, ready for an explicit
/// correction through [`crate::ops::set_utterance_people`] or, to leave
/// the lines as transcribed, [`crate::ops::dismiss_live_review`] (fenced by
/// `review_key`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LiveReviewItem {
    pub turn_id: String,
    pub decision: u64,
    pub outcome: LiveOutcome,
    /// `None` once the person is gone (never a stale name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub person_id: Option<PersonId>,
    pub track: String,
    pub from_s: f64,
    pub to_s: f64,
    /// `false` when the range's anchors no longer map onto the final
    /// timeline: `from_s`/`to_s` are recording time then.
    pub mapped: bool,
    /// Final time the decision covered (partial or conflicting parts).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supported: Vec<[f64; 2]>,
    /// Final lines of the track the range overlaps (beyond rounding).
    pub lines: Vec<crate::ops::SegmentFingerprint>,
    /// Opaque fence of exactly this item as shown: the decision, its turn
    /// and range, outcome, mapping, supported time, overlapped lines and the
    /// meeting's speaker namespace. Any of them changing (a reprocess, a
    /// republication, a reused number) changes it.
    pub review_key: String,
}

/// Every live record of `meeting` that [`LiveAttribution::needs_review`],
/// in decision order. Read-only; derived from the meeting alone.
pub fn live_review_items(meeting: &Meeting) -> Vec<LiveReviewItem> {
    let mut records: Vec<&LiveAttribution> = meeting
        .live_attributions
        .iter()
        .filter(|r| r.needs_review())
        .collect();
    records.sort_by_key(|r| r.decision);
    records
        .into_iter()
        .map(|r| {
            let (from_s, to_s) = r.span_s();
            let mapped = r.from_s.is_some() && r.to_s.is_some();
            let lines = if mapped {
                meeting
                    .segments
                    .iter()
                    .filter(|s| {
                        s.track == r.range.track
                            && s.end.min(to_s) - s.start.max(from_s) > ROUNDING_S
                    })
                    .map(crate::ops::segment_fingerprint)
                    .collect()
            } else {
                Vec::new()
            };
            LiveReviewItem {
                review_key: review_key(meeting, r, (from_s, to_s), mapped, &lines),
                turn_id: r.turn_id.clone(),
                decision: r.decision,
                outcome: r.outcome,
                person_id: r.person_id.clone(),
                track: r.range.track.clone(),
                from_s,
                to_s,
                mapped,
                supported: r.supported.clone(),
                lines,
            }
        })
        .collect()
}

/// [`LiveReviewItem::review_key`]: SHA-256 over every field the item shows.
fn review_key(
    meeting: &Meeting,
    r: &LiveAttribution,
    span: (f64, f64),
    mapped: bool,
    lines: &[crate::ops::SegmentFingerprint],
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut field = |bytes: &[u8]| {
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    field(meeting.speaker_namespace.as_bytes());
    field(r.turn_id.as_bytes());
    field(&r.decision.to_le_bytes());
    field(format!("{:?}", r.outcome).as_bytes());
    field(r.person_id.as_ref().map_or("", |p| p.as_str()).as_bytes());
    field(r.range.track.as_bytes());
    field(&r.range.from_sample.to_le_bytes());
    field(&r.range.to_sample.to_le_bytes());
    field(&span.0.to_bits().to_le_bytes());
    field(&span.1.to_bits().to_le_bytes());
    field(&[u8::from(mapped)]);
    for [a, b] in &r.supported {
        field(&a.to_bits().to_le_bytes());
        field(&b.to_bits().to_le_bytes());
    }
    field(b"lines");
    for l in lines {
        field(l.track.as_bytes());
        field(&l.start.to_bits().to_le_bytes());
        field(&l.end.to_bits().to_le_bytes());
        field(l.text_sha256.as_bytes());
    }
    schema::hex(&h.finalize())
}

/// One meeting's live decisions awaiting review ([`live_reviews`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LiveReviewMeeting {
    pub meeting_id: crate::MeetingId,
    pub title: Option<String>,
    pub started_at: DateTime<FixedOffset>,
    /// The recording's SHA-256 while its audio is available (the items'
    /// ranges can be played); `None` once it is missing or deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_sha256: Option<String>,
    /// [`live_review_items`], never empty.
    pub items: Vec<LiveReviewItem>,
}

/// Every meeting with live decisions awaiting review, newest first (then
/// by ID), read from the meeting files: the one rule the app's review queue
/// and the CLI share. Unreadable meetings are skipped. Read-only; call on a
/// background executor: it reads every meeting.
pub fn live_reviews(lib: &Library) -> Result<Vec<LiveReviewMeeting>> {
    live_reviews_and_namings(lib).map(|(reviews, _)| reviews)
}

pub fn live_reviews_and_namings(
    lib: &Library,
) -> Result<(Vec<LiveReviewMeeting>, Vec<RecordedSpeakerNaming>)> {
    let mut out = Vec::new();
    let mut namings = Vec::new();
    for dir in lib.list_meeting_dirs()? {
        let Ok(meeting) = lib.load_meeting_at(&dir) else {
            continue;
        };
        namings.extend(
            speaker_namings(&meeting)
                .into_iter()
                .map(|naming| RecordedSpeakerNaming {
                    meeting_id: meeting.id.clone(),
                    naming,
                }),
        );
        if !meeting
            .live_attributions
            .iter()
            .any(LiveAttribution::needs_review)
        {
            continue;
        }
        let items = live_review_items(&meeting);
        out.push(LiveReviewMeeting {
            recording_sha256: meeting
                .recording
                .as_ref()
                .filter(|r| r.state == schema::RecordingState::Available)
                .map(|r| r.sha256.clone()),
            meeting_id: meeting.id,
            title: meeting.title,
            started_at: meeting.started_at,
            items,
        });
    }
    out.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.meeting_id.cmp(&b.meeting_id))
    });
    namings.sort_by_key(|entry| std::cmp::Reverse(entry.naming.promotion.at));
    Ok((out, namings))
}

/// Forget-person on one meeting: its live records naming `person` lose the
/// person and read as forgotten. Returns whether anything changed.
pub fn forget_person_in_meeting(meeting: &mut Meeting, person: &PersonId) -> bool {
    let mut changed = false;
    for a in &mut meeting.live_attributions {
        if a.person_id.as_ref() == Some(person) {
            a.person_id = None;
            a.outcome = LiveOutcome::Forgotten;
            a.supported.clear();
            a.forget_fence = None;
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Library;
    use crate::schema::Source;

    fn range(from: u64, to: u64) -> LiveRange {
        LiveRange {
            track: "system".into(),
            from_sample: from,
            to_sample: to,
            from: Some(JournalPoint {
                seq: 0,
                frame: from,
            }),
            to: Some(JournalPoint { seq: 0, frame: to }),
        }
    }

    fn decide(
        tx: &WriteTx<'_>,
        turn: &str,
        revision: u64,
        action: LiveAction,
        person: Option<&PersonId>,
    ) -> Result<LiveDecision> {
        record_decision(
            tx,
            "s1",
            NewDecision {
                turn_id: turn.into(),
                epoch: 1,
                turn_revision: revision,
                action,
                person_id: person.cloned(),
                range: range(16_000, 48_000),
            },
        )
    }

    #[test]
    fn record_live_clear_is_durable_and_older_revisions_cannot_resurrect_a_name() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path().join("library")).unwrap();
        let ana = lib.new_person_id();
        lib.write(|tx| {
            let named = decide(tx, "system-16000", 2, LiveAction::Name, Some(&ana))?;
            assert_eq!(named.decision, 1);
            let cleared = decide(tx, "system-16000", 3, LiveAction::Clear, None)?;
            assert_eq!(cleared.decision, 2);
            // A retried Name taken on the earlier revision is refused.
            let stale = decide(tx, "system-16000", 2, LiveAction::Name, Some(&ana)).unwrap_err();
            assert_eq!(stale.code(), STALE_REVISION);
            Ok(())
        })
        .unwrap();
        let sidecar = read_sidecar(lib.root(), "s1").unwrap().unwrap();
        assert_eq!(sidecar.decisions.len(), 1);
        assert_eq!(sidecar.current("system-16000").unwrap().person(), None);
        assert_eq!(sidecar.next_decision, 3);
        // Identifiers and anchors only.
        let raw = fs::read_to_string(sidecar_path(lib.root(), "s1").unwrap()).unwrap();
        assert!(!raw.contains("text") && !raw.contains("embedding"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(sidecar_path(lib.root(), "s1").unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(remove_sidecar(lib.root(), "s1").unwrap());
        assert!(!remove_sidecar(lib.root(), "s1").unwrap());
        assert_eq!(read_sidecar(lib.root(), "s1").unwrap(), None);
    }

    #[test]
    fn record_live_decisions_validate_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path().join("library")).unwrap();
        let ana = lib.new_person_id();
        lib.write(|tx| {
            assert!(decide(tx, "../x", 1, LiveAction::Name, Some(&ana)).is_err());
            assert!(decide(tx, "t", 1, LiveAction::Name, None).is_err());
            assert!(decide(tx, "t", 1, LiveAction::Clear, Some(&ana)).is_err());
            let mut bad = range(10, 10);
            bad.track = "mic".into();
            assert!(
                record_decision(
                    tx,
                    "s1",
                    NewDecision {
                        turn_id: "t".into(),
                        epoch: 0,
                        turn_revision: 1,
                        action: LiveAction::Clear,
                        person_id: None,
                        range: bad,
                    },
                )
                .is_err()
            );
            assert!(sidecar_path(tx.root(), "../escape").is_err());
            Ok(())
        })
        .unwrap();
        assert_eq!(read_sidecar(lib.root(), "s1").unwrap(), None);
    }

    #[test]
    fn record_live_forget_person_scrubs_sidecars_and_final_records() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path().join("library")).unwrap();
        let ana = lib.new_person_id();
        let bia = lib.new_person_id();
        lib.write(|tx| {
            decide(tx, "a", 1, LiveAction::Name, Some(&ana))?;
            decide(tx, "b", 1, LiveAction::Confirm, Some(&bia))?;
            assert_eq!(forget_person_in_sidecars(tx, &ana)?, 1);
            Ok(())
        })
        .unwrap();
        let sidecar = read_sidecar(lib.root(), "s1").unwrap().unwrap();
        let a = sidecar.current("a").unwrap();
        assert!(a.forgotten && a.person().is_none() && a.person_id.is_none());
        assert_eq!(sidecar.current("b").unwrap().person(), Some(&bia));

        let mut meeting = Meeting::new(
            lib.new_meeting_id().unwrap(),
            Source::Recording,
            schema::now(),
        );
        meeting.live_attributions.push(LiveAttribution {
            turn_id: "a".into(),
            decision: 1,
            action: LiveAction::Name,
            person_id: Some(ana.clone()),
            range: range(0, 16_000),
            from_s: Some(0.0),
            to_s: Some(1.0),
            supported: vec![[0.0, 1.0]],
            outcome: LiveOutcome::Applied,
            at: schema::now(),
            user_override: false,
            forget_fence: None,
        });
        assert!(forget_person_in_meeting(&mut meeting, &ana));
        assert!(!forget_person_in_meeting(&mut meeting, &ana));
        let a = &meeting.live_attributions[0];
        assert_eq!(a.outcome, LiveOutcome::Forgotten);
        assert!(a.person_id.is_none() && a.supported.is_empty());
    }

    #[test]
    fn record_live_manual_line_edit_takes_over_a_live_created_line() {
        use crate::ops;
        use crate::schema::{Person, ProcessingStatus, Segment, UtterancePerson};
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path().join("library")).unwrap();
        let ana = Person::new(lib.new_person_id(), "Ana Manual");
        let mut meeting = Meeting::new(
            lib.new_meeting_id().unwrap(),
            Source::Recording,
            schema::now(),
        );
        meeting.processing.status = ProcessingStatus::Done;
        meeting.segments = vec![Segment {
            speech_overlap: Vec::new(),
            id: 0,
            track: "system".into(),
            speaker: None,
            start: 1.0,
            end: 3.0,
            text: "live line".into(),
            words: Vec::new(),
        }];
        let fp = ops::segment_fingerprint(&meeting.segments[0]);
        meeting.utterance_people.push(UtterancePerson {
            track: fp.track.clone(),
            start: fp.start,
            end: fp.end,
            text_sha256: fp.text_sha256.clone(),
            person_id: ana.id.clone(),
            at: schema::now(),
            live_decision: Some(4),
        });
        meeting.live_attributions.push(LiveAttribution {
            turn_id: "system-16000".into(),
            decision: 4,
            action: LiveAction::Name,
            person_id: Some(ana.id.clone()),
            range: range(16_000, 48_000),
            from_s: Some(1.0),
            to_s: Some(3.0),
            supported: vec![[1.0, 3.0]],
            outcome: LiveOutcome::Applied,
            at: schema::now(),
            user_override: false,
            forget_fence: None,
        });
        let id = meeting.id.clone();
        lib.write(|tx| {
            tx.save_person(&ana)?;
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)
        })
        .unwrap();
        // The user confirms the same person on that line by hand.
        let after = ops::with_index(&lib, |tx, index| {
            ops::set_utterance_people(tx, index, &id, &[fp], Some(ana.id.as_str()))
        })
        .unwrap();
        assert_eq!(after.utterance_people.len(), 1);
        assert_eq!(after.utterance_people[0].live_decision, None, "now theirs");
        assert!(after.live_attributions[0].user_override);
    }

    #[test]
    fn record_live_forget_fence_changes_on_every_forget_whatever_the_clock() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let ana = crate::schema::Person::new(lib.new_person_id(), "Ana Fence");
        lib.write(|tx| tx.save_person(&ana)).unwrap();
        let id = ana.id.as_str();
        let never = forget_fence(&lib, id).unwrap();
        assert_eq!(never, NEVER_FORGOTTEN);
        let forget = || {
            crate::ops::with_index(&lib, |tx, index| crate::ops::forget_voice(tx, index, id))
                .unwrap()
        };
        forget();
        let first = forget_fence(&lib, id).unwrap();
        assert_ne!(first, never);
        assert!(
            lib.load_person(&ana.id).is_ok(),
            "forget-voice keeps the person"
        );
        forget();
        let second = forget_fence(&lib, id).unwrap();
        assert_ne!(second, first);
        // A forget under a clock stepped back, then another in the same
        // instant: timestamps say nothing, each fence is still new.
        let path = crate::ops::tombstone_path(lib.root(), id);
        let at = "2001-01-01T00:00:00+00:00";
        fs::write(&path, format!("{at} {:032x}\n", 1u128)).unwrap();
        let back = forget_fence(&lib, id).unwrap();
        fs::write(&path, format!("{at} {:032x}\n", 2u128)).unwrap();
        let same_instant = forget_fence(&lib, id).unwrap();
        assert!(![&never, &first, &second].contains(&&back));
        assert!(![&never, &first, &second, &back].contains(&&same_instant));
        // Tombstone time still reads with the nonce.
        assert_eq!(
            crate::ops::forgotten_since(&lib, id)
                .unwrap()
                .unwrap()
                .to_rfc3339(),
            at
        );
        assert_eq!(
            forget_fences(&lib).unwrap().get(&ana.id),
            Some(&same_instant)
        );
    }
}
