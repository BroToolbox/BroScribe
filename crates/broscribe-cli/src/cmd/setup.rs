//! `setup`: the terminal setup assistant (design §4.1, M1 steps). Interactive
//! on a terminal (prompts on stderr), flag-driven otherwise; `--json` implies
//! `--non-interactive`. Completed steps are saved in the config.

use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};

use broscribe_core::config::Config;
use broscribe_core::index::Index;
use broscribe_core::schema::{
    Person, Strength, Voiceprint, VoiceprintKind, VoiceprintOrigin, VoiceprintSource,
    encode_vector, now, voiceprint_id,
};
use broscribe_core::{Error, Library, MeetingId, Result, WriteTx, names, ops};
use broscribe_models::{ModelStore, PullOptions, PullOutcome, Registry, pull};
use serde_json::{Value, json};

use super::models::{profile_models, role_name};
use crate::Ctx;
use crate::args::parse_range;
use crate::cli::{SetupArgs, SetupContact as Contact};

fn fold(s: &str) -> String {
    s.trim().to_lowercase()
}

pub(super) fn mark(config: &mut Config, step: &str) {
    if !config.setup.completed_steps.iter().any(|s| s == step) {
        config.setup.completed_steps.push(step.to_owned());
    }
}

/// Saves the user's own name and e-mails in the config: the batch import
/// names a new Me and recognises the user by these.
pub(super) fn remember_me(config: &mut Config, name: Option<&str>, emails: &[String]) {
    if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
        config.setup.name = Some(n.to_owned());
    }
    for e in emails.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
        if !config.setup.emails.iter().any(|x| fold(x) == fold(e)) {
            config.setup.emails.push(e.to_owned());
        }
    }
}

/// The user's own name as given (`--name`, `--spec-stdin`, the prompt or the
/// name `setup` saved): absent or blank means not given; anything else must
/// pass the person-name policy every other name writer applies
/// ([`names::validate_name`]), before it reaches the Me person or the
/// config. Returns the trimmed name.
pub(super) fn admit_me_name(name: Option<&str>) -> Result<Option<&str>> {
    name.filter(|n| !n.trim().is_empty())
        .map(names::validate_name)
        .transpose()
}

/// Prompts on stderr and reads one line; empty input keeps `default`.
fn ask(ctx: &mut Ctx<'_>, question: &str, default: Option<&str>) -> Result<Option<String>> {
    match default {
        Some(d) if !d.is_empty() => {
            let _ = write!(ctx.err, "{question} [{d}]: ");
        }
        _ => {
            let _ = write!(ctx.err, "{question}: ");
        }
    }
    let _ = ctx.err.flush();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| Error::io("reading the answer", e))?;
    let line = line.trim();
    Ok(if line.is_empty() {
        default.filter(|d| !d.is_empty()).map(str::to_owned)
    } else {
        Some(line.to_owned())
    })
}

fn interview(ctx: &mut Ctx<'_>, a: &mut SetupArgs, config: &Config, lib: &Library) -> Result<()> {
    ctx.note("BroScribe setup. Press Enter to keep the value in brackets or skip a step.");
    if !a.auto_language && a.lang.is_none() {
        a.lang = ask(
            ctx,
            "Main meeting language (e.g. en, pt-BR)",
            config.default_language.as_deref(),
        )?;
    }
    let me = lib.self_person()?;
    if a.name.is_none() {
        a.name = ask(ctx, "Your name", me.as_ref().map(|p| p.name.as_str()))?;
    }
    if a.emails.is_empty() {
        let current = me.as_ref().map(|p| p.emails.join(", ")).unwrap_or_default();
        if let Some(e) = ask(
            ctx,
            "Your e-mail addresses (comma-separated)",
            Some(current.as_str()),
        )? {
            a.emails = e
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    if a.words_file.is_none() {
        a.words_file = ask(
            ctx,
            "File of names and terms to recognise, one per line (optional)",
            None,
        )?
        .map(PathBuf::from);
    }
    if a.people_file.is_none() {
        a.people_file = ask(
            ctx,
            "Contacts file to seed people, CSV or vCard (optional)",
            None,
        )?
        .map(PathBuf::from);
    }
    if !a.pull_models {
        let answer = ask(ctx, "Download the models now? (y/N)", None)?;
        a.pull_models = answer.is_some_and(|s| matches!(fold(&s).as_str(), "y" | "yes"));
    }
    if a.enroll_me.is_none() {
        a.enroll_me = ask(
            ctx,
            "Enroll your voice from a meeting ID or an audio file (optional)",
            None,
        )?;
        if a.enroll_me.is_some() {
            a.range = ask(
                ctx,
                "Seconds where only you speak, start-end (e.g. 12-45)",
                None,
            )?;
            if a.range.is_none() {
                a.enroll_me = None;
            }
        }
    }
    Ok(())
}

/// The contacts of a people file, admitted whole before anything is saved:
/// a blank name counts as none (the contact goes by its e-mail), and the name
/// each contact would give a new person (its name, else its first e-mail)
/// must pass [`names::validate_name`]. One refused contact refuses the file,
/// naming its position, so no contact of it is seeded.
fn parse_people_file(path: &Path) -> Result<Vec<Contact>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io_at(path, e))?;
    let contacts = if text.to_ascii_uppercase().contains("BEGIN:VCARD") {
        parse_vcard(&text)
    } else {
        parse_csv(&text)
    };
    admit_contacts(contacts, "of the people file")
}

