//! `audio devices`, `setup voice` and `setup try` (plan §8 items 2, 11, 16,
//! 18, 19, 22): the CLI side of the app's "Your voice" and "Try it" steps.
//!
//! - Enrollment by passage takes exactly two recordings. The first: the
//!   user reads the supplied simple text aloud for about 30 s, starting it
//!   again if they finish early (no improvising needed). The second: a
//!   separate confirmation with a short simple text, read and repeated until
//!   the recording stops (`--mic` records `CONFIRM_MAX_CAPTURE_S`), which
//!   does not count towards the 30 s (optional with `--from-file`).
//! - The first recording goes through VAD → audio checks (clipping, level,
//!   short speech, noise; reported, not fatal) → chunks of ≥ 3 s (≥ 4
//!   needed), all in `analyze_reading`, shared by the readiness check,
//!   `--mic`'s early check and enrollment so they cannot disagree. Then
//!   embeddings → optional confirmation-phrase check → one commit under the
//!   library lock. With `--session <id>` the commit happens only while the
//!   config's `setup.voice_session` is `<id>` (the app changes it under the
//!   same lock to cancel). A new passage enrollment replaces the earlier
//!   passage voiceprints and kept passages ("redo").
//! - A confirmation that cannot confirm the reading is an error, unless
//!   `--report-unconfirmed` asks for a `confirmation_unconfirmed` report
//!   instead (exit 0, nothing saved), with `confirmation_issue`:
//!   `too_short` (under `CONFIRM_MIN_SPEECH_S` of detected speech, checked
//!   before the speaker model runs, so no score exists), `unusable` (enough
//!   speech, but the speaker model gave no embedding) or `below_threshold`
//!   (the comparison). Silence never counts towards the minimum.
//!   The report is never a pass: the user then chooses to record the
//!   phrase again or to save the reading by running without the phrase.
//!   A superseded session reports `session_superseded` instead.
//! - `--check-recording --from-file` runs only that analysis of the first
//!   recording, with only the VAD model loaded, and reports whether it has
//!   enough usable parts (`ready`): a duration check, not a promise that
//!   embedding or confirmation will succeed. It writes no library, config,
//!   person or voiceprint; `--session` only chooses the cache folder.
//!   `--mic` runs the same check on its first recording before asking for
//!   the phrase, and stops there when the recording is not ready.
//! - Input files handed to these commands are copied into a private
//!   (0700) child folder at once: under `<cache>/setup-capture/<session>`
//!   for the app's sessions, else under `<cache>/setup-capture-cli/<pid>`
//!   (outside the folder the app sweeps). Exit deletes that folder; the app
//!   cleans its sessions, and CLI runs remove folders of dead CLI processes.
//!   The confirmation phrase and Try-it audio are never kept; the passage is
//!   kept as `people/<me>/enrollment/<sha256>.wav` only with
//!   `--keep-as-enrollment` and a retention policy other than immediate deletion.
//! - `--mic` and `setup try` record with the microphone recordings use (the
//!   pinned one while connected, else the default input). A running
//!   recording agent records them under its own microphone grant
//!   (`capture_sample`); an installed but stopped agent is asked to start;
//!   only without an installed agent does this process open the microphone.
//!   Ctrl-C cancels a capture and keeps nothing (the agent discards its
//!   sample). The two enrollment recordings must come from one microphone;
//!   the voiceprints record its name.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use broscribe_core::config::{CONFIRM_MAX_CAPTURE_S, CONFIRM_MIN_SPEECH_S, Config, Retention};
use broscribe_core::identity;
use broscribe_core::ops::{self, ConfirmationSample};
use broscribe_core::schema::{
    self, Handling, LanguageSpan, Person, Segment, Strength, Voiceprint, VoiceprintKind,
    VoiceprintOrigin, VoiceprintSource,
};
use broscribe_core::{Error, Library, PersonId, Result, fsutil};
use broscribe_ipc::Call;
use broscribe_models::{ModelSet, Vad};
use broscribe_pipeline::analyze::analyze_with_languages;
use broscribe_pipeline::audio::{TARGET_RATE, decode_16k, slice_s};
use broscribe_pipeline::confirm::{
    AudioChecks, ENROLL_MIN_CHUNKS, PhraseCheck, audio_checks, embed_phrase, embed_sample_ranges,
    enrollment_chunks, phrase_check,
};
use broscribe_pipeline::import::import_file_with_recoveries;
use broscribe_pipeline::ranges::Range;
use broscribe_pipeline::worker::wav_16k_bytes;
use broscribe_pipeline::{AnalyzeOptions, Env, ImportOptions};
use broscribe_platform::MicChunk;
use broscribe_platform::ProcessProbe;
use broscribe_platform_native::ipc::CancelToken;
use serde_json::{Value, json};

use super::setup::{admit_me_name, mark, remember_me, upsert_me};
use super::{meeting_id, speaker_id};
use crate::Ctx;
use crate::args::parse_range;
use crate::cli::{TryArgs, VoiceArgs};

/// Folder of kept passages inside a person's folder.
pub(crate) const ENROLLMENT_DIR: &str = "enrollment";
/// Longest passage capture (plan §8 item 16).
const MAX_PASSAGE_S: f64 = 45.0;
/// Longest Try-it capture.
const MAX_TRY_S: f64 = 25.0;
/// Extra wait for the last chunks of a capture.
const CAPTURE_GRACE: Duration = Duration::from_secs(2);

// ---- passages ----

/// Passage files: the passage, a line `---`, the confirmation phrase.
/// Keyed by BCP 47 tag; the first entry of a language is its default.
const PASSAGES: [(&str, &str); 3] = [
    ("en", include_str!("../../passages/en.txt")),
    ("pt-BR", include_str!("../../passages/pt-BR.txt")),
    ("pt-PT", include_str!("../../passages/pt-PT.txt")),
];

pub(crate) struct Passage {
    pub lang: &'static str,
    pub text: &'static str,
    pub phrase: &'static str,
}

/// The passage for `lang`: exact tag (case-insensitive), else the first
/// passage of the same primary language, else English.
pub(crate) fn passage_for(lang: Option<&str>) -> Passage {
    let want = lang
        .map(|l| l.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let primary = want.split(['-', '_']).next().unwrap_or("").to_owned();
    let &(tag, raw) = PASSAGES
        .iter()
        .find(|(tag, _)| tag.to_ascii_lowercase() == want)
        .or_else(|| {
            PASSAGES
                .iter()
                .find(|(tag, _)| tag.split('-').next() == Some(primary.as_str()))
        })
        .unwrap_or(&PASSAGES[0]);
    let (text, phrase) = raw.split_once("\n---\n").unwrap_or((raw, ""));
    Passage {
        lang: tag,
        text: text.trim(),
        phrase: phrase.trim(),
    }
}

// ---- private working copies ----

/// Parent of CLI-owned capture folders, one `<pid>` folder per run. Kept
/// apart from the app's `setup-capture`, which the app sweeps whole.
const CLI_CAPTURE_DIR: &str = "setup-capture-cli";

/// A private (0700) temporary folder, removed with its content on drop.
struct PrivateDir {
    dir: PathBuf,
    /// The run's own `setup-capture-cli/<pid>` parent, removed too.
    run_dir: Option<PathBuf>,
}

impl PrivateDir {
    /// Under `<cache>/setup-capture/<session>` for the app's sessions, else
    /// under `<cache>/setup-capture-cli/<pid>` after removing the folders
    /// of CLI runs that are no longer alive.
    fn new(cache: &Path, session: Option<&str>, probe: &dyn ProcessProbe) -> Result<Self> {
        let (base, run_dir) = match session {
            Some(session)
                if !session.is_empty()
                    && session
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
            {
                (cache.join("setup-capture").join(session), None)
            }
            Some(_) => return Err(Error::InvalidInput("invalid setup session".into())),
            None => {
                let root = cache.join(CLI_CAPTURE_DIR);
                sweep_dead_runs(&root, probe);
                let run = root.join(probe.current_pid().to_string());
                (run.clone(), Some(run))
            }
        };
        let mut parent = fs::DirBuilder::new();
        parent.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            parent.mode(0o700);
        }
        parent.create(&base).map_err(|e| Error::io_at(&base, e))?;
        parent.recursive(false);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        for attempt in 0..32u32 {
            let dir = base.join(format!(
                "broscribe-voice-{}-{:08x}",
                std::process::id(),
                nanos.wrapping_add(attempt.wrapping_mul(0x9E37_79B9))
            ));
            match parent.create(&dir) {
                Ok(()) => return Ok(Self { dir, run_dir }),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Error::io_at(&dir, e)),
            }
        }
        Err(Error::Internal(
            "could not create a private temporary folder".into(),
        ))
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    /// Copies `file` to a fixed private working name; the caller keeps the original.
    fn copy_input(&self, file: &Path, name: &str) -> Result<PathBuf> {
        if !file.is_file() {
            return Err(Error::NotFound(file.display().to_string()));
        }
        let extension = file
            .extension()
            .and_then(|ext| ext.to_str())
            .filter(|ext| {
                (1..=8).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
            })
            .unwrap_or("bin");
        let dest = self
            .dir
            .join(format!("{name}.{}", extension.to_ascii_lowercase()));
        let mut source = fs::File::open(file).map_err(|e| Error::io_at(file, e))?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut target = options.open(&dest).map_err(|e| Error::io_at(&dest, e))?;
        std::io::copy(&mut source, &mut target).map_err(|e| Error::io_at(&dest, e))?;
        Ok(dest)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
        if let Some(run) = &self.run_dir {
            // Only when empty: another folder of this run may still be in use.
            let _ = fs::remove_dir(run);
        }
    }
}

