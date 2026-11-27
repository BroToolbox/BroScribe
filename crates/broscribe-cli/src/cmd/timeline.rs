//! Saved recording availability, playback clips and timeline annotations.

use std::path::Path;

use broscribe_core::ops::{
    Clip, ClipFiles, ClipSource, clip_of, enqueue_playback_cache, locate_clip,
};
use broscribe_core::render::format_hms;
use broscribe_core::schema::{Recording, RecordingState, Voiceprint};
use broscribe_core::{Error, Library, MeetingId, PersonId, Result, ops};
use broscribe_pipeline::audio::TARGET_RATE;
use broscribe_pipeline::worker::{playback_cache_path, wav_16k_bytes};
use serde::Serialize;
use serde_json::json;

use crate::{Ctx, args::parse_clock};

pub(super) fn check_recording(ctx: &mut Ctx<'_>, meeting: &str) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let id = super::meeting_id(meeting)?;
    let m = ops::reconcile_recording(&lib, &id)?;
    let path = ops::expected_recording_path(&lib, &m)?;
    let state = m.recording.map(|r| r.state);
    ctx.emit(
        json!({"meeting_id": id, "state": state, "path": path}),
        |out| {
            writeln!(
                out,
                "{id}: {}",
                state.map_or("no recording", |s| match s {
                    RecordingState::Available => "available",
                    RecordingState::Missing => "missing",
                    RecordingState::Deleted => "deleted",
                })
            )
        },
    )
}

fn available(recording: Option<&Recording>) -> Result<&Recording> {
    recording
        .filter(|r| r.state == RecordingState::Available)
        .ok_or_else(|| Error::NotFound("available meeting recording".into()))
}

pub(super) fn prepare_audio(ctx: &mut Ctx<'_>, meeting: &str) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let id = super::meeting_id(meeting)?;
    let m = ops::reconcile_recording(&lib, &id)?;
    let rec = available(m.recording.as_ref())?;
    let cache = playback_cache_path(&ctx.paths.cache_dir, &rec.sha256);
    let ready = cache.is_file();
    let job = if ready {
        None
    } else {
        let job = enqueue_playback_cache(&lib, &id, &rec.sha256)?;
        super::kick_agent(ctx)?;
        Some(job)
    };
    ctx.emit(
        json!({"meeting_id": id, "ready": ready, "job_id": job, "cache": cache}),
        |out| match job {
            Some(job) => writeln!(out, "Playback cache queued ({job})."),
            None => writeln!(out, "Playback cache ready: {}", cache.display()),
        },
    )
}

pub(super) fn meeting_clip(
    ctx: &mut Ctx<'_>,
    meeting: &str,
    from: &str,
    to: &str,
    path: &Path,
) -> Result<()> {
    let id = super::meeting_id(meeting)?;
    let lib = ctx.library_existing(&ctx.config()?)?;
    let clip = Clip {
        source: ClipSource::Meeting {
            meeting_id: id,
            sha256: None,
        },
        start_s: parse_clock(from)?,
        end_s: parse_clock(to)?,
    };
    write_clip(ctx, &lib, &clip, path)
}

fn write_clip(ctx: &mut Ctx<'_>, lib: &Library, clip: &Clip, path: &Path) -> Result<()> {
    let (from, to) = (clip.start_s, clip.end_s);
    if !from.is_finite() || !to.is_finite() || from < 0.0 || to <= from {
        return Err(Error::InvalidInput("clip requires 0 <= start < end".into()));
    }
    let destination = if ctx.request_library.is_some() {
        super::export::mcp_export_path(ctx.data_dir(), lib.root(), path, "wav")?
    } else {
        let parent =
            broscribe_core::export::outside_library_parent(lib.root(), path).ok_or_else(|| {
                Error::InvalidInput("choose an existing folder outside the library".into())
            })?;
        let name = path
            .file_name()
            .ok_or_else(|| Error::InvalidInput("give a WAV file path".into()))?;
        parent.join(name)
    };
    let parent = destination
        .parent()
        .ok_or_else(|| Error::InvalidInput("choose an existing destination folder".into()))?;
    let files = locate_clip(lib, &ctx.paths.cache_dir, &clip.source)?;
    let ClipFiles::Found { audio, .. } = files else {
        return Err(Error::NotFound("source audio for this clip".into()));
    };
    let samples = match &clip.source {
        ClipSource::Meeting { meeting_id, sha256 } => {
            let m = lib.load_meeting(meeting_id)?;
            if sha256
                .as_ref()
                .is_some_and(|hash| Some(hash) != m.recording.as_ref().map(|r| &r.sha256))
            {
                return Err(Error::NotFound(
                    "the recording this sample came from".into(),
                ));
            }
            available(m.recording.as_ref())?;
            broscribe_pipeline::audio::playback_mix(lib, &ctx.deps.decoder, &m)?
        }
        ClipSource::Enrollment { .. } => {
            broscribe_pipeline::audio::decode_16k(&ctx.deps.decoder, &audio)?.samples
        }
    };
    let start = (from * f64::from(TARGET_RATE)).round() as usize;
    let end = (to * f64::from(TARGET_RATE)).round() as usize;
    let slice = samples
        .get(start..end)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::InvalidInput("clip range falls outside the recording".into()))?;
    let bytes = wav_16k_bytes(slice)?;
    ctx.check_request_cancelled()?;
    super::write_new_private_file(parent, &destination, &bytes)?;
    ctx.emit(
        json!({"path": destination, "from_s": from, "to_s": to}),
        |out| writeln!(out, "Saved {}", destination.display()),
    )
}

fn person(lib: &Library, reference: &str) -> Result<PersonId> {
    ops::find_person(lib, reference)?
        .map(|p| p.id)
        .ok_or_else(|| Error::NotFound(format!("person {}", reference.trim())))
}