fn admit_contacts(contacts: Vec<Contact>, source: &str) -> Result<Vec<Contact>> {
    let mut admitted = Vec::with_capacity(contacts.len());
    for (n, Contact { name, emails }) in contacts.into_iter().enumerate() {
        let name = name.filter(|name| !name.trim().is_empty());
        let Some(shown) = name
            .as_deref()
            .or_else(|| emails.first().map(String::as_str))
        else {
            continue;
        };
        let valid = match names::validate_name(shown) {
            Ok(valid) => valid.to_owned(),
            Err(Error::InvalidInput(why)) => {
                return Err(Error::InvalidInput(format!(
                    "contact {} {source}: {why}",
                    n + 1
                )));
            }
            Err(e) => return Err(e),
        };
        // An e-mail-only contact keeps no name; seeding names it by e-mail.
        let name = name.map(|_| valid);
        admitted.push(Contact { name, emails });
    }
    Ok(admitted)
}

fn vcard_unescape(s: &str) -> String {
    s.replace("\\,", ",")
        .replace("\\;", ";")
        .replace("\\n", " ")
        .replace("\\N", " ")
        .replace("\\\\", "\\")
        .trim()
        .to_owned()
}

fn parse_vcard(text: &str) -> Vec<Contact> {
    // Unfold continuation lines (RFC 6350 §3.2).
    let mut lines: Vec<String> = Vec::new();
    for raw in text.lines() {
        let raw = raw.trim_end_matches('\r');
        if raw.starts_with([' ', '\t'])
            && let Some(last) = lines.last_mut()
        {
            last.push_str(&raw[1..]);
        } else {
            lines.push(raw.to_owned());
        }
    }
    let mut out = Vec::new();
    let mut card: Option<Contact> = None;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let prop = key.split(';').next().unwrap_or("");
        let prop = prop.rsplit('.').next().unwrap_or(prop).to_ascii_uppercase();
        match prop.as_str() {
            "BEGIN" if value.trim().eq_ignore_ascii_case("VCARD") => {
                card = Some(Contact::default())
            }
            "END" if value.trim().eq_ignore_ascii_case("VCARD") => {
                if let Some(c) = card.take()
                    && (c.name.is_some() || !c.emails.is_empty())
                {
                    out.push(c);
                }
            }
            "FN" => {
                if let Some(c) = card.as_mut() {
                    let v = vcard_unescape(value);
                    if !v.is_empty() {
                        c.name = Some(v);
                    }
                }
            }
            "EMAIL" => {
                if let Some(c) = card.as_mut() {
                    let v = vcard_unescape(value);
                    if v.contains('@') {
                        c.emails.push(v);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Splits one CSV record (double-quoted fields, `""` escapes).
fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' | ';' if !quoted => fields.push(std::mem::take(&mut cur).trim().to_owned()),
            _ => cur.push(c),
        }
    }
    fields.push(cur.trim().to_owned());
    fields
}

/// CSV with a header naming a `name` and an `e-mail` column, or headerless
/// `name,email` rows.
fn parse_csv(text: &str) -> Vec<Contact> {
    let mut rows = text
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .map(csv_fields);
    let Some(first) = rows.next() else {
        return Vec::new();
    };
    let header: Vec<String> = first.iter().map(|h| fold(h)).collect();
    let email_col = header.iter().position(|h| h.contains("mail"));
    let name_col = header
        .iter()
        .position(|h| h == "name" || h == "full name" || h == "display name")
        .or_else(|| header.iter().position(|h| h.contains("name")));
    let has_header = email_col.is_some() || name_col.is_some();
    let (name_col, email_col) = if has_header {
        (name_col, email_col)
    } else {
        (Some(0), Some(1))
    };
    let pick = |row: &[String], col: Option<usize>| {
        col.and_then(|i| row.get(i))
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    let data: Vec<Vec<String>> = if has_header {
        rows.collect()
    } else {
        std::iter::once(first).chain(rows).collect()
    };
    data.iter()
        .filter_map(|row| {
            let name = pick(row.as_slice(), name_col);
            let emails: Vec<String> = pick(row.as_slice(), email_col)
                .filter(|e| e.contains('@'))
                .into_iter()
                .collect();
            (name.is_some() || !emails.is_empty()).then_some(Contact { name, emails })
        })
        .collect()
}

fn add_emails(person: &mut Person, emails: &[String]) -> bool {
    let mut changed = false;
    for e in emails {
        let e = e.trim();
        if !e.is_empty() && !person.emails.iter().any(|x| fold(x) == fold(e)) {
            person.emails.push(e.to_owned());
            changed = true;
        }
    }
    changed
}

/// Re-renders `meeting.md` of every meeting that references `person`
/// (speakers, participants, line corrections or an enrichment's line
/// people; after a rename); returns how many.
pub(super) fn rerender_meetings_of(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    person: &Person,
) -> Result<usize> {
    let lib = tx.library().clone();
    let mut n = 0;
    for dir in lib.list_meeting_dirs()? {
        let m = lib.load_meeting_at(&dir)?;
        if ops::meeting_references_person(&m, &person.id) {
            tx.save_meeting(&m)?;
            index.upsert_meeting(tx, &m)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Creates or updates the Me person: the existing self person, else the
/// person matching one of the e-mails or the name, else a new one. A name
/// [`admit_me_name`] refuses changes nothing.
pub(super) fn upsert_me(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    name: Option<&str>,
    emails: &[String],
) -> Result<(Person, bool)> {
    let name = admit_me_name(name)?;
    let lib = tx.library().clone();
    let mut found = lib.self_person()?;
    if found.is_none() {
        for e in emails {
            if let Some(p) = lib.find_person_by_email(e)? {
                found = Some(p);
                break;
            }
        }
    }
    if found.is_none()
        && let Some(n) = name
    {
        found = lib.find_person_by_name(n)?;
    }
    let created = found.is_none();
    let mut me = found.unwrap_or_else(|| Person::new(lib.new_person_id(), name.unwrap_or("")));
    let before = me.clone();
    me.is_self = true;
    let renamed = match name {
        Some(n) if n != me.name => {
            me.name = n.to_owned();
            true
        }
        _ => false,
    };
    add_emails(&mut me, emails);
    if created || me != before {
        tx.save_person(&me)?;
        index.upsert_person(tx, &me)?;
        if renamed && !created {
            rerender_meetings_of(tx, index, &me)?;
        }
    }
    Ok((me, created))
}

/// Seeds people from contacts; returns (created, updated).
fn seed_people(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    contacts: &[Contact],
) -> Result<(usize, usize)> {
    let lib = tx.library().clone();
    let (mut created, mut updated) = (0, 0);
    for c in contacts {
        let mut found = None;
        for e in &c.emails {
            if let Some(p) = lib.find_person_by_email(e)? {
                found = Some(p);
                break;
            }
        }
        if found.is_none()
            && let Some(n) = &c.name
        {
            found = lib.find_person_by_name(n)?;
        }
        match found {
            Some(mut p) => {
                if add_emails(&mut p, &c.emails) {
                    tx.save_person(&p)?;
                    index.upsert_person(tx, &p)?;
                    updated += 1;
                }
            }
            None => {
                let name = c
                    .name
                    .clone()
                    .or_else(|| c.emails.first().cloned())
                    .unwrap_or_default();
                let mut p = Person::new(lib.new_person_id(), name);
                add_emails(&mut p, &c.emails);
                tx.save_person(&p)?;
                index.upsert_person(tx, &p)?;
                created += 1;
            }
        }
    }
    Ok((created, updated))
}

/// The words of a words file: one per line; blank lines and `#` comments skipped.
fn read_word_list(path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io_at(path, e))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|w| !w.is_empty() && !w.starts_with('#'))
        .map(str::to_owned)
        .collect())
}

/// Adds the `words` not already in `into` (case-insensitive); returns how many.
fn merge_words(into: &mut Vec<String>, words: &[String]) -> usize {
    let mut added = 0;
    for w in words {
        if !into.iter().any(|x| fold(x) == fold(w)) {
            into.push(w.clone());
            added += 1;
        }
    }
    added
}

/// `--enroll-me <meeting|file> --range a-b`: embeds the range outside the
/// lock, then stores enrollment voiceprints on the Me person.
fn enroll_me(
    ctx: &mut Ctx<'_>,
    lib: &Library,
    config: &Config,
    source: &str,
    range: (f64, f64),
    name: Option<&str>,
    emails: &[String],
) -> Result<Value> {
    let deps = ctx.deps;
    let (audio, meeting_id, sha) = match MeetingId::parse(source.trim()) {
        Ok(id) => {
            let m = lib.load_meeting(&id)?;
            let (Some(path), Some(rec)) = (lib.meeting_audio_path(&m)?, m.recording.as_ref())
            else {
                return Err(Error::NotFound(format!(
                    "the recording of {id} is not available"
                )));
            };
            let sha = rec.sha256.clone();
            (path, Some(id), sha)
        }
        Err(_) => {
            let path = PathBuf::from(source);
            if !path.is_file() {
                return Err(Error::NotFound(format!(
                    "{source} is neither a meeting ID nor an audio file"
                )));
            }
            let sha = broscribe_pipeline::audio::file_sha256(&path)?.0;
            (path, None, sha)
        }
    };
    let model = config.profile.speaker_embedding.clone();
    let embedder = deps.models.load_embedder(&model)?;
    ctx.note(format!(
        "Embedding {:.1}-{:.1} s of {}…",
        range.0,
        range.1,
        audio.display()
    ));
    let samples = broscribe_pipeline::confirm::embed_file_ranges(
        &deps.decoder,
        embedder.as_ref(),
        &audio,
        &[range],
    )?;
    if samples.is_empty() {
        return Err(Error::InvalidInput(format!(
            "no usable speech in {:.1}-{:.1} s: pick a longer range where only you speak",
            range.0, range.1
        )));
    }
    // Enrollment always has its audio: key by the recording.
    let origin = VoiceprintOrigin::Recording { sha256: &sha };
    let (me, created) = ops::with_index(lib, |tx, index| {
        let (me, created) = upsert_me(tx, index, name, emails)?;
        let prints: Vec<Voiceprint> = samples
            .iter()
            .map(|s| {
                let (start, end) = (f64::from(s.start), f64::from(s.end));
                Voiceprint {
                    id: voiceprint_id(
                        &me.id,
                        &model,
                        origin,
                        start,
                        end,
                        VoiceprintKind::Enrollment,
                    ),
                    model: model.clone(),
                    dim: s.vector.len(),
                    vector_b64: encode_vector(&s.vector),
                    source: VoiceprintSource {
                        kind: VoiceprintKind::Enrollment,
                        meeting_id: meeting_id.clone(),
                        sha256: Some(sha.clone()),
                        start,
                        end,
                        ranges: Vec::new(),
                        speaker_id: None,
                        speaker_namespace: String::new(),
                    },
                    device: None,
                    strength: Strength::Confirmed,
                    created_at: now(),
                }
            })
            .collect();
        tx.upsert_voiceprints(&me.id, prints)?;
        index.upsert_person(tx, &me)?;
        Ok((me, created))
    })?;
    Ok(json!({
        "person_id": me.id,
        "created": created,
        "model": model,
        "voiceprints": samples.len(),
        "source": source,
        "range": [range.0, range.1],
    }))
}

pub(super) fn set_deferred(ctx: &mut Ctx<'_>, deferred: bool) -> Result<()> {
    Config::update(ctx.data_dir(), |config| {
        ctx.check_request_cancelled()?;
        config.setup.defer_assistant = deferred;
        Ok(())
    })?;
    ctx.emit(json!({"deferred": deferred}), |out| {
        let message = if deferred {
            "Setup assistant deferred."
        } else {
            "Setup assistant deferral cleared."
        };
        writeln!(out, "{message}")
    })
}

pub(super) fn complete_step(ctx: &mut Ctx<'_>, step: &'static str) -> Result<()> {
    let config = Config::update(ctx.data_dir(), |config| {
        ctx.check_request_cancelled()?;
        mark(config, step);
        Ok(())
    })?
    .0;
    super::daemon::emit(
        ctx,
        json!({"completed_steps": config.setup.completed_steps}),
    )
}

fn recording_permissions_answered(value: &Value) -> bool {
    ["microphone", "system_audio"].iter().all(|key| {
        matches!(
            value[key].as_str(),
            Some("granted" | "denied" | "restricted" | "unsupported")
        )
    })
}

pub(super) fn recording_access(
    ctx: &mut Ctx<'_>,
    request: Option<crate::cli::SetupAudioPermission>,
) -> Result<()> {
    use crate::cli::SetupAudioPermission::{Microphone, SystemAudio};
    use broscribe_ipc::Call;
    let call = match request {
        Some(request) => Call::PermissionsRequest {
            permission: match request {
                Microphone => "microphone",
                SystemAudio => "system_audio",
            }
            .into(),
        },
        None => Call::PermissionsStatus,
    };
    let permissions = super::daemon::required_call(ctx, call)?;
    let completed = recording_permissions_answered(&permissions);
    let config = if completed {
        Config::update(ctx.data_dir(), |config| {
            ctx.check_request_cancelled()?;
            mark(config, "permissions");
            Ok(())
        })?
        .0
    } else {
        ctx.config()?
    };
    super::daemon::emit(
        ctx,
        json!({
            "permissions": permissions,
            "setup_completed": completed,
            "completed_steps": config.setup.completed_steps,
        }),
    )
}

pub(super) fn setup(ctx: &mut Ctx<'_>, mut a: SetupArgs) -> Result<()> {
    // A refused name stops setup before the library or the config changes.
    admit_me_name(a.name.as_deref())?;
    let data_dir = ctx.data_dir().to_path_buf();
    let mut config = ctx.config()?;
    let lib = if ctx.request_cancel.is_some() {
        ctx.library_existing(&config)?
    } else {
        ctx.create_library(&config)?
    };
    let interactive = !a.non_interactive && !ctx.json && std::io::stdin().is_terminal();
    if interactive {
        interview(ctx, &mut a, &config, &lib)?;
    }
    // The answer to the prompt too, and every contact of the people file,
    // before any step below saves anything.
    let name = admit_me_name(a.name.as_deref())?;
    let contacts = match a.contacts.take() {
        Some(contacts) => Some(admit_contacts(contacts, "of the request")?),
        None => a
            .people_file
            .as_deref()
            .map(parse_people_file)
            .transpose()?,
    };
    let range = a.range.as_deref().map(parse_range).transpose()?;
    // Profiles come from the built-in registry (design §8.3).
    let profile = a
        .profile
        .as_deref()
        .map(|p| Registry::builtin().and_then(|r| r.profile(p.trim()).cloned()))
        .transpose()?;
    let mut report = serde_json::Map::new();

    ctx.check_request_cancelled()?;

    // Every config write below reloads the file under the data directory's
    // config lock and applies only what this step sets, so settings written
    // meanwhile (by the app or another command) survive.
    let lang = a
        .lang
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned);
    let words = match a.words.take() {
        Some(words) => Some(
            words
                .into_iter()
                .filter_map(|word| {
                    let word = word.trim();
                    (!word.is_empty() && !word.starts_with('#')).then(|| word.to_owned())
                })
                .collect(),
        ),
        None => a.words_file.as_deref().map(read_word_list).transpose()?,
    };
    // 3. Models: a profile that changes the speaker-embedding model takes
    // the `models use speaker_embedding` path: the new model loads before
    // anything is saved, and the library is re-embedded after. With
    // `--pull-models` every model the profile needs (live preview included)
    // is downloaded first, so the new embedder can load; a failed load
    // still leaves the config untouched.
    let target = profile.as_ref().map(|p| {
        let mut next = config.clone();
        next.profile = p.apply(&config.profile);
        next
    });
    let registry = (target.is_some() || a.pull_models)
        .then(|| Registry::load(&data_dir))
        .transpose()?;
    if let (Some(registry), Some(next)) = (&registry, &target) {
        // The chosen profile is complete (its live model included) before
        // anything is downloaded or saved.
        let entries = profile_models(registry, &next.profile)?;
        if ctx.request_library.is_some() {
            for (entry, _) in entries {
                entry.check_license(false)?;
            }
        }
    }
    let pulled = if let (true, Some(registry)) = (a.pull_models, &registry) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let mut opts = PullOptions::from_env(a.allow_unverified_license);
        let cancel = match &ctx.request_cancel {
            Some(cancel) => Arc::clone(cancel),
            None => {
                let cancel = Arc::new(AtomicBool::new(false));
                if a.cancel_on_stdin_eof {
                    super::models::cancel_at_stdin_eof(Arc::clone(&cancel));
                }
                super::enrich::watch_interrupts(
                    Arc::clone(&cancel),
                    super::models::PULL_STOP_NOTICE,
                    ctx.background_notes(),
                );
                cancel
            }
        };
        opts.cancel = Some(Arc::clone(&cancel));
        let speech = &target.as_ref().unwrap_or(&config).profile;
        let entries = profile_models(registry, speech)?;
        let space = model_space(&data_dir, entries.iter().map(|(entry, _)| *entry), false)?;
        for (entry, _) in &entries {
            entry.check_license(a.allow_unverified_license)?;
        }
        space.require_disk()?;
        if space.low_memory {
            ctx.note("Large speech models may be slow with less than 8 GiB of memory.");
        }
        let mut pulled = Vec::new();
        let model_count = entries.len() as f64;
        for (position, (entry, roles)) in entries.into_iter().enumerate() {
            let progress_updates = ctx.request_progress.clone();
            let id = entry.id.as_str();
            let err = &mut *ctx.err;
            let mut last = u64::MAX;
            let mut progress = |done: u64, total: u64| {
                let pct = (done * 100).checked_div(total).unwrap_or(100);
                if pct / 10 != last / 10 {
                    last = pct;
                    if let Some(updates) = &progress_updates {
                        let done = (position as f64 + pct as f64 / 100.0) / model_count;
                        updates.send_replace(Some((done, 1.0)));
                    }
                    let _ = writeln!(err, "{id}: {pct}%");
                }
            };
            let outcome = match pull(entry, &data_dir, &opts, &mut progress, &ctx.deps.execution) {
                Ok(outcome) => outcome,
                Err(_) if cancel.load(Ordering::SeqCst) => {
                    return Err(Error::Service {
                        code: "cancelled",
                        message: format!("download of {id} cancelled; completed installs are kept"),
                    });
                }
                Err(e) => return Err(e),
            };
            pulled.push(json!({
                "roles": roles.iter().map(|r| role_name(*r)).collect::<Vec<_>>(),
                "id": id,
                "outcome": match outcome {
                    PullOutcome::AlreadyInstalled => "already_installed",
                    PullOutcome::Installed => "installed",
                },
            }));
        }
        Some(pulled)
    } else {
        None
    };
    let reembed_with = match &target {
        Some(next) => {
            super::models::speaker_switch_embedder(ctx, next, &config.profile.speaker_embedding)?
        }
        None => None,
    };
    let mut words_added = 0;
    if a.auto_language || lang.is_some() || profile.is_some() || words.is_some() {
        config = Config::update(&data_dir, |c| {
            // 1. Languages.
            if a.auto_language || lang.is_some() {
                c.default_language = lang.clone();
                mark(c, "languages");
            }
            // 3. Models: profile.
            if let Some(p) = &profile {
                c.profile = p.apply(&c.profile);
                c.setup.completed_steps.retain(|step| step != "models");
                if let Some(registry) = &registry {
                    let store = ModelStore::new(&data_dir);
                    if profile_models(registry, &c.profile)?
                        .iter()
                        .all(|(entry, _)| store.is_installed(&entry.id))
                    {
                        mark(c, "models");
                    }
                }
            }
            // 7. Words.
            if let Some(words) = &words {
                words_added = merge_words(&mut c.hotwords, words);
                mark(c, "words");
            }
            Ok(())
        })?
        .0;
    }
    if a.auto_language || lang.is_some() {
        report.insert("language".into(), json!(lang));
    }
    if let Some(p) = &profile {
        report.insert("profile".into(), json!(p.id));
    }
    if let Some(embedder) = reembed_with {
        let r = super::models::reembed_library(ctx, &config, embedder.as_ref())?;
        report.insert(
            "reembed".into(),
            serde_json::to_value(r).map_err(|e| Error::Internal(e.to_string()))?,
        );
    }
    if words.is_some() {
        report.insert("words_added".into(), json!(words_added));
    }

    // 3. Models: download (done above).
    if let Some(pulled) = pulled {
        config = Config::update(&data_dir, |c| {
            mark(c, "models");
            Ok(())
        })?
        .0;
        report.insert("models".into(), json!(pulled));
    }

    // 4. Your voice: the Me person.
    if name.is_some() || !a.emails.is_empty() {
        let (me, created) =
            ops::with_index(&lib, |tx, index| upsert_me(tx, index, name, &a.emails))?;
        config = Config::update(&data_dir, |c| {
            remember_me(c, name, &a.emails);
            mark(c, "me");
            Ok(())
        })?
        .0;
        report.insert(
            "me".into(),
            json!({ "person_id": me.id, "name": me.name, "emails": me.emails, "created": created }),
        );
    }
    // 7. People.
    if let Some(contacts) = &contacts {
        let (created, updated) =
            ops::with_index(&lib, |tx, index| seed_people(tx, index, contacts))?;
        config = Config::update(&data_dir, |c| {
            mark(c, "people");
            Ok(())
        })?
        .0;
        report.insert(
            "people".into(),
            json!({ "contacts": contacts.len(), "created": created, "updated": updated }),
        );
    }
    // 4. Your voice: enrollment.
    if let (Some(source), Some(range)) = (a.enroll_me.as_deref(), range) {
        let enrolled = enroll_me(ctx, &lib, &config, source, range, name, &a.emails)?;
        config = Config::update(&data_dir, |c| {
            mark(c, "voice");
            Ok(())
        })?
        .0;
        report.insert("enrollment".into(), enrolled);
    }

    let mut missing = Vec::new();
    if !config
        .setup
        .completed_steps
        .iter()
        .any(|step| step == "languages")
    {
        missing.push("--lang");
    }
    if lib.self_person()?.is_none() {
        missing.push("--name/--email");
    }
    report.insert(
        "completed_steps".into(),
        json!(config.setup.completed_steps),
    );
    report.insert("next".into(), json!(missing));
    report.insert("deferred".into(), json!(config.setup.defer_assistant));
    // Never prompts: the grant itself is an explicit action in the app.
    let accessibility = accessibility_report(
        broscribe_platform_native::NativePlatform::app_ui_reader().is_trusted(),
    );
    report.insert("accessibility".into(), accessibility.clone());
    let value = Value::Object(report);
    ctx.emit(value.clone(), |out| {
        if let Value::Object(map) = &value {
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "next" | "completed_steps" | "accessibility" | "deferred"
                ) {
                    continue;
                }
                writeln!(out, "{}:", k.replace('_', " "))?;
                super::write_value(out, v, 1)?;
            }
        }
        writeln!(
            out,
            "Completed steps: {}",
            if config.setup.completed_steps.is_empty() {
                "none".to_owned()
            } else {
                config.setup.completed_steps.join(", ")
            }
        )?;
        if config.setup.defer_assistant {
            writeln!(out, "Setup assistant deferred.")?;
        }
        if !missing.is_empty() {
            writeln!(out, "Still to do: {}", missing.join(", "))?;
        }
        writeln!(out, "{}", accessibility_line(&accessibility))?;
        writeln!(out, "Run `broscribe doctor` to check everything.")
    })
}