/// Removes `<root>/<pid>` folders of CLI runs whose process is gone (killed
/// before its own cleanup). Live runs' folders are never touched.
fn sweep_dead_runs(root: &Path, probe: &dyn ProcessProbe) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != probe.current_pid() && !probe.is_alive(pid) {
            let path = entry.path();
            if let Err(e) = fs::remove_dir_all(&path) {
                tracing::warn!(path = %path.display(), error = %e, "removing a stale capture folder");
            }
        }
    }
}

// ---- microphone ----

/// Mono samples at `from` Hz to `to` Hz: a box filter when decimating,
/// linear interpolation otherwise.
pub(super) fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() || from == 0 || to == 0 {
        return samples.to_vec();
    }
    let ratio = f64::from(from) / f64::from(to);
    let n = ((samples.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let pos = i as f64 * ratio;
        if ratio > 1.0 {
            let a = pos.floor() as usize;
            let b = (((i + 1) as f64 * ratio).floor() as usize).clamp(a + 1, samples.len());
            let window = &samples[a.min(samples.len() - 1)..b];
            out.push(window.iter().sum::<f32>() / window.len() as f32);
        } else {
            let a = pos.floor() as usize;
            let frac = (pos - a as f64) as f32;
            let x0 = samples[a.min(samples.len() - 1)];
            let x1 = samples[(a + 1).min(samples.len() - 1)];
            out.push(x0 + (x1 - x0) * frac);
        }
    }
    out
}

/// A setup capture: 16 kHz mono samples and the microphone that recorded
/// them, as the capturing side reported it.
struct Captured {
    samples: Vec<f32>,
    /// `None`: the default input.
    device_id: Option<String>,
    device_name: Option<String>,
    /// Recorded by BroScribe Agent under its own microphone grant.
    by_agent: bool,
}

/// The setup capture the user's Ctrl-C (or a termination request) cancels;
/// outside a capture the process ends as usual (exit status 130).
static ACTIVE_CAPTURE: std::sync::Mutex<Option<CancelToken>> = std::sync::Mutex::new(None);
/// The thread that turns a termination request into a capture cancel.
static TERMINATION_WATCH: std::sync::LazyLock<()> = std::sync::LazyLock::new(watch_termination);

fn watch_termination() {
    let started = std::thread::Builder::new()
        .name("setup-capture-cancel".into())
        .spawn(|| {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            while runtime
                .block_on(broscribe_platform_native::ipc::termination())
                .is_ok()
            {
                let active = ACTIVE_CAPTURE
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                match active {
                    Some(token) => token.cancel(),
                    None => std::process::exit(130),
                }
            }
        });
    if let Err(e) = started {
        tracing::warn!(error = %e, "setup capture: Ctrl-C cannot cancel it");
    }
}

/// Makes `token` the capture a termination request cancels until dropped.
struct CaptureCancel;

impl CaptureCancel {
    fn arm(token: &CancelToken) -> Self {
        std::sync::LazyLock::force(&TERMINATION_WATCH);
        *ACTIVE_CAPTURE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(token.clone());
        Self
    }
}

impl Drop for CaptureCancel {
    fn drop(&mut self) {
        ACTIVE_CAPTURE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

fn cancelled() -> Error {
    Error::Service {
        code: "cancelled",
        message: "recording cancelled; nothing was kept".into(),
    }
}

/// Records up to `seconds` for `purpose` (`enrollment`, `try_it`) from the
/// microphone recordings use: the pinned one while it is connected, else
/// the default input (RT06), so enrollment hears the voice as recordings
/// will. A running recording agent records it under its own microphone
/// grant (`capture_sample`); an installed agent that is stopped is asked to
/// start instead; without an installed agent this process records. Ctrl-C
/// cancels it and keeps nothing.
fn capture(ctx: &Ctx<'_>, device: Option<&str>, seconds: f64, purpose: &str) -> Result<Captured> {
    let cancel = ctx
        .request_cancel
        .clone()
        .map(CancelToken::from_flag)
        .unwrap_or_default();
    let _armed = ctx
        .request_cancel
        .is_none()
        .then(|| CaptureCancel::arm(&cancel));
    if super::daemon::optional_call(ctx.data_dir(), Call::Ping)?.is_some() {
        if device.is_some() {
            return Err(Error::InvalidInput(
                "BroScribe Agent records setup samples with the microphone recordings use \
                 (the pinned one, else the default input); pin this one with `broscribe audio \
                 pin <id>` first, or leave out --device"
                    .into(),
            ));
        }
        return capture_by_agent(ctx, seconds, purpose, &cancel);
    }
    match super::daemon::installed(ctx) {
        Ok(false) | Err(Error::Unsupported(_)) => {}
        Ok(true) => {
            return Err(Error::DaemonUnavailable(
                "BroScribe Agent owns microphone access and is installed but not running; \
                 start it with `broscribe daemon start` and try again"
                    .into(),
            ));
        }
        Err(e) => return Err(e),
    }
    let config = ctx.config()?;
    let pinned = broscribe_capture::session::pinned_input(
        ctx.deps.mic.as_ref(),
        config.audio.pinned_mic.as_deref(),
    )
    .map(|d| d.id);
    capture_here(ctx, device.map(str::to_owned).or(pinned), seconds, &cancel)
}

/// `capture_sample` on the running agent; a cancel stops and discards it
/// there. The agent's sample folder is removed once read.
fn capture_by_agent(
    ctx: &Ctx<'_>,
    seconds: f64,
    purpose: &str,
    cancel: &CancelToken,
) -> Result<Captured> {
    let session = broscribe_ipc::RequestId::random().to_string();
    let call = Call::CaptureSample {
        session: session.clone(),
        seconds,
        purpose: purpose.to_owned(),
    };
    let answer = super::daemon::cancellable_call(ctx.data_dir(), call, cancel);
    if cancel.is_cancelled() {
        // Stops the capture and deletes what it wrote, also when the
        // request never reached it.
        let stop = Call::CaptureSampleStop {
            session,
            discard: true,
        };
        if let Err(e) = super::daemon::optional_call(ctx.data_dir(), stop) {
            tracing::warn!(code = e.code(), "setup capture: discard not confirmed");
        }
        return Err(cancelled());
    }
    let value = answer?.ok_or_else(|| {
        Error::DaemonUnavailable("BroScribe Agent stopped during the recording".into())
    })?;
    let path = value["path"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| Error::Internal("BroScribe Agent’s sample names no file".into()))?;
    let key = broscribe_platform::paths::helper_sample_session_key(&session);
    // Only the private folder the agent made for this request.
    let folder = path
        .parent()
        .filter(|dir| {
            dir.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| {
                    broscribe_platform::paths::helper_sample_owner_key(name) == Some(key.as_str())
                })
        })
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            Error::Internal("BroScribe Agent’s sample is not in its own folder".into())
        })?;
    let decoded = decode_16k(&ctx.deps.decoder, &path);
    if let Err(e) = fs::remove_dir_all(&folder)
        && e.kind() != ErrorKind::NotFound
    {
        tracing::warn!(error = %e.kind(), "setup capture: sample folder not removed");
    }
    let samples = decoded?.samples;
    if samples.is_empty() {
        return Err(Error::InvalidInput("no audio was captured".into()));
    }
    Ok(Captured {
        samples,
        device_id: value["device_id"].as_str().map(str::to_owned),
        device_name: value["device_name"].as_str().map(str::to_owned),
        by_agent: true,
    })
}