/// The CLI projects provenance only. Voice embeddings never leave the library.
#[derive(Serialize)]
struct Sample<'a> {
    id: &'a str,
    model: &'a str,
    device: Option<&'a str>,
    kind: broscribe_core::schema::VoiceprintKind,
    strength: broscribe_core::schema::Strength,
    created_at: chrono::DateTime<chrono::FixedOffset>,
    meeting_id: Option<&'a MeetingId>,
    meeting_title: Option<String>,
    meeting_date: Option<chrono::DateTime<chrono::FixedOffset>>,
    from_s: f64,
    to_s: f64,
    duration_s: f64,
    audio_available: bool,
}

fn sample_row<'a>(
    lib: &Library,
    cache: &Path,
    person: &PersonId,
    vp: &'a Voiceprint,
) -> Result<Sample<'a>> {
    let source = &vp.source;
    let meeting = source
        .meeting_id
        .as_ref()
        .and_then(|id| lib.load_meeting(id).ok());
    let audio_available = match clip_of(person, source) {
        Some(clip) => matches!(
            locate_clip(lib, cache, &clip.source)?,
            ClipFiles::Found { .. }
        ),
        None => false,
    };
    Ok(Sample {
        id: &vp.id,
        model: &vp.model,
        device: vp.device.as_deref(),
        kind: source.kind,
        strength: vp.strength,
        created_at: vp.created_at,
        meeting_id: source.meeting_id.as_ref(),
        meeting_title: meeting.as_ref().and_then(|m| m.title.clone()),
        meeting_date: meeting.as_ref().map(|m| m.started_at),
        from_s: source.start,
        to_s: source.end,
        duration_s: (source.end - source.start).max(0.0),
        audio_available,
    })
}

pub(super) fn samples(ctx: &mut Ctx<'_>, reference: &str) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let id = person(&lib, reference)?;
    let prints = lib.load_voiceprints(&id)?;
    let rows = prints
        .embeddings
        .iter()
        .map(|vp| sample_row(&lib, &ctx.paths.cache_dir, &id, vp))
        .collect::<Result<Vec<_>>>()?;
    ctx.emit(json!({"person_id": id, "samples": rows}), |out| {
        if rows.is_empty() {
            writeln!(out, "No voice samples.")?;
        }
        for row in rows {
            writeln!(
                out,
                "{}  {:?}  {:?}  {:.2}s  {}  {}",
                row.id,
                row.kind,
                row.strength,
                row.duration_s,
                row.meeting_title.as_deref().unwrap_or(""),
                if row.audio_available {
                    "Audio available"
                } else {
                    "Audio no longer available"
                }
            )?;
        }
        Ok(())
    })
}

pub(super) fn person_clip(
    ctx: &mut Ctx<'_>,
    reference: &str,
    sample: &str,
    path: &Path,
) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let id = person(&lib, reference)?;
    let prints = lib.load_voiceprints(&id)?;
    let clip = prints
        .embeddings
        .iter()
        .find(|vp| vp.id == sample)
        .and_then(|vp| clip_of(&id, &vp.source))
        .ok_or_else(|| Error::NotFound("playable voice sample".into()))?;
    write_clip(ctx, &lib, &clip, path)
}

/// One bookmark or note, as the meeting view's timeline shows it.
#[derive(Serialize)]
struct Marker<'a> {
    kind: &'static str,
    at_s: f64,
    text: Option<&'a str>,
}

/// Bookmarks and notes in recording-time order, bookmarks first on a tie
/// (the order of the saved transcript's markers and the notes export).
fn markers(m: &broscribe_core::schema::Meeting) -> Vec<Marker<'_>> {
    let mut rows: Vec<_> = m
        .bookmarks
        .iter()
        .map(|b| Marker {
            kind: "bookmark",
            at_s: b.at_s,
            text: b.label.as_deref(),
        })
        .chain(m.notes.iter().map(|n| Marker {
            kind: "note",
            at_s: n.at_s,
            text: Some(n.text.as_str()),
        }))
        .collect();
    rows.sort_by(|a, b| a.at_s.total_cmp(&b.at_s));
    rows
}

pub(super) fn timeline(ctx: &mut Ctx<'_>, meeting: &str) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let id = super::meeting_id(meeting)?;
    let m = lib.load_meeting(&id)?;
    let rows = markers(&m);
    ctx.emit(
        json!({"meeting_id": id, "bookmarks": m.bookmarks, "notes": m.notes, "timeline": rows}),
        |out| {
            if rows.is_empty() {
                writeln!(out, "No bookmarks or notes.")?;
            }
            for row in &rows {
                let kind = if row.kind == "note" {
                    "Note"
                } else {
                    "Bookmark"
                };
                match row.text.filter(|t| !t.trim().is_empty()) {
                    Some(text) => writeln!(out, "{} · {kind} · {text}", format_hms(row.at_s))?,
                    None => writeln!(out, "{} · {kind}", format_hms(row.at_s))?,
                }
            }
            Ok(())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_core::schema::{Bookmark, Note};

    #[test]
    fn parity_timeline_lists_bookmarks_and_notes_in_recording_order() {
        let mut m = broscribe_core::schema::Meeting::new(
            MeetingId::parse("m-000001").unwrap(),
            broscribe_core::schema::Source::Import,
            broscribe_core::schema::now(),
        );
        m.bookmarks.push(Bookmark {
            at_s: 6.0,
            label: None,
        });
        m.notes.push(Note {
            at_s: 2.0,
            text: "Synthetic note".into(),
        });
        let rows = markers(&m);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].kind, rows[0].at_s), ("note", 2.0));
        assert_eq!((rows[1].kind, rows[1].text), ("bookmark", None));
    }
}