const ACCESSIBILITY_ABOUT: &str = "Accessibility is optional. It reads participant names and captions from meeting apps; audio recording works without it.";
/// macOS grants Accessibility per app: what this command can check is the
/// terminal running it, never BroScribe.app.
const ACCESSIBILITY_SCOPE: &str =
    "This checks the terminal running this command, not BroScribe.app, which has its own grant.";
const ACCESSIBILITY_GRANT: &str = "To let BroScribe read meeting apps, choose Continue on Meeting app names in the app's setup assistant, or turn BroScribe on in the system's accessibility settings.";

fn accessibility_report(terminal_granted: bool) -> Value {
    json!({
        "optional": true,
        "terminal_granted": terminal_granted,
        "scope": ACCESSIBILITY_SCOPE,
        "about": ACCESSIBILITY_ABOUT,
        "grant": ACCESSIBILITY_GRANT,
    })
}

fn accessibility_line(report: &Value) -> String {
    let terminal = if report["terminal_granted"] == json!(true) {
        "granted"
    } else {
        "not granted"
    };
    format!(
        "Accessibility for this terminal: {terminal}. {ACCESSIBILITY_SCOPE} {ACCESSIBILITY_ABOUT} \
         {ACCESSIBILITY_GRANT}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_reports_terminal_accessibility_and_how_to_grant_the_app() {
        let denied = accessibility_report(false);
        assert_eq!(denied["optional"], json!(true));
        assert_eq!(denied["terminal_granted"], json!(false));
        assert!(denied.get("granted").is_none());
        assert!(
            denied["scope"]
                .as_str()
                .unwrap()
                .contains("not BroScribe.app")
        );
        let line = accessibility_line(&denied);
        assert!(line.starts_with("Accessibility for this terminal: not granted."));
        assert!(line.contains("not BroScribe.app"));
        assert!(line.contains("recording works without it"));
        assert!(line.contains("Meeting app names"));
        // A terminal's grant says nothing about the app: the app guidance stays.
        let granted = accessibility_report(true);
        assert!(
            granted["grant"]
                .as_str()
                .unwrap()
                .contains("choose Continue on Meeting app names")
        );
        let line = accessibility_line(&granted);
        assert!(line.starts_with("Accessibility for this terminal: granted."));
        assert!(line.contains("not BroScribe.app"));
    }

    #[test]
    fn csv_with_header_and_quotes() {
        let c = parse_csv("Full Name,E-mail\n\"Example, Ana\",ana@example.com\nBo Test,\n");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name.as_deref(), Some("Example, Ana"));
        assert_eq!(c[0].emails, ["ana@example.com"]);
        assert!(c[1].emails.is_empty());
    }

    #[test]
    fn csv_without_header() {
        let c = parse_csv("Ana Example,ana@example.com\n");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name.as_deref(), Some("Ana Example"));
    }

    #[test]
    fn vcard_names_and_emails() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ana\r\n  Example\r\nitem1.EMAIL;TYPE=INTERNET:ana@example.com\r\nTEL:123\r\nEND:VCARD\r\nBEGIN:VCARD\r\nFN:Bo Test\r\nEND:VCARD\r\n";
        let c = parse_vcard(text);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name.as_deref(), Some("Ana Example"));
        assert_eq!(c[0].emails, ["ana@example.com"]);
        assert_eq!(c[1].name.as_deref(), Some("Bo Test"));
    }
}