/// Records up to `seconds` from `device` (default input when `None`) in
/// this process.
fn capture_here(
    ctx: &Ctx<'_>,
    device: Option<String>,
    seconds: f64,
    cancel: &CancelToken,
) -> Result<Captured> {
    let mic = ctx.deps.mic.as_ref();
    let devices = mic.devices()?;
    let listed = match device.as_deref() {
        Some(id) => devices.iter().find(|d| d.id == id),
        None => devices.iter().find(|d| d.is_default),
    }
    .map(|d| d.name.clone());
    let (tx, rx) = mpsc::channel::<MicChunk>();
    let session = mic.start(
        device.as_deref(),
        Box::new(move |chunk| {
            let _ = tx.send(chunk);
        }),
    )?;
    let device_name = session.device_name().or(listed);
    let rate = session.sample_rate();
    let target = (seconds * f64::from(rate)) as usize;
    let deadline = Instant::now() + Duration::from_secs_f64(seconds) + CAPTURE_GRACE;
    let mut raw: Vec<f32> = Vec::with_capacity(target);
    while raw.len() < target && !cancel.is_cancelled() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout((deadline - now).min(Duration::from_millis(200))) {
            Ok(chunk) => raw.extend_from_slice(&chunk.samples_mono_f32),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let failure = session.failure();
    session.stop()?;
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    while let Ok(chunk) = rx.try_recv() {
        raw.extend_from_slice(&chunk.samples_mono_f32);
    }
    raw.truncate(target);
    if raw.is_empty() {
        return Err(match failure {
            Some(f) => Error::Unsupported(format!("the microphone failed: {f}")),
            None => Error::InvalidInput("no audio was captured".into()),
        });
    }
    Ok(Captured {
        samples: resample(&raw, rate, TARGET_RATE),
        device_id: device,
        device_name,
        by_agent: false,
    })
}

fn check_seconds(seconds: f64, max: f64) -> Result<f64> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(Error::InvalidInput(format!(
            "--seconds must be positive: {seconds}"
        )));
    }
    Ok(seconds.min(max))
}

// ---- audio devices ----

pub(super) fn audio_devices(ctx: &mut Ctx<'_>) -> Result<()> {
    let devices = ctx.deps.mic.devices()?;
    let list: Vec<Value> = devices
        .iter()
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name,
                "is_default": d.is_default,
                "sample_rates": d.sample_rates,
            })
        })
        .collect();
    ctx.emit(json!({ "devices": list }), |out| {
        if devices.is_empty() {
            return writeln!(out, "No audio input devices.");
        }
        for d in &devices {
            let mark = if d.is_default { "*" } else { " " };
            writeln!(out, "{mark} {}  ({})", d.name, d.id)?;
        }
        Ok(())
    })
}

// ---- setup voice ----

/// The name to give the Me person: `--name`; when there is no Me yet, the
/// name saved by `setup --name`; a new Me without either is refused. Either
/// name passes [`admit_me_name`] before anything is recorded or saved.
fn me_name(lib: &Library, config: &Config, name: Option<&str>) -> Result<Option<String>> {
    if let Some(n) = admit_me_name(name)? {
        return Ok(Some(n.to_owned()));
    }
    if lib.self_person()?.is_some() {
        return Ok(None);
    }
    match admit_me_name(config.setup.name.as_deref())? {
        Some(n) => Ok(Some(n.to_owned())),
        None => Err(Error::InvalidInput(
            "there is no Me person yet: give --name".into(),
        )),
    }
}

pub(super) fn setup_voice(ctx: &mut Ctx<'_>, a: VoiceArgs) -> Result<()> {
    // Keep working copies in the private directory for the lifetime of the command.
    let private = PrivateDir::new(
        &ctx.paths.cache_dir,
        a.session.as_deref(),
        ctx.deps.process.as_ref(),
    )?;
    let passage_file = a
        .from_file
        .as_deref()
        .map(|f| private.copy_input(f, "passage-input"))
        .transpose()?;
    let phrase_file = a
        .confirm_file
        .as_deref()
        .map(|f| private.copy_input(f, "confirmation-input"))
        .transpose()?;
    let config = ctx.config()?;
    if a.check_recording {
        // Read-only: the VAD model alone, no library, Me, config write or
        // session comparison; `--session` only placed the file.
        let path = passage_file
            .as_deref()
            .ok_or_else(|| Error::InvalidInput("--check-recording needs --from-file".into()))?;
        let mut vad = ctx.deps.models.load_vad(&config.profile.vad)?;
        let passage = first_recording(ctx, path, a.range.as_deref())?;
        let analysis = analyze_reading(vad.as_mut(), &passage)?;
        drop(vad);
        drop(passage);
        drop(private);
        return emit_reading_check(ctx, &analysis);
    }
    let lang = a.lang.clone().or_else(|| config.default_language.clone());

    if a.show_passage {
        let p = passage_for(lang.as_deref());
        let value = json!({ "lang": p.lang, "passage": p.text, "confirmation_phrase": p.phrase });
        return ctx.emit(value, |out| {
            writeln!(out, "{}\n\nConfirmation reading: {}", p.text, p.phrase)
        });
    }
    if a.delete_enrollment {
        let lib = ctx.library_existing(&config)?;
        let deleted = lib.write(|tx| match tx.library().self_person()? {
            Some(me) => remove_enrollment_files(&tx.library().person_dir(&me.id)),
            None => Ok(0),
        })?;
        return ctx.emit(json!({ "deleted_files": deleted }), |out| {
            writeln!(out, "Deleted {deleted} kept passage recordings.")
        });
    }

    let lib = ctx.create_library(&config)?;
    let name = me_name(&lib, &config, a.name.as_deref())?;
    let name = name.as_deref();
    if let (Some(from), Some(speaker)) = (a.from.as_deref(), a.speaker.as_deref()) {
        return voice_from_meeting(ctx, &lib, &config, from, speaker, name, &a.emails);
    }

    let (passage, phrase, device, analysis, microphone) = if a.mic {
        let seconds = check_seconds(a.seconds, MAX_PASSAGE_S)?;
        let p = passage_for(lang.as_deref());
        // Loaded before recording, so a missing model fails before the
        // user reads anything.
        let mut vad = ctx.deps.models.load_vad(&config.profile.vad)?;
        ctx.note(format!(
            "Recording 1 of 2 ({seconds:.0} s, recording now). Read this text aloud at a natural pace. If you finish before the time is up, read it again from the start. You do not need to improvise, finish it or read it perfectly; this is not a pronunciation test:\n\n{}\n",
            p.text
        ));
        let first = capture(ctx, a.device.as_deref(), seconds, "enrollment")?;
        let passage = first.samples;
        let analysis = analyze_reading(vad.as_mut(), &passage)?;
        drop(vad);
        if !analysis.ready() {
            return Err(too_few_parts(analysis.chunks.len(), &analysis.checks));
        }
        ctx.note(format!(
            "Recording 2 of 2, confirmation reading ({CONFIRM_MAX_CAPTURE_S} s, recording now). Keep the same microphone and position. Read this short text aloud at a natural pace, and when you reach the end, read it again from the start: keep reading until the recording stops. At least {CONFIRM_MIN_SPEECH_S:.0} s of detected speech is needed; silence and pauses do not count, and the speech is measured after the recording stops. It is separate from the first recording and does not count towards its time. This is not a pronunciation test:\n\n{}\n",
            p.phrase
        ));
        let second = capture(
            ctx,
            a.device.as_deref(),
            CONFIRM_MAX_CAPTURE_S as f64,
            "enrollment",
        )?;
        // Both recordings describe one microphone, or the phrase confirms
        // nothing about the reading.
        if second.device_id != first.device_id {
            return Err(Error::InvalidInput(
                "the microphone changed between the two recordings; nothing was saved, run it \
                 again with one microphone"
                    .into(),
            ));
        }
        let microphone = json!({
            "device_id": first.device_id,
            "device_name": first.device_name,
            "recorded_by": if first.by_agent { "agent" } else { "cli" },
        });
        let device = first.device_name.or(first.device_id);
        (
            passage,
            Some(second.samples),
            device,
            Some(analysis),
            Some(microphone),
        )
    } else {
        let Some(path) = passage_file.as_deref() else {
            return Err(Error::InvalidInput(
                "give --mic, --from-file, --from/--speaker, --show-passage or --delete-enrollment"
                    .into(),
            ));
        };
        let passage = first_recording(ctx, path, a.range.as_deref())?;
        let phrase = phrase_file
            .as_deref()
            .map(|p| decode_16k(&ctx.deps.decoder, p).map(|d| d.samples))
            .transpose()?;
        (passage, phrase, a.device.clone(), None, None)
    };
    let mut value = enroll_passage(
        ctx,
        &lib,
        &config,
        &EnrollInput {
            passage,
            analysis,
            phrase,
            device,
            keep: a.keep_as_enrollment,
            report_unconfirmed: a.report_unconfirmed,
            session: a.session.as_deref(),
            name,
            emails: &a.emails,
        },
    )?;
    drop(private);
    if let Some(microphone) = microphone {
        value["microphone"] = microphone;
    }
    if value["committed"] == true {
        let current = ctx.config()?;
        super::after_naming(ctx, &lib, &current, Some(&[]));
        if value["enrollment_file"]
            .as_str()
            .is_some_and(|path| !Path::new(path).is_file())
        {
            value["enrollment_file"] = Value::Null;
        }
    }
    ctx.emit(value.clone(), |out| {
        let unconfirmed = value["reason"] == "confirmation_unconfirmed";
        if value["committed"] == true {
            writeln!(
                out,
                "Enrolled your voice: {} voiceprints ({}).",
                value["voiceprints"], value["model"]
            )?;
            if value["confirmation"].is_null() {
                writeln!(out, "The optional confirmation reading was skipped.")?;
            }
        } else if unconfirmed {
            writeln!(out, "{}", unconfirmed_message(&value))?;
        } else {
            writeln!(
                out,
                "Not saved: the setup session {} was cancelled or replaced.",
                a.session.as_deref().unwrap_or("")
            )?;
        }
        for (key, label) in [
            ("checks", if unconfirmed { " (reading)" } else { "" }),
            ("confirmation_checks", " (confirmation reading)"),
        ] {
            if let Some(w) = value[key]["warnings"].as_array()
                && !w.is_empty()
            {
                writeln!(
                    out,
                    "Audio warnings{label}: {}",
                    warning_labels(w.iter().filter_map(Value::as_str))
                )?;
            }
        }
        Ok(())
    })
}

/// Why a confirmation with too little detected speech was not compared.
fn too_short_text(speech_s: f64) -> String {
    // Never display a below-minimum duration as if it reached the minimum.
    let speech_s = (speech_s * 10.0).floor() / 10.0;
    format!(
        "The confirmation reading had {speech_s:.1} s of detected speech; at least \
         {CONFIRM_MIN_SPEECH_S:.0} s is needed to compare it with the reading, and silence \
         and pauses do not count. No voice similarity was measured. Keep the same microphone \
         and position, read the short text aloud and start it again when you reach the end, \
         until the recording stops."
    )
}

/// Why a confirmation with enough speech still has no comparison: the
/// speaker model gave no embedding. No cause is claimed.
fn unusable_text(speech_s: f64) -> String {
    let speech_s = (speech_s * 10.0).floor() / 10.0;
    format!(
        "The confirmation reading had {speech_s:.1} s of detected speech (at least \
         {CONFIRM_MIN_SPEECH_S:.0} s is needed), but the speaker model could not use it, so \
         no voice similarity was measured. Record it again with the same microphone and \
         position."
    )
}

/// The human text of a `confirmation_unconfirmed` report: inconclusive,
/// not proof of a different speaker, and nothing saved.
fn unconfirmed_message(value: &Value) -> String {
    let check = &value["confirmation"];
    let speech_s = value["confirmation_checks"]["speech_s"]
        .as_f64()
        .expect("structured unconfirmed results contain detected speech");
    let mut message = match (
        value["confirmation_issue"].as_str(),
        check["score"].as_f64(),
        check["threshold"].as_f64(),
        check["method"].as_str(),
    ) {
        (Some("below_threshold"), Some(score), Some(threshold), Some(method)) => format!(
            "Not saved: the confirmation reading was inconclusive. The voice similarity of the \
             first and confirmation readings was {score:.2}, below the threshold {threshold:.2} \
             (method {method}), with {speech_s:.1} s of detected speech in the confirmation reading. This \
             does not show that they were spoken by different people, and it is not a \
             pronunciation test."
        ),
        (Some("unusable"), ..) => {
            format!(
                "Not saved: the confirmation reading could not be checked. {}",
                unusable_text(speech_s)
            )
        }
        _ => format!(
            "Not saved: the confirmation reading could not be checked. {}",
            too_short_text(speech_s)
        ),
    };
    message.push_str(
        " The first recording has enough usable speech. Read confirmation again, or run \
         again with the first recording and without --confirm-file to save it without the \
         confirmation reading.",
    );
    message
}

/// The first recording read from `--from-file` (16 kHz mono), cut to
/// `--range` when given.
fn first_recording(ctx: &Ctx<'_>, path: &Path, range: Option<&str>) -> Result<Vec<f32>> {
    let decoded = decode_16k(&ctx.deps.decoder, path)?.samples;
    Ok(match range.map(parse_range).transpose()? {
        Some((start, end)) => slice_s(&decoded, start, end).to_vec(),
        None => decoded,
    })
}

/// What enrollment uses of the first recording: the audio checks over its
/// VAD speech and the chunks that would be embedded.
struct ReadingAnalysis {
    checks: AudioChecks,
    chunks: Vec<Range>,
}

impl ReadingAnalysis {
    /// Enough chunks by duration to try enrolling. Not a promise that each
    /// chunk embeds or that a confirmation phrase will match.
    fn ready(&self) -> bool {
        self.chunks.len() >= ENROLL_MIN_CHUNKS
    }
}

/// The one analysis of the first recording behind `--check-recording`,
/// `--mic`'s early check and enrollment: VAD speech ranges, then the audio
/// checks and enrollment chunks over them.
fn analyze_reading(vad: &mut dyn Vad, samples: &[f32]) -> Result<ReadingAnalysis> {
    let speech = speech_ranges(vad, samples)?;
    Ok(ReadingAnalysis {
        checks: audio_checks(samples, &speech),
        chunks: enrollment_chunks(&speech),
    })
}