#[derive(serde::Serialize)]
pub(super) struct ModelSpace {
    download_bytes: u64,
    free_bytes: Option<u64>,
    memory_bytes: Option<u64>,
    low_disk: bool,
    low_memory: bool,
    unverified_licences: Vec<String>,
}

impl ModelSpace {
    pub(super) fn require_disk(&self) -> Result<()> {
        if self.low_disk {
            return Err(Error::Service {
                code: "disk_low",
                message: format!(
                    "{} bytes to download, {} bytes free on this disk",
                    self.download_bytes,
                    self.free_bytes.unwrap_or_default()
                ),
            });
        }
        Ok(())
    }
}

pub(super) fn model_space<'a>(
    data_dir: &Path,
    entries: impl IntoIterator<Item = &'a broscribe_models::ModelEntry>,
    force: bool,
) -> Result<ModelSpace> {
    let store = ModelStore::new(data_dir);
    let mut download_bytes = 0u64;
    let mut unverified_licences = Vec::new();
    for m in entries {
        if !m.license_verified {
            unverified_licences.push(m.id.clone());
        }
        if force || store.doctor(m, false)? != broscribe_models::Health::Installed {
            download_bytes = download_bytes.saturating_add(m.total_bytes());
        }
    }
    let system = broscribe_platform_native::NativePlatform::system_info();
    let model_dir = data_dir.join("models");
    let disk_root = model_dir
        .ancestors()
        .find(|p| p.is_dir())
        .unwrap_or(data_dir);
    let free_bytes = system.free_disk_bytes(disk_root).ok();
    let memory_bytes = Some(system.memory_bytes()).filter(|bytes| *bytes > 0);
    Ok(ModelSpace {
        download_bytes,
        free_bytes,
        memory_bytes,
        low_disk: broscribe_platform::model_download_low_disk(download_bytes, free_bytes),
        low_memory: broscribe_platform::model_download_low_memory(memory_bytes),
        unverified_licences,
    })
}

pub(super) fn model_check(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    let registry = Registry::load(ctx.data_dir())?;
    let models = profile_models(&registry, &config.profile)?;
    let space = model_space(
        ctx.data_dir(),
        models.iter().map(|(entry, _)| *entry),
        false,
    )?;
    ctx.emit(crate::to_value(&space)?, |out| {
        writeln!(out, "{} bytes to download", space.download_bytes)?;
        if let Some(free) = space.free_bytes {
            writeln!(out, "{free} bytes free on this disk")?;
        }
        if let Some(memory) = space.memory_bytes {
            writeln!(out, "{memory} bytes of memory")?;
        }
        if space.low_disk {
            writeln!(out, "Free up disk space before downloading.")?;
        }
        if space.low_memory {
            writeln!(
                out,
                "Large speech models may be slow with less than 8 GiB of memory."
            )?;
        }
        for id in &space.unverified_licences {
            writeln!(out, "{id}: licence acknowledgement required")?;
        }
        Ok(())
    })
}