/// `--check-recording`'s result. Not ready is a result, not an error.
fn emit_reading_check(ctx: &mut Ctx<'_>, analysis: &ReadingAnalysis) -> Result<()> {
    let parts = analysis.chunks.len();
    let ready = analysis.ready();
    let value = json!({
        "ready": ready,
        "usable_parts": parts,
        "required_parts": ENROLL_MIN_CHUNKS,
        "checks": analysis.checks,
    });
    ctx.emit(value, |out| {
        if !ready {
            return writeln!(out, "{}", not_ready_message(parts, &analysis.checks));
        }
        writeln!(
            out,
            "The first recording is ready: {parts} usable speech parts (need {ENROLL_MIN_CHUNKS})."
        )?;
        if !analysis.checks.warnings.is_empty() {
            writeln!(
                out,
                "Audio warnings: {}",
                warning_labels(analysis.checks.warnings.iter().copied())
            )?;
        }
        Ok(())
    })
}

fn warning_label(warning: &str) -> &str {
    match warning {
        "noisy" => "possible background noise or missed speech (pause-based estimate)",
        _ => warning,
    }
}

fn warning_labels<'a>(warnings: impl IntoIterator<Item = &'a str>) -> String {
    let mut text = String::new();
    for warning in warnings {
        if !text.is_empty() {
            text.push_str(", ");
        }
        text.push_str(warning_label(warning));
    }
    text
}

/// Why the first recording cannot be enrolled and what to do about it,
/// with a hint for each measured level problem.
fn not_ready_message(parts: usize, checks: &AudioChecks) -> String {
    let mut message = format!(
        "The first recording has only {parts} usable speech parts of at least 3 s \
         (need {ENROLL_MIN_CHUNKS}). Record the first recording again: read the text aloud \
         for about 30 seconds without long pauses, and if you finish before 30 seconds, read \
         it again from the start. You do not need to improvise."
    );
    for &warning in &checks.warnings {
        message.push_str(match warning {
            "low_level" => {
                " Your voice was very quiet: move closer to the microphone, speak a little \
                 louder or raise the microphone's input level."
            }
            "clipping" => {
                " The sound was too loud and distorted: move back a little or lower the \
                 microphone's input level."
            }
            "noisy" => " Audio outside detected speech was relatively loud; this can reflect background noise or missed speech.",
            _ => "",
        });
    }
    if !checks.warnings.is_empty() {
        message.push_str(&format!(
            " Audio warnings: {}.",
            warning_labels(checks.warnings.iter().copied())
        ));
    }
    message
}

fn too_few_parts(parts: usize, checks: &AudioChecks) -> Error {
    Error::InvalidInput(not_ready_message(parts, checks))
}

struct EnrollInput<'a> {
    /// 16 kHz mono.
    passage: Vec<f32>,
    /// `analyze_reading` of `passage` when already run (`--mic`).
    analysis: Option<ReadingAnalysis>,
    phrase: Option<Vec<f32>>,
    device: Option<String>,
    keep: bool,
    /// Report a phrase that cannot confirm the reading instead of failing.
    report_unconfirmed: bool,
    session: Option<&'a str>,
    name: Option<&'a str>,
    emails: &'a [String],
}

/// The confirmation's audio checks without `short_speech`, which judges a
/// reading's length; the confirmation's own minimum is `CONFIRM_MIN_SPEECH_S`.
fn phrase_audio_checks(phrase: &[f32], speech: &[Range]) -> AudioChecks {
    let mut checks = audio_checks(phrase, speech);
    checks.warnings.retain(|&w| w != "short_speech");
    checks
}

/// The error for a phrase below the threshold without `--report-unconfirmed`.
fn mismatch_error(check: &PhraseCheck, reading: &AudioChecks, phrase: &AudioChecks) -> Error {
    let mut message = format!(
        "Could not confirm that the first and confirmation readings were spoken by the same person \
         (voice similarity {:.2}, threshold {:.2}, method {}). This is not a \
         pronunciation test. {} Nothing was enrolled. Record both again with the \
         same microphone and position. In the app, the confirmation reading is optional: \
         choose Start reading, speak for about 30 seconds, choose Done reading, \
         then Skip the confirmation reading.",
        check.score,
        check.threshold,
        check.method,
        if check.method == "calibrated" {
            "The threshold comes from the model's calibrated speaker-identification settings."
        } else {
            "The threshold comes from the similarity between parts of the reading."
        }
    );
    for (label, warnings) in [
        ("reading", &reading.warnings),
        ("confirmation reading", &phrase.warnings),
    ] {
        if !warnings.is_empty() {
            message.push_str(&format!(
                " Audio warnings ({label}): {}.",
                warning_labels(warnings.iter().copied())
            ));
        }
    }
    Error::InvalidInput(message)
}

/// The error for a confirmation that could not be compared (too little
/// detected speech, or no embedding) without `--report-unconfirmed`.
fn uncompared_error(text: String, phrase: &AudioChecks) -> Error {
    let mut message = format!(
        "{text} Nothing was enrolled. In the app, the confirmation reading is optional: choose \
         Start reading, speak for about 30 seconds, choose Done reading, then Skip the \
         confirmation reading."
    );
    if !phrase.warnings.is_empty() {
        message.push_str(&format!(
            " Audio warnings (confirmation reading): {}.",
            warning_labels(phrase.warnings.iter().copied())
        ));
    }
    Error::InvalidInput(message)
}

/// Why a phrase did not confirm the reading under `--report-unconfirmed`.
struct Unconfirmed {
    /// `too_short`, `unusable` or `below_threshold`.
    issue: &'static str,
    /// The comparison; `None` when none was made (`too_short`, `unusable`).
    check: Option<PhraseCheck>,
    phrase_checks: AudioChecks,
}

/// Embeds the passage outside the lock, then commits under it (see the
/// module docs). Returns the command's JSON result.
fn enroll_passage(
    ctx: &mut Ctx<'_>,
    lib: &Library,
    config: &Config,
    input: &EnrollInput<'_>,
) -> Result<Value> {
    let mut models: ModelSet = ctx.deps.models.load(&config.profile)?;
    let model = models.embedder.model_id().to_owned();
    let computed;
    let analysis = match &input.analysis {
        Some(analysis) => analysis,
        None => {
            computed = analyze_reading(models.vad.as_mut(), &input.passage)?;
            &computed
        }
    };
    let checks = &analysis.checks;
    if !analysis.ready() {
        return Err(too_few_parts(analysis.chunks.len(), checks));
    }
    let samples = embed_sample_ranges(&input.passage, models.embedder.as_ref(), &analysis.chunks)?;
    if samples.len() < ENROLL_MIN_CHUNKS {
        return Err(too_few_parts(samples.len(), checks));
    }
    let mut unconfirmed = None;
    let confirmation: Option<PhraseCheck> = match &input.phrase {
        Some(phrase) => {
            let phrase_speech = speech_ranges(models.vad.as_mut(), phrase)?;
            // Measured once, before the speaker model sees the phrase: too
            // little detected speech gets no comparison and no score.
            let phrase_checks = phrase_audio_checks(phrase, &phrase_speech);
            let vector = if phrase_checks.speech_s < CONFIRM_MIN_SPEECH_S {
                Err("too_short")
            } else {
                embed_phrase(phrase, &phrase_speech, models.embedder.as_ref())?.ok_or("unusable")
            };
            match vector {
                Err(issue) if input.report_unconfirmed => {
                    unconfirmed = Some(Unconfirmed {
                        issue,
                        check: None,
                        phrase_checks,
                    });
                    None
                }
                Err(issue) => {
                    let text = if issue == "too_short" {
                        too_short_text(phrase_checks.speech_s)
                    } else {
                        unusable_text(phrase_checks.speech_s)
                    };
                    return Err(uncompared_error(text, &phrase_checks));
                }
                Ok(vector) => {
                    let vectors: Vec<Vec<f32>> = samples.iter().map(|s| s.vector.clone()).collect();
                    let suggest = config.thresholds(&model).and_then(|t| t.suggest);
                    let check = phrase_check(&vector, &vectors, suggest);
                    if check.passed {
                        Some(check)
                    } else {
                        if !input.report_unconfirmed {
                            return Err(mismatch_error(&check, checks, &phrase_checks));
                        }
                        unconfirmed = Some(Unconfirmed {
                            issue: "below_threshold",
                            check: Some(check),
                            phrase_checks,
                        });
                        None
                    }
                }
            }
        }
        None => None,
    };
    drop(models);

    if let Some(u) = unconfirmed {
        // Read-only: a session cancelled meanwhile reports its cancellation,
        // so a superseded run never offers to save its reading.
        if let Some(id) = input.session
            && Config::load(ctx.data_dir())?.setup.voice_session.as_deref() != Some(id)
        {
            return Ok(json!({
                "committed": false,
                "reason": "session_superseded",
                "model": model,
                "voiceprints": 0,
                "enrollment_file": null,
                "checks": checks,
                "confirmation": u.check,
            }));
        }
        return Ok(json!({
            "committed": false,
            "reason": "confirmation_unconfirmed",
            "confirmation_issue": u.issue,
            "confirmation": u.check,
            "checks": checks,
            "confirmation_checks": u.phrase_checks,
            "confirmation_min_speech_s": CONFIRM_MIN_SPEECH_S,
            "model": model,
            "voiceprints": 0,
            "enrollment_file": null,
        }));
    }

    let wav = wav_16k_bytes(&input.passage)?;
    let sha = schema::sha256_hex(&wav);
    let data_dir = ctx.data_dir().to_path_buf();
    let committed = ops::with_index(lib, |tx, index| {
        // Check-and-commit under the lock the app takes to change the session.
        let current = Config::load(&data_dir)?;
        if let Some(id) = input.session
            && current.setup.voice_session.as_deref() != Some(id)
        {
            return Ok(None);
        }
        let (me, _) = upsert_me(tx, index, input.name, input.emails)?;
        let person_dir = tx.library().person_dir(&me.id);
        // Redo: earlier passage enrollments and kept passages go.
        let mut vps = tx.library().load_voiceprints(&me.id)?;
        vps.embeddings.retain(|e| {
            !(e.source.kind == VoiceprintKind::Enrollment && e.source.meeting_id.is_none())
        });
        remove_enrollment_files(&person_dir)?;
        let kept =
            if input.keep && !matches!(current.retention, Retention::DeleteImmediately { .. }) {
                let dir = person_dir.join(ENROLLMENT_DIR);
                fs::create_dir_all(&dir).map_err(|e| Error::io_at(&dir, e))?;
                let path = dir.join(format!("{sha}.wav"));
                fsutil::write_atomic(&path, &wav)?;
                Some(path)
            } else {
                None
            };
        vps.upsert(
            samples
                .iter()
                .map(|s| enrollment_voiceprint(&me.id, &model, &sha, s, input.device.as_deref()))
                .collect(),
        );
        tx.save_voiceprints(&vps)?;
        index.upsert_person(tx, &me)?;
        // A fresh read under the config lock: settings saved meanwhile stay.
        Config::update(&data_dir, |config| {
            remember_me(config, input.name, input.emails);
            mark(config, "voice");
            Ok(())
        })?;
        Ok(Some((me, kept)))
    })?;
    Ok(match committed {
        Some((me, kept)) => json!({
            "committed": true,
            "person_id": me.id,
            "model": model,
            "voiceprints": samples.len(),
            "enrollment_file": kept,
            "checks": checks,
            "confirmation": confirmation,
        }),
        None => json!({
            "committed": false,
            "reason": "session_superseded",
            "model": model,
            "voiceprints": 0,
            "enrollment_file": null,
            "checks": checks,
            "confirmation": confirmation,
        }),
    })
}

fn speech_ranges(vad: &mut dyn Vad, samples: &[f32]) -> Result<Vec<Range>> {
    Ok(vad
        .segments(samples)?
        .iter()
        .map(|s| (s.start_s, s.end_s))
        .collect())
}

/// An enrollment voiceprint keyed by the passage WAV's SHA-256.
fn enrollment_voiceprint(
    person: &PersonId,
    model: &str,
    sha: &str,
    s: &ConfirmationSample,
    device: Option<&str>,
) -> Voiceprint {
    let (start, end) = (f64::from(s.start), f64::from(s.end));
    let mut v = s.vector.clone();
    identity::l2_normalize(&mut v);
    Voiceprint {
        id: schema::voiceprint_id(
            person,
            model,
            VoiceprintOrigin::Recording { sha256: sha },
            start,
            end,
            VoiceprintKind::Enrollment,
        ),
        model: model.to_owned(),
        dim: v.len(),
        vector_b64: schema::encode_vector(&v),
        source: VoiceprintSource {
            kind: VoiceprintKind::Enrollment,
            meeting_id: None,
            sha256: Some(sha.to_owned()),
            start,
            end,
            ranges: Vec::new(),
            speaker_id: None,
            speaker_namespace: String::new(),
        },
        device: device.map(str::to_owned),
        strength: Strength::Confirmed,
        created_at: schema::now(),
    }
}

/// Deletes `<person_dir>/enrollment/` (kept passages); returns how many
/// files were removed. Call under the library lock.
pub(crate) fn remove_enrollment_files(person_dir: &Path) -> Result<usize> {
    let dir = person_dir.join(ENROLLMENT_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Error::io_at(&dir, e)),
    };
    let mut removed = 0;
    for entry in entries {
        let path = entry.map_err(|e| Error::io_at(&dir, e))?.path();
        if path.is_file() {
            fs::remove_file(&path).map_err(|e| Error::io_at(&path, e))?;
            removed += 1;
        }
    }
    let _ = fs::remove_dir(&dir);
    Ok(removed)
}

/// `--from <meeting> --speaker S#`: names that speaker as Me (confirmation
/// samples from the recording, else the centroid), like `speakers name`.
fn voice_from_meeting(
    ctx: &mut Ctx<'_>,
    lib: &Library,
    config: &Config,
    meeting: &str,
    speaker: &str,
    name: Option<&str>,
    emails: &[String],
) -> Result<()> {
    let mid = meeting_id(meeting)?;
    let sid = speaker_id(speaker)?;
    let model = config.profile.speaker_embedding.clone();
    let samples = match ctx.deps.models.load_embedder(&model) {
        Ok(embedder) => broscribe_pipeline::confirmation_samples(
            lib,
            &ctx.deps.decoder,
            embedder.as_ref(),
            &mid,
            sid,
        )?,
        Err(e @ Error::ModelMissing(_)) => {
            ctx.note(format!("note: {e}; using the speaker's stored centroid"));
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    let data_dir = ctx.data_dir().to_path_buf();
    let (me, outcome): (Person, ops::NameOutcome) = ops::with_index(lib, |tx, index| {
        let (me, _) = upsert_me(tx, index, name, emails)?;
        let outcome = ops::name_speaker(tx, index, &mid, sid, me.id.as_str(), samples, &model)?;
        Config::update(&data_dir, |config| {
            remember_me(config, name, emails);
            mark(config, "voice");
            Ok(())
        })?;
        Ok((me, outcome))
    })?;
    super::after_naming(ctx, lib, config, Some(std::slice::from_ref(&mid)));
    let agent_notified = super::kick_recheck(ctx);
    let value = json!({
        "committed": true,
        "person_id": me.id,
        "meeting_id": mid,
        "speaker_id": sid,
        "model": model,
        "voiceprint_added": outcome.voiceprint_added,
        "learning": outcome.learning,
        "agent_notified": agent_notified,
    });
    ctx.emit(value, |out| {
        writeln!(out, "Named {mid} {sid} as you ({}).", me.id)?;
        if !outcome.voiceprint_added {
            writeln!(
                out,
                "No voiceprint added: the speaker has no audio or centroid."
            )?;
        }
        writeln!(
            out,
            "Checking other speakers: this meeting first, then the rest of the library."
        )
    })
}

// ---- setup try ----

/// `[hh:mm:ss] S#: text` lines.
fn transcript_lines(
    segments: &[Segment],
    names: &std::collections::BTreeMap<broscribe_core::SpeakerId, String>,
) -> String {
    let mut text = String::new();
    for s in segments {
        let who = s.speaker.map_or_else(
            || "?".to_owned(),
            |sp| names.get(&sp).cloned().unwrap_or_else(|| sp.to_string()),
        );
        text.push_str(&format!(
            "[{}] {who}: {}\n",
            broscribe_core::render::format_hms(s.start),
            s.text
        ));
    }
    text
}

/// Unsaved Try-it recognition, as an import of this audio would run it:
/// hotwords from known people and the configured terms, and the requested
/// language over the whole recording. Nothing is stored.
fn recognise_unsaved(
    models: &mut ModelSet,
    samples: &[f32],
    lang: Option<&str>,
    lib: Option<&Library>,
    hotwords: &[String],
) -> Result<broscribe_pipeline::Analysis> {
    if models.asr.accepts_hotwords() {
        let terms = broscribe_pipeline::reprocess::library_hotwords(lib, hotwords)?;
        models.asr.set_hotwords(&terms)?;
    }
    let languages: Vec<LanguageSpan> = lang
        .map(|lang| LanguageSpan {
            lang: lang.to_owned(),
            from_s: 0.0,
            to_s: samples.len() as f64 / f64::from(TARGET_RATE),
        })
        .into_iter()
        .collect();
    analyze_with_languages(samples, models, AnalyzeOptions::default(), &languages)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum TryMe {
    #[default]
    None,
    Suggested,
    Named,
}

impl TryMe {
    fn observe(
        &mut self,
        me: Option<&broscribe_core::PersonId>,
        person: Option<&broscribe_core::PersonId>,
        assignment: &schema::Assignment,
    ) {
        if me.is_none() || me != person {
            return;
        }
        let result = match assignment {
            schema::Assignment::Confirmed | schema::Assignment::Auto => Self::Named,
            schema::Assignment::Suggested => Self::Suggested,
            schema::Assignment::None => Self::None,
        };
        *self = (*self).max(result);
    }
}

pub(super) fn setup_try(ctx: &mut Ctx<'_>, a: TryArgs) -> Result<()> {
    // Example clips are checks of the installed model, independent of onboarding.
    let finish_setup = a.session.is_none() && !a.model_example;
    let config = ctx.config()?;
    let example = if a.model_example {
        let store = broscribe_models::ModelStore::new(ctx.data_dir());
        Some(
            broscribe_core::fsutil::find_sample_wav(&store.model_dir(&config.profile.asr)?)
                .ok_or_else(|| {
                    Error::NotFound(format!("example clip for {}", config.profile.asr))
                })?,
        )
    } else {
        None
    };
    let private = PrivateDir::new(
        &ctx.paths.cache_dir,
        a.session.as_deref(),
        ctx.deps.process.as_ref(),
    )?;
    let input = a.from_file.as_deref().or(example.as_deref());
    let copied = input
        .map(|f| private.copy_input(f, "try-input"))
        .transpose()?;
    let (path, title) = match copied {
        Some(p) => {
            let title = input
                .and_then(Path::file_stem)
                .and_then(|name| name.to_str())
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or("Setup test recording");
            (p, Some(title.to_owned()))
        }
        None => {
            let seconds = check_seconds(a.seconds, MAX_TRY_S)?;
            ctx.note(format!(
                "Say a few sentences ({seconds:.0} s, recording now)…"
            ));
            let captured = capture(ctx, a.device.as_deref(), seconds, "try_it")?;
            let p = private.path().join("setup-test.wav");
            fsutil::write_atomic(&p, &wav_16k_bytes(&captured.samples)?)?;
            (p, Some("Setup test recording".to_owned()))
        }
    };
    let deps = ctx.deps;
    let mut models = deps.models.load(&config.profile)?;
    let mut names = std::collections::BTreeMap::new();
    let mut me_result = TryMe::None;
    let (segments, duration_s, meeting_id) = if a.save_as_meeting {
        let lib = ctx.create_library(&config)?;
        let env = Env {
            lib: &lib,
            config: &config,
            decoders: &deps.decoder,
            probe: deps.process.as_ref(),
            execution: &deps.execution,
            progress: Some(ctx.pipeline_progress()),
        };
        // Copy: the temporary file goes at exit.
        let opts = ImportOptions {
            handling: Some(Handling::Copy),
            language: a.lang.clone(),
            title,
            started_at: None,
            participants: Vec::new(),
            auto_language: false,
            setup_session: a
                .session
                .as_ref()
                .map(|session| (ctx.data_dir().to_owned(), session.clone())),
        };
        let mut completed = Vec::new();
        let result = if ctx.request_library.is_some() {
            broscribe_pipeline::import::import_media_without_moves(&env, &mut models, &path, &opts)
        } else {
            import_file_with_recoveries(&env, &mut models, &path, &opts, &mut completed)
        };
        super::import::follow_up_completed_moves(ctx, &lib, &config, &completed);
        let outcome = match result {
            Err(Error::InvalidInput(message)) if message == "setup session superseded" => {
                return ctx.emit(
                    json!({"committed":false,"reason":"session_superseded","meeting_id":null,"me":TryMe::None}),
                    |out| {
                        writeln!(
                            out,
                            "Not saved: the setup session was cancelled or replaced."
                        )
                    },
                );
            }
            other => other?,
        };
        let m = lib.load_meeting(&outcome.meeting_id)?;
        let people = lib.people_lookup()?;
        let me = lib.self_person()?.map(|person| person.id);
        for speaker in &m.speakers {
            let person = speaker.named_person().or_else(|| {
                speaker
                    .suggestion
                    .as_ref()
                    .map(|suggestion| &suggestion.person_id)
            });
            me_result.observe(me.as_ref(), person, &speaker.assignment);
            if let Some(person) = speaker.named_person()
                && let Some(name) = people.display_name(person)
            {
                names.insert(speaker.id, name.to_owned());
            }
        }
        super::after_identity(ctx, &lib, &config, Some(std::slice::from_ref(&m.id)));
        (m.segments, m.duration_s, Some(m.id))
    } else {
        let audio = decode_16k(&deps.decoder, &path)?;
        let lib = match ctx.library_existing(&config) {
            Ok(lib) => Some(lib),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let analysis = recognise_unsaved(
            &mut models,
            &audio.samples,
            a.lang.as_deref(),
            lib.as_ref(),
            &config.hotwords,
        )?;
        if let Some(lib) = &lib {
            let model = models.embedder.model_id();
            let pool = ops::load_person_samples(lib, model)?;
            let people = lib.people_lookup()?;
            let me = lib.self_person()?.map(|person| person.id);
            for speaker in &analysis.speakers {
                if let Some(centroid) = &speaker.centroid {
                    // Display uses the same promotion gate as the resolver
                    // that publishes names.
                    let resolution = identity::resolve_voice(
                        &centroid.vector()?,
                        pool.all(),
                        &[],
                        config.thresholds(model),
                        None,
                        None,
                    );
                    me_result.observe(
                        me.as_ref(),
                        resolution.person_id.as_ref(),
                        &resolution.assignment,
                    );
                    if resolution.assignment != schema::Assignment::None
                        && let Some(person) = resolution.person_id
                        && let Some(name) = people.display_name(&person)
                    {
                        let suffix = if resolution.assignment == schema::Assignment::Suggested {
                            "?"
                        } else {
                            ""
                        };
                        names.insert(speaker.id, format!("{name}{suffix}"));
                    }
                }
            }
        }
        (analysis.segments, analysis.duration_s, None)
    };
    drop(private);
    let transcript = transcript_lines(&segments, &names);
    if finish_setup && !segments.is_empty() {
        Config::update(ctx.data_dir(), |config| {
            super::setup::mark(config, "try");
            Ok(())
        })?;
    }
    let value = json!({
        "transcript": transcript,
        "recognised_me": me_result == TryMe::Named,
        "me": me_result,
        "duration_s": duration_s,
        "segments": segments.len(),
        "meeting_id": meeting_id,
    });
    ctx.emit(value, |out| {
        if transcript.is_empty() {
            writeln!(out, "No speech recognised.")?;
        } else {
            write!(out, "{transcript}")?;
        }
        if let Some(id) = &meeting_id {
            writeln!(out, "Saved as {id}.")?;
        }
        Ok(())
    })
}

pub(super) fn mic_test(ctx: &mut Ctx<'_>) -> Result<()> {
    let captured = match capture(ctx, None, 2.0, "audio_check") {
        Ok(captured) => Some(captured),
        Err(Error::InvalidInput(message)) if message == "no audio was captured" => None,
        Err(e) => return Err(e),
    };
    let samples = captured.as_ref().map_or(&[][..], |c| c.samples.as_slice());
    let (status, peak) = broscribe_platform::mic_check(samples);
    ctx.emit(
        json!({"status": status, "peak_dbfs": peak, "seconds": 2,
            "device_id": captured.as_ref().and_then(|c| c.device_id.as_deref()),
            "device_name": captured.as_ref().and_then(|c| c.device_name.as_deref()),
        }),
        |out| writeln!(out, "Microphone check: {}.", status.replace('_', " ")),
    )
}

pub(super) fn voice_status(ctx: &mut Ctx<'_>) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let files = broscribe_core::ops::enrollment_files(&lib)?;
    let bytes = files.iter().map(|(_, bytes)| bytes).sum::<u64>();
    let count = files.len();
    ctx.emit(json!({"readings": count, "bytes": bytes}), |out| {
        writeln!(out, "{count} kept voice reading(s), {bytes} bytes.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passages_fall_back_by_language() {
        assert_eq!(passage_for(Some("pt-PT")).lang, "pt-PT");
        assert_eq!(passage_for(Some("pt")).lang, "pt-BR");
        assert_eq!(passage_for(Some("PT-br")).lang, "pt-BR");
        assert_eq!(passage_for(Some("en-GB")).lang, "en");
        assert_eq!(passage_for(Some("de")).lang, "en");
        assert_eq!(passage_for(None).lang, "en");
        for (tag, _) in PASSAGES {
            let p = passage_for(Some(tag));
            let words = p.text.split_whitespace().count();
            assert!((55..=65).contains(&words), "{tag}: {words} words");
            // A simple confirmation text that is read and repeated until the
            // capture stops, long enough for the minimum detected speech.
            let phrase_words = p.phrase.split_whitespace().count();
            assert!(
                (30..=35).contains(&phrase_words),
                "{tag}: {phrase_words} phrase words"
            );
        }
    }

    #[test]
    fn resampling_keeps_duration() {
        let input = vec![0.5f32; 48_000];
        let out = resample(&input, 48_000, 16_000);
        assert_eq!(out.len(), 16_000);
        assert!(out.iter().all(|x| (x - 0.5).abs() < 1e-6));
        assert_eq!(resample(&input[..8_000], 8_000, 16_000).len(), 16_000);
    }

    #[test]
    fn reading_analysis_counts_parts_by_speech_duration() {
        use broscribe_models::fakes::{EnergyVad, sine};
        let analyze =
            |samples: &[f32]| analyze_reading(&mut EnergyVad::default(), samples).unwrap();
        let spoken = |seconds: f64, amplitude: f32| {
            let mut a = vec![0.0f32; 8_000];
            a.extend(sine(200.0, seconds, amplitude));
            a.extend(vec![0.0f32; 8_000]);
            a
        };
        let short = analyze(&spoken(18.0, 0.5));
        assert_eq!(short.chunks.len(), 3);
        assert!(!short.ready());
        let enough = analyze(&spoken(25.0, 0.5));
        assert_eq!(enough.chunks.len(), 5);
        assert!(enough.ready());
        assert!(enough.checks.warnings.is_empty(), "{:?}", enough.checks);
        // Silence, and a voice below the VAD's level: nothing usable, and the
        // level is what the user is told about.
        for samples in [vec![0.0f32; 25 * 16_000], spoken(25.0, 0.01)] {
            let quiet = analyze(&samples);
            assert!(quiet.chunks.is_empty() && !quiet.ready());
            assert!(quiet.checks.warnings.contains(&"low_level"));
            let message = not_ready_message(quiet.chunks.len(), &quiet.checks);
            assert!(message.contains("low_level"), "{message}");
        }
    }

    #[test]
    fn private_dir_cleans_up_only_its_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("in.wav");
        fs::write(&file, b"x").unwrap();
        let probe = broscribe_platform::fakes::FakeProcessProbe::new(41);
        let dir = PrivateDir::new(tmp.path(), Some("session-1"), &probe).unwrap();
        let copy = dir.copy_input(&file, "passage-input").unwrap();
        assert!(copy.starts_with(tmp.path().join("setup-capture/session-1")));
        assert_eq!(copy.file_name().unwrap(), "passage-input.wav");
        assert_eq!(fs::read(&copy).unwrap(), b"x");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(dir.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
            assert_eq!(
                fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let root = dir.path().to_path_buf();
        drop(dir);
        assert!(!root.exists());
        assert_eq!(fs::read(file).unwrap(), b"x");
    }

    #[test]
    fn cli_runs_capture_outside_the_app_folder_and_sweep_only_dead_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let probe = broscribe_platform::fakes::FakeProcessProbe::new(41);
        let cli = tmp.path().join(CLI_CAPTURE_DIR);
        // Another live CLI run and a killed one.
        probe.set_alive(42, true);
        fs::create_dir_all(cli.join("42/broscribe-voice-x")).unwrap();
        fs::create_dir_all(cli.join("43/broscribe-voice-y")).unwrap();
        let dir = PrivateDir::new(tmp.path(), None, &probe).unwrap();
        assert!(dir.path().starts_with(cli.join("41")));
        assert!(!dir.path().starts_with(tmp.path().join("setup-capture")));
        assert!(cli.join("42/broscribe-voice-x").is_dir(), "live run kept");
        assert!(!cli.join("43").exists(), "dead run removed");
        drop(dir);
        assert!(!cli.join("41").exists(), "the run folder goes at exit");
        assert!(cli.join("42").is_dir());
    }

    #[test]
    fn unsaved_try_uses_the_requested_language_and_hotwords() {
        use std::sync::{Arc, Mutex};
        type Log = Arc<Mutex<(Vec<String>, Vec<(Option<String>, Vec<String>)>)>>;
        struct CheckingAsr(Log);
        impl broscribe_models::Asr for CheckingAsr {
            fn transcribe(&self, _: &[f32]) -> Result<broscribe_models::AsrOutput> {
                panic!("language-aware recognition required")
            }
            fn transcribe_with(
                &self,
                _: &[f32],
                lang: Option<&str>,
            ) -> Result<broscribe_models::AsrOutput> {
                let mut log = self.0.lock().unwrap();
                let terms = log.0.clone();
                log.1.push((lang.map(str::to_owned), terms));
                Ok(broscribe_models::AsrOutput {
                    text: "synthetic".into(),
                    tokens: Vec::new(),
                })
            }
            fn accepts_hotwords(&self) -> bool {
                true
            }
            fn set_hotwords(&mut self, terms: &[String]) -> Result<()> {
                self.0.lock().unwrap().0 = terms.to_vec();
                Ok(())
            }
        }
        use broscribe_models::ModelProvider as _;
        let mut models = broscribe_models::fakes::FakeModelProvider::default()
            .load(&broscribe_core::config::Profile::default())
            .unwrap();
        let log = Log::default();
        models.asr = Box::new(CheckingAsr(log.clone()));
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path().join("library")).unwrap();
        lib.write(|tx| tx.save_person(&Person::new(lib.new_person_id(), "Ana Synthetic")))
            .unwrap();
        let samples: Vec<f32> = (0..32_000)
            .map(|i| 0.5 * (i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin())
            .collect();
        recognise_unsaved(
            &mut models,
            &samples,
            Some("pt-BR"),
            Some(&lib),
            &["TermOne".to_owned()],
        )
        .unwrap();
        let calls = std::mem::take(&mut log.lock().unwrap().1);
        assert!(!calls.is_empty());
        for (lang, terms) in calls {
            assert_eq!(lang.as_deref(), Some("pt-BR"));
            assert_eq!(terms, ["Ana Synthetic", "TermOne"]);
        }
    }
}
