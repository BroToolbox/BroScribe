//! People (design §4 "People", plan §4.3): the list of people in the library
//! with their emails, meetings, talk time and voice samples, and a detail
//! pane to edit name / aliases / emails / notes (through `broscribe people
//! edit --spec-stdin`, the CLI parity endpoint: values on stdin, fenced by
//! the revision read when the editor opened, so a reload never moves the
//! editor's baseline and a field changed elsewhere is kept or the save is
//! refused, never overwritten; a saved edit offers a short Undo through
//! `people undo-edit`), play voice samples for their source range and forget
//! a voice or a person after a confirmation (no Undo: design §4 People).
//! Cached directory photos show in the list and detail; the detail searches
//! the signed-in accounts' directories (`broscribe people lookup`), links a
//! result's email to the person and fetches their photo (`broscribe people
//! photo`, exact email only), all on the background executor and fenced by
//! person existence and account generation (design §10).
//!
//! This module also holds the bounded sample playback shared with the review
//! queue ([`SamplePlayback`], plan §8 items 12, 15, 16): the sample's file is
//! located on the background executor within [`SAMPLE_START_DEADLINE`];
//! natively playable files are prepared by the
//! [`AudioPlayer`](broscribe_platform::AudioPlayer) (its probe runs on the
//! background executor, bounded and cancellable) and then played, other
//! formats play from the playback cache made by the CLI worker (a
//! `playback-cache` job is queued when the cache does not exist yet). The
//! player is polled at 4 Hz only while a sample loads or plays; the clip's
//! end timer starts once its audio plays. Stopping, starting another sample
//! or releasing the owning view cancels a start in flight and stops the
//! sample; a superseded start ends silently.
//!
//! Mounting: `cx.new(|cx| PeopleView::new(state.clone(), window, cx))` once
//! per window, rendered for [`Route::People`](crate::state::Route::People).
//! Playback needs [`AppState::set_player`] to have installed a player;
//! without one the sample buttons report that playback is unavailable.

use crate::ui::kit::{self, StatusKind, button};
use crate::views::person_picker::{self, PeopleIndex, speaker_meetings};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use broscribe_core::index::{Index, MeetingHit, PersonSummary, SearchQuery};
use broscribe_core::ops::{
    self, ForgetOutcome, PersonEditSpec, PersonField, PersonRevision, PersonState,
};
use broscribe_core::schema::{Person, Strength, VoiceprintKind};
use broscribe_core::{Error, Library, MeetingId, PersonId, Result};
use broscribe_integrations::IntegrationId;
use broscribe_platform::PlayerState;
use broscribe_platform::player::{
    PLAYBACK_CANCELLED, PLAYBACK_NEEDS_CACHE, PLAYBACK_SUPERSEDED, PlaybackCancel,
    PlaybackPreparation, PreparedPlayback,
};
use chrono::{DateTime, FixedOffset};
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::{Input, InputEvent, InputState, Textarea, TextareaState, Undo};
use gpui::component::kbd::Kbd;
use gpui::component::menu::PopupMenuItem;
use gpui::component::popover::Popover;
use gpui::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, Image, ImageFormat,
    ImageSource, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _,
    PathPromptOptions, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div,
};

use crate::jobs::{ChildKind, CliError, CliRunner, core_user_text, failure_text, sentence};
use crate::state::{AppState, Route};
use crate::ui::format::{count, length, spoken_length};
use crate::ui::kit::fact_row;
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::ui::{heading, text_line, title};
use crate::views::meeting::{PLAYBACK_TICK, enqueue_playback_cache, playback_cache_path};
use crate::views::settings::muted;
use crate::views::speaker_review::UNDO_WINDOW;

/// Sub-folder of `people/<id>/` holding the kept enrollment passage.
pub use broscribe_core::ops::ENROLLMENT_DIR;

const LIST_WIDTH: f32 = 18.75;
const PERSON_MEETINGS_SHOWN: usize = 10;
const TYPE_SELECT_WINDOW: Duration = Duration::from_millis(500);

gpui::actions!(
    people,
    [
        ForgetSelectedPerson,
        SavePerson,
        CancelPersonEdit,
        KeepEditingPerson
    ]
);

// ---- shared sample playback ----

pub use broscribe_core::ops::{Clip, ClipFiles, ClipSource, PlaybackCache, clip_of, locate_clip};

/// What to do to play a clip.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipPlan {
    Play {
        path: PathBuf,
        start_s: f64,
        end_s: f64,
    },
    /// Queue a `playback-cache` job, then play the cache.
    PrepareCache {
        meeting_id: MeetingId,
        sha256: String,
    },
    Unavailable(String),
}

/// Decides how to play `clip` given its files and what the player plays
/// natively. Cache positions are recording-relative, like the original's.
pub fn plan_clip(files: ClipFiles, clip: &Clip, natively: impl Fn(&Path) -> bool) -> ClipPlan {
    let play = |path: PathBuf| ClipPlan::Play {
        path,
        start_s: clip.start_s,
        end_s: clip.end_s,
    };
    match files {
        ClipFiles::Unavailable(message) => ClipPlan::Unavailable(message),
        ClipFiles::Found {
            audio,
            cache,
            mix_required,
        } => {
            if !mix_required && natively(&audio) {
                return play(audio);
            }
            match cache {
                Some(c) if c.exists => play(c.path),
                Some(c) => ClipPlan::PrepareCache {
                    meeting_id: c.meeting_id,
                    sha256: c.sha256,
                },
                None => ClipPlan::Unavailable("Playback is not supported for this format.".into()),
            }
        }
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "Keep common playback preparation inline instead of allocating on every sample start."
)]
enum StartOutcome {
    /// The player issued a preparation of the file for this view: it runs
    /// off the UI loop, then plays the clip.
    Preparing {
        preparation: PlaybackPreparation,
        pending: PendingStart,
    },
    /// The recording needs its playable cache first.
    MakeCache {
        meeting_id: MeetingId,
        sha256: String,
    },
    Unavailable(String),
}

/// A start whose file the player is preparing.
struct PendingStart {
    /// The player's owner token this view holds.
    owner: u64,
    start_s: f64,
    end_s: f64,
    /// The clip and its files, planned again without native playback when
    /// the player answers that the file needs the playable cache; `None`
    /// once that was done (the cache itself is not retried).
    retry: Option<(ClipFiles, Clip)>,
}

/// Takes the player and asks it to prepare `clip`'s file (UI loop: the
/// player does no file I/O here).
fn begin_clip(state: &mut AppState, files: ClipFiles, clip: &Clip) -> StartOutcome {
    let Some(player) = state.player_mut() else {
        return StartOutcome::Unavailable("Audio playback is not available.".into());
    };
    match plan_clip(files.clone(), clip, |path| player.can_play_natively(path)) {
        ClipPlan::Play {
            path,
            start_s,
            end_s,
        } => {
            let owner = state.claim_player();
            match state
                .player_owned(owner)
                .expect("player was checked")
                .prepare(&path)
            {
                Ok(preparation) => StartOutcome::Preparing {
                    preparation,
                    pending: PendingStart {
                        owner,
                        start_s,
                        end_s,
                        retry: Some((files, clip.clone())),
                    },
                },
                Err(e) => StartOutcome::Unavailable(sample_failure(&core_user_text(&e))),
            }
        }
        ClipPlan::PrepareCache { meeting_id, sha256 } => {
            StartOutcome::MakeCache { meeting_id, sha256 }
        }
        ClipPlan::Unavailable(message) => StartOutcome::Unavailable(message),
    }
}

/// "Couldn't load people. {Cause}. Try again." (WRT-08)
pub fn people_load_failure(cause: &str) -> String {
    failure_text("Couldn't load people.", cause, "Try again.")
}

/// "Couldn't play the sample. {Cause}."
fn sample_failure(cause: &str) -> String {
    failure_text("Couldn't play the sample.", cause, "")
}

/// How long finding a sample's file may take before the start is abandoned.
pub const SAMPLE_START_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

/// One view's sample playback: which sample is active (by a view-chosen
/// key), whether it plays, and a status line. Starting a sample stops the
/// previous one; results of a superseded start are discarded (generation).
/// Releasing the view that owns it stops its sample (the player is shared
/// by every view, so a released view never leaves it playing).
#[derive(Default)]
pub struct SamplePlayback {
    active: Option<SharedString>,
    playing: bool,
    status: Option<String>,
    generation: u64,
    tick: Option<Task<()>>,
    cache_wait: Option<Task<()>>,
    owner: Option<u64>,
    end_timer: Option<Task<()>>,
    /// End of the current clip, recording-relative.
    end_s: f64,
    /// Cancels the current start: finding the file, then the player's
    /// preparation of it.
    cancel: Option<PlaybackCancel>,
    /// Stops the sample when the owning view is released.
    release: Option<Subscription>,
}

impl SamplePlayback {
    /// Key of the sample playing, loading or reporting a status.
    pub fn active(&self) -> Option<&SharedString> {
        self.active.as_ref()
    }

    pub fn is_playing(&self, key: &str) -> bool {
        self.playing && self.active.as_ref().is_some_and(|k| k.as_ref() == key)
    }

    /// Status of `key` ("Loading…", why it cannot play), if any.
    pub fn status_of(&self, key: &str) -> Option<&str> {
        match &self.active {
            Some(k) if k.as_ref() == key => self.status.as_deref(),
            _ => None,
        }
    }

    /// Plays `clip`, or stops it when `key` is already playing.
    /// `field` returns this playback inside the view `V`.
    pub fn toggle<V: 'static>(
        &mut self,
        key: SharedString,
        clip: Clip,
        state: &Entity<AppState>,
        cx: &mut Context<V>,
        field: fn(&mut V) -> &mut SamplePlayback,
    ) {
        if self.is_playing(&key)
            || self
                .status_of(&key)
                .is_some_and(|status| matches!(status, LOADING | PREPARING_CACHE))
        {
            self.stop(state, cx);
            cx.notify();
        } else {
            self.play(key, clip, state, cx, field);
        }
    }

    /// Plays `clip` (bounded to its range) after locating its file on the
    /// background executor.
    pub fn play<V: 'static>(
        &mut self,
        key: SharedString,
        clip: Clip,
        state: &Entity<AppState>,
        cx: &mut Context<V>,
        field: fn(&mut V) -> &mut SamplePlayback,
    ) {
        self.stop(state, cx);
        if self.release.is_none() {
            let state = state.clone();
            self.release = Some(cx.on_release(move |view, cx| field(view).stop(&state, cx)));
        }
        let generation = self.generation;
        self.active = Some(key);
        self.status = Some(LOADING.into());
        let cancel = PlaybackCancel::default();
        self.cancel = Some(cancel.clone());
        cx.notify();
        let (lib, cache_dir) = {
            let s = state.read(cx);
            (s.library().clone(), s.paths().cache_dir.clone())
        };
        let source = clip.source.clone();
        let task = cx.background_executor().spawn(async move {
            if cancel.is_cancelled() {
                return None;
            }
            Some(locate_clip(&lib, &cache_dir, &source))
        });
        let deadline = cx.background_executor().timer(SAMPLE_START_DEADLINE);
        let state = state.clone();
        cx.spawn(async move |this, cx| {
            let located = match futures::future::select(task, deadline).await {
                futures::future::Either::Left((located, _)) => located,
                futures::future::Either::Right(_) => {
                    this.update(cx, |view, cx| {
                        let pb = field(view);
                        if pb.generation == generation {
                            pb.abandon("Opening the sample took too long; try again.");
                            cx.notify();
                        }
                    })
                    .ok();
                    return;
                }
            };
            let Some(files) = located else {
                return;
            };
            this.update(cx, |view, cx| {
                if field(view).generation != generation {
                    return;
                }
                let outcome = match files {
                    Ok(files) => state.update(cx, |s, _| begin_clip(s, files, &clip)),
                    Err(e) => StartOutcome::Unavailable(failure_text(
                        "Couldn't open the sample.",
                        &core_user_text(&e),
                        "",
                    )),
                };
                match outcome {
                    StartOutcome::Preparing {
                        preparation,
                        pending,
                    } => run_preparation(
                        field(view),
                        &state,
                        generation,
                        preparation,
                        pending,
                        cx,
                        field,
                    ),
                    StartOutcome::MakeCache { meeting_id, sha256 } => {
                        field(view).cache_wait = Some(prepare_cache(
                            state.clone(),
                            meeting_id,
                            sha256,
                            clip.clone(),
                            generation,
                            cx,
                            field,
                        ));
                        field(view).status = Some(PREPARING_CACHE.into());
                    }
                    StartOutcome::Unavailable(message) => field(view).status = Some(message),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Stops this view's sample (if it holds the player) and forgets its
    /// status; a start in flight is cancelled (the player's preparation
    /// included) and its late result discarded. Never waits.
    pub fn stop(&mut self, state: &Entity<AppState>, cx: &mut App) {
        self.generation += 1;
        self.tick = None;
        self.cache_wait = None;
        self.end_timer = None;
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        if let Some(owner) = self.owner.take() {
            // Only while this view still holds the player: stops playback
            // and cancels a pending preparation, never another view's.
            state.update(cx, |s, _| s.release_player(owner));
        }
        self.playing = false;
        self.active = None;
        self.status = None;
    }

    /// Gives up the current start (deadline passed): its late result is
    /// discarded and `message` explains why nothing plays.
    fn abandon(&mut self, message: &str) {
        self.generation += 1;
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.status = Some(message.into());
    }
}

/// Status while a sample's file is found, prepared or still loading.
pub(super) const LOADING: &str = "Loading the sample…";
const PREPARING_CACHE: &str = "Preparing the recording for playback…";

/// Runs `preparation` (checking and probing the file, bounded by the
/// player's `PREPARE_TIMEOUT`) on the background executor, never on the UI
/// loop, then plays `pending` with its result.
fn run_preparation<V: 'static>(
    pb: &mut SamplePlayback,
    state: &Entity<AppState>,
    generation: u64,
    preparation: PlaybackPreparation,
    pending: PendingStart,
    cx: &mut Context<V>,
    field: fn(&mut V) -> &mut SamplePlayback,
) {
    pb.owner = Some(pending.owner);
    pb.end_s = pending.end_s;
    pb.cancel = Some(preparation.cancel_handle());
    let run = state
        .read(cx)
        .cli()
        .spawn(cx.background_executor(), async move { preparation.run() });
    let state = state.clone();
    cx.spawn(async move |this, cx| {
        let prepared = run.await;
        this.update(cx, |view, cx| {
            start_prepared(view, &state, generation, pending, prepared, cx, field)
        })
        .ok();
    })
    .detach();
}

/// The player prepared the file of start `generation` (off the UI loop):
/// plays it unless the start was superseded meanwhile (by this view, or by
/// another view taking the player), which ends silently. A file the player
/// can only play from the playable cache goes that way instead.
fn start_prepared<V: 'static>(
    view: &mut V,
    state: &Entity<AppState>,
    generation: u64,
    pending: PendingStart,
    prepared: Result<PreparedPlayback>,
    cx: &mut Context<V>,
    field: fn(&mut V) -> &mut SamplePlayback,
) {
    if field(view).generation != generation {
        return;
    }
    field(view).cancel = None;
    if prepared
        .as_ref()
        .is_err_and(|e| e.code() == PLAYBACK_NEEDS_CACHE)
    {
        play_from_cache(view, state, generation, pending, cx, field);
        cx.notify();
        return;
    }
    let owner = pending.owner;
    let started = prepared.map(|prepared| {
        state.update(cx, |s, _| {
            s.player_owned(owner)
                .map(|player| player.play_prepared(prepared, pending.start_s, Some(pending.end_s)))
        })
    });
    let pb = field(view);
    match started {
        Ok(Some(Ok(()))) => {
            pb.playing = true;
            pb.tick = Some(spawn_tick(state.clone(), generation, cx, field));
            observe(view, state, generation, cx, field);
        }
        // Another view took the player, or a newer request superseded this.
        Ok(None) => pb.reset(),
        Ok(Some(Err(e))) | Err(e)
            if matches!(e.code(), PLAYBACK_CANCELLED | PLAYBACK_SUPERSEDED) =>
        {
            pb.reset();
        }
        Ok(Some(Err(e))) | Err(e) => {
            pb.owner = None;
            pb.status = Some(sample_failure(&core_user_text(&e)));
        }
    }
    cx.notify();
}

/// The player cannot play the clip's file itself: plays it from the
/// playable cache, or queues that cache, as for a format the player never
/// plays natively.
fn play_from_cache<V: 'static>(
    view: &mut V,
    state: &Entity<AppState>,
    generation: u64,
    pending: PendingStart,
    cx: &mut Context<V>,
    field: fn(&mut V) -> &mut SamplePlayback,
) {
    let owner = pending.owner;
    let Some((files, clip)) = pending.retry else {
        let pb = field(view);
        pb.owner = None;
        pb.status = Some("Playback is not supported for this format.".into());
        return;
    };
    match plan_clip(files, &clip, |_| false) {
        ClipPlan::Play {
            path,
            start_s,
            end_s,
        } => match state.update(cx, |s, _| s.player_owned(owner).map(|p| p.prepare(&path))) {
            Some(Ok(preparation)) => run_preparation(
                field(view),
                state,
                generation,
                preparation,
                PendingStart {
                    owner,
                    start_s,
                    end_s,
                    retry: None,
                },
                cx,
                field,
            ),
            Some(Err(e)) => {
                let pb = field(view);
                pb.owner = None;
                pb.status = Some(sample_failure(&core_user_text(&e)));
            }
            None => field(view).reset(),
        },
        ClipPlan::PrepareCache { meeting_id, sha256 } => {
            let pb = field(view);
            pb.owner = None;
            pb.status = Some(PREPARING_CACHE.into());
            pb.cache_wait = Some(prepare_cache(
                state.clone(),
                meeting_id,
                sha256,
                clip,
                generation,
                cx,
                field,
            ));
        }
        ClipPlan::Unavailable(message) => {
            let pb = field(view);
            pb.owner = None;
            pb.status = Some(message);
        }
    }
}

impl SamplePlayback {
    /// Nothing active (a superseded start).
    fn reset(&mut self) {
        self.owner = None;
        self.playing = false;
        self.active = None;
        self.status = None;
    }
}

/// Polls the player for this view's sample and applies what it reports;
/// returns whether to keep polling. The clip's end timer starts once audio
/// plays (not while the player is still loading the file).
fn observe<V: 'static>(
    view: &mut V,
    state: &Entity<AppState>,
    generation: u64,
    cx: &mut Context<V>,
    field: fn(&mut V) -> &mut SamplePlayback,
) -> bool {
    if field(view).generation != generation {
        return false;
    }
    let owner = field(view).owner;
    let polled = state.update(cx, |s, _| {
        owner
            .and_then(|owner| s.player_owned(owner))
            .map(|p| p.poll())
    });
    let pb = field(view);
    match polled {
        Some(PlayerState::Loading { .. }) => {
            if pb.status.is_none() {
                pb.status = Some(LOADING.into());
                cx.notify();
            }
            true
        }
        Some(PlayerState::Playing { position_s }) => {
            if pb.status.take().is_some() {
                cx.notify();
            }
            if pb.end_timer.is_none()
                && let Some(owner) = owner
            {
                let left = (pb.end_s - position_s).max(0.0);
                pb.end_timer = Some(spawn_end_timer(
                    state.clone(),
                    generation,
                    owner,
                    left,
                    cx,
                    field,
                ));
            }
            true
        }
        Some(PlayerState::Paused { .. }) => {
            pb.playing = false;
            cx.notify();
            false
        }
        Some(PlayerState::Failed { message }) => {
            pb.playing = false;
            pb.status = Some(sample_failure(&sentence(&message)));
            cx.notify();
            false
        }
        Some(PlayerState::Finished { .. } | PlayerState::Stopped) | None => {
            pb.playing = false;
            pb.active = None;
            pb.status = None;
            cx.notify();
            false
        }
    }
}

/// Stops the clip `left_s` seconds after its audio started, without
/// waiting for the next poll.
fn spawn_end_timer<V: 'static>(
    state: Entity<AppState>,
    generation: u64,
    owner: u64,
    left_s: f64,
    cx: &mut Context<V>,
    field: fn(&mut V) -> &mut SamplePlayback,
) -> Task<()> {
    let duration = std::time::Duration::from_secs_f64(left_s);
    cx.spawn(async move |this, cx| {
        cx.background_executor().timer(duration).await;
        this.update(cx, |view, cx| {
            let pb = field(view);
            if pb.generation != generation {
                return;
            }
            state.update(cx, |state, _| state.release_player(owner));
            pb.owner = None;
            pb.playing = false;
            pb.active = None;
            pb.tick = None;
            cx.notify();
        })
        .ok();
    })
}

/// Polls the player at [`PLAYBACK_TICK`] while the sample loads or plays;
/// repaints only when that changes.
fn spawn_tick<V: 'static>(
    state: Entity<AppState>,
    generation: u64,
    cx: &mut Context<V>,
    field: fn(&mut V) -> &mut SamplePlayback,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(PLAYBACK_TICK).await;
            let keep_going = this
                .update(cx, |view, cx| observe(view, &state, generation, cx, field))
                .unwrap_or(false);
            if !keep_going {
                break;
            }
        }
    })
}

/// Queues the playable cache on the background executor and starts the
/// worker.
fn prepare_cache<V: 'static>(
    state: Entity<AppState>,
    meeting_id: MeetingId,
    sha256: String,
    clip: Clip,
    generation: u64,
    cx: &mut Context<V>,
    field: fn(&mut V) -> &mut SamplePlayback,
) -> Task<()> {
    use broscribe_core::queue::{JobState, Queue};
    let lib = state.read(cx).library().clone();
    let cache = playback_cache_path(&state.read(cx).paths().cache_dir, &sha256);
    let executor = cx.background_executor().clone();
    let enqueue = executor.spawn({
        let lib = lib.clone();
        async move { enqueue_playback_cache(&lib, &meeting_id, &sha256) }
    });
    cx.spawn(async move |this, cx| {
        let result = async {
            let job = enqueue.await.map_err(|error| core_user_text(&error))?;
            state.update(cx, |state, cx| state.kick_jobs(cx));
            loop {
                let lib = lib.clone();
                let cache = cache.clone();
                let job = job.clone();
                let ready = executor
                    .spawn(async move {
                        if cache.is_file() {
                            return Ok(true);
                        }
                        let job = Queue::open(lib.root())
                            .and_then(|queue| queue.get(&job))
                            .map_err(|error| core_user_text(&error))?;
                        match job.state {
                            JobState::Failed => {
                                Err(crate::jobs::playback_cache_failure(job.error.as_deref()))
                            }
                            JobState::Done => Err("The prepared recording is missing.".into()),
                            _ => Ok(false),
                        }
                    })
                    .await?;
                if ready {
                    return Ok::<_, String>(());
                }
                executor.timer(PLAYBACK_TICK).await;
            }
        }
        .await;
        let _ = this.update(cx, |view, cx| {
            let playback = field(view);
            if playback.generation != generation {
                return;
            }
            if let Err(cause) = result {
                playback.status = Some(failure_text(
                    "Couldn't prepare the recording for playback.",
                    &cause,
                    "",
                ));
                cx.notify();
            } else if let Some(key) = playback.active.clone() {
                playback.play(key, clip, &state, cx, field);
            }
        });
    })
}

// ---- small shared helpers ----

// ---- people: data ----

/// A row of the people list.
#[derive(Clone, Debug, PartialEq)]
pub struct PersonRow {
    pub summary: PersonSummary,
    /// Voiceprints in `voiceprints.json`.
    pub voice_samples: usize,
}

/// One voiceprint as the detail pane shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceSample {
    pub id: String,
    pub kind: VoiceprintKind,
    pub strength: Strength,
    pub device: Option<String>,
    pub meeting_id: Option<MeetingId>,
    pub meeting_title: Option<String>,
    pub meeting_date: Option<DateTime<FixedOffset>>,
    /// `None` when the sample has no playable source range.
    pub clip: Option<Clip>,
}

/// The selected person.
#[derive(Clone, Debug, PartialEq)]
pub struct PersonDetail {
    pub person: Person,
    pub notes: String,
    /// The persisted field hashes as read: an editor's baseline.
    pub revision: PersonRevision,
    pub samples: Vec<VoiceSample>,
    /// Meetings the person is named in, newest first.
    pub meetings: Vec<MeetingHit>,
    identity_checks: Option<std::result::Result<ops::IdentityChecks, String>>,
}

/// Every indexed person with their voice-sample count; empty without an index.
pub fn load_people(lib: &Library) -> Result<Vec<PersonRow>> {
    let root = lib.root();
    let summaries = match Index::open_readonly(root) {
        Ok(index) => index.list_people()?,
        Err(Error::NotFound(_)) if !Index::path(root).exists() => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut summaries = summaries;
    summaries.sort_by_key(|person| !person.is_self);
    Ok(summaries
        .into_iter()
        .map(|summary| {
            let voice_samples = match lib.load_voiceprints(&summary.id) {
                Ok(v) => v.embeddings.len(),
                Err(e) => {
                    tracing::warn!(code = e.code(), "reading voiceprints");
                    0
                }
            };
            PersonRow {
                summary,
                voice_samples,
            }
        })
        .collect())
}

/// The person's files, voice samples and meetings.
pub fn load_detail(lib: &Library, id: &PersonId) -> Result<PersonDetail> {
    let PersonState {
        person,
        notes,
        revision,
    } = ops::person_state(lib, id)?;
    let prints = lib.load_voiceprints(id)?;
    let meetings = match Index::open_readonly(lib.root()) {
        Ok(index) => index.search_meetings(&SearchQuery {
            person: Some(id.as_str().to_owned()),
            ..SearchQuery::default()
        })?,
        Err(Error::NotFound(_)) if !Index::path(lib.root()).exists() => Vec::new(),
        Err(e) => return Err(e),
    };
    let mut titles: HashMap<MeetingId, (Option<String>, DateTime<FixedOffset>)> = meetings
        .iter()
        .map(|m| (m.meeting_id.clone(), (m.title.clone(), m.started_at)))
        .collect();
    let mut samples = Vec::with_capacity(prints.embeddings.len());
    for vp in &prints.embeddings {
        let (meeting_title, meeting_date) = match &vp.source.meeting_id {
            Some(mid) => {
                if !titles.contains_key(mid) {
                    // A sample from a meeting the person is no longer named in.
                    let info = lib
                        .load_meeting(mid)
                        .map(|m| (m.title.clone(), m.started_at))
                        .ok();
                    if let Some(info) = info {
                        titles.insert(mid.clone(), info);
                    }
                }
                match titles.get(mid) {
                    Some((title, date)) => (title.clone(), Some(*date)),
                    None => (None, None),
                }
            }
            None => (None, None),
        };
        samples.push(VoiceSample {
            id: vp.id.clone(),
            kind: vp.source.kind,
            strength: vp.strength,
            device: vp.device.clone(),
            meeting_id: vp
                .source
                .meeting_id
                .clone()
                .filter(|id| titles.contains_key(id)),
            meeting_title,
            meeting_date,
            clip: clip_of(id, &vp.source),
        });
    }
    Ok(PersonDetail {
        person,
        notes,
        revision,
        samples,
        meetings,
        identity_checks: None,
    })
}

// ---- people: editing ----

/// The edit form's text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PersonForm {
    pub name: String,
    /// Comma- or newline-separated.
    pub aliases: String,
    /// Separated by commas, semicolons, spaces or newlines.
    pub emails: String,
    pub notes: String,
}

fn split_list(text: &str, separators: &[char]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in text.split(|c: char| separators.contains(&c)) {
        let item = item.trim();
        if !item.is_empty() && !out.iter().any(|o| o.to_lowercase() == item.to_lowercase()) {
            out.push(item.to_owned());
        }
    }
    out
}

fn diff_lists(before: &[String], after: &[String]) -> (Vec<String>, Vec<String>) {
    let has = |list: &[String], x: &str| list.iter().any(|y| y.to_lowercase() == x.to_lowercase());
    let add = after.iter().filter(|a| !has(before, a)).cloned().collect();
    let remove = before.iter().filter(|b| !has(after, b)).cloned().collect();
    (add, remove)
}

/// The user's changes from `baseline` (the person as read when the editor
/// opened) to `form`, fenced by the baseline's revision. Only fields the
/// user changed are sent, lists as additions and removals, so a field
/// changed elsewhere meanwhile is left alone, or refused (`stale_person`)
/// when the user changed it too: never overwritten. `Err` with a message
/// for an empty name.
pub fn edit_spec(
    baseline: &PersonState,
    form: &PersonForm,
) -> std::result::Result<PersonEditSpec, String> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err("The name can't be empty.".into());
    }
    let person = &baseline.person;
    let aliases = split_list(&form.aliases, &[',', '\n']);
    let emails = split_list(&form.emails, &[',', ';', ' ', '\t', '\n']);
    let (add_aliases, remove_aliases) = diff_lists(&person.aliases, &aliases);
    let (add_emails, remove_emails) = diff_lists(&person.emails, &emails);
    Ok(PersonEditSpec {
        expected: Some(baseline.revision.clone()),
        name: (name != person.name).then(|| name.to_owned()),
        add_aliases,
        remove_aliases,
        add_emails,
        remove_emails,
        notes: (form.notes.trim_end() != baseline.notes.trim_end()).then(|| form.notes.clone()),
        ..PersonEditSpec::default()
    })
}

/// Every field an editor shows.
pub const EDIT_FIELDS: [PersonField; 4] = [
    PersonField::Name,
    PersonField::Aliases,
    PersonField::Emails,
    PersonField::Notes,
];

/// The fields whose persisted value differs between two reads.
pub fn changed_fields(before: &PersonRevision, after: &PersonRevision) -> Vec<PersonField> {
    EDIT_FIELDS
        .into_iter()
        .filter(|f| before.field(*f) != after.field(*f))
        .collect()
}

fn field_label(field: PersonField) -> &'static str {
    match field {
        PersonField::Name => "name",
        PersonField::Aliases => "other names",
        PersonField::Emails => "emails",
        PersonField::Notes => "notes",
    }
}

fn field_list(fields: &[PersonField]) -> String {
    let names = fields.iter().copied().map(field_label).collect::<Vec<_>>();
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_owned(),
        Some((last, before)) => format!("{} and {last}", before.join(", ")),
    }
}

/// The editor's notice about changes made elsewhere while it is open.
pub fn external_change_notice(fields: &[PersonField]) -> Option<String> {
    let names = field_list(fields);
    let verb = if fields == [PersonField::Name] {
        "The name"
    } else {
        &names
    };
    let mut subject = verb.to_owned();
    if let Some(first) = subject.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    (!fields.is_empty()).then(|| format!(
        "{subject} changed elsewhere while you were editing. Your changes to other fields will still save."
    ))
}

/// Why a save was refused because the same fields changed elsewhere.
pub fn conflict_message(fields: &[PersonField]) -> String {
    let names = field_list(fields);
    format!(
        "Not saved: {names} also changed elsewhere. Your text is still here. Cancel to see the current {names}, then edit again."
    )
}

/// `people edit <person> --spec-stdin`: the spec, notes included, goes on
/// stdin ([`spec_stdin`]), never in argv.
pub fn edit_args(person: &PersonId) -> Vec<OsString> {
    vec![
        "people".into(),
        "edit".into(),
        person.as_str().into(),
        "--spec-stdin".into(),
    ]
}

pub fn spec_stdin(spec: &PersonEditSpec) -> Vec<u8> {
    serde_json::to_vec(spec).expect("a person edit serializes")
}

/// `people undo-edit <person> --undo-id <id>` (the ID is opaque, not data).
pub fn undo_args(person: &PersonId, undo_id: &str) -> Vec<OsString> {
    vec![
        "people".into(),
        "undo-edit".into(),
        person.as_str().into(),
        "--undo-id".into(),
        undo_id.into(),
    ]
}

/// Why a person edit was not saved.
#[derive(Clone, Debug, PartialEq)]
pub enum SaveRefusal {
    /// Fields the user changed were changed elsewhere too; nothing was written.
    Conflict(Vec<PersonField>),
    /// The person was forgotten or deleted meanwhile; nothing is re-created.
    Gone,
    /// Why, in words ([`CliError::user_text`]).
    Failed(String),
}

impl SaveRefusal {
    pub fn message(&self) -> String {
        match self {
            SaveRefusal::Conflict(fields) => conflict_message(fields),
            SaveRefusal::Gone => {
                "Not saved: this person was forgotten or deleted elsewhere.".to_owned()
            }
            SaveRefusal::Failed(cause) => failure_text("Couldn't save the changes.", cause, ""),
        }
    }
}

/// Saves `spec` on `person` through `broscribe people edit --spec-stdin`
/// and returns the edit's Undo ID (`None` when nothing changed). A
/// `stale_person` refusal is explained by the fields of `spec` whose
/// persisted value no longer matches `spec.expected` (blocking: background
/// executor only).
fn save_person_edit(
    cli: &CliRunner,
    lib: &Library,
    person: &PersonId,
    spec: &PersonEditSpec,
) -> std::result::Result<Option<String>, SaveRefusal> {
    match cli.run_blocking_stdin(&edit_args(person), ChildKind::Command, spec_stdin(spec)) {
        Ok(out) => Ok(out["undo_id"].as_str().map(str::to_owned)),
        Err(e) => Err(match e.code.as_str() {
            "not_found" => SaveRefusal::Gone,
            "stale_person" => match ops::person_state(lib, person) {
                Err(Error::NotFound(_)) => SaveRefusal::Gone,
                now => {
                    let addressed = spec.fields();
                    let mut fields: Vec<PersonField> = match (&spec.expected, now) {
                        (Some(expected), Ok(now)) => changed_fields(expected, &now.revision)
                            .into_iter()
                            .filter(|f| addressed.contains(f))
                            .collect(),
                        _ => Vec::new(),
                    };
                    if fields.is_empty() {
                        fields = addressed.into_iter().collect();
                    }
                    SaveRefusal::Conflict(fields)
                }
            },
            _ => SaveRefusal::Failed(e.user_text()),
        }),
    }
}

/// What a refused Undo tells the user: by its code, else why in words.
pub fn undo_refusal(error: &CliError) -> String {
    match error.code.as_str() {
        "stale_person" => {
            "Can't undo: these details changed again since; nothing was changed.".to_owned()
        }
        "undo_unavailable" => "This edit can no longer be undone.".to_owned(),
        "not_found" => "Can't undo: this person was forgotten; nothing is re-created.".to_owned(),
        _ => failure_text("Couldn't undo.", &error.user_text(), ""),
    }
}

/// Undoes the edit `undo_id` of `person` (blocking: background executor only).
fn undo_person_edit(
    cli: &CliRunner,
    person: &PersonId,
    undo_id: &str,
) -> std::result::Result<(), String> {
    cli.run_blocking(&undo_args(person, undo_id), ChildKind::Command)
        .map(|_| ())
        .map_err(|e| undo_refusal(&e))
}

/// Forget voice or forget person.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForgetKind {
    Voice,
    Person,
}

/// A committed forget and whether cached directory photos of the person
/// remain after the purge that follows forget-person.
struct Forgotten {
    outcome: ForgetOutcome,
    /// Some integration or account still holds a cached photo: the purge
    /// failed there (it never fails the forget it follows) and is reported
    /// beside its success.
    photos_left: bool,
}

fn forget(lib: &Library, data_dir: &Path, id: &PersonId, kind: ForgetKind) -> Result<Forgotten> {
    let outcome = ops::with_index(lib, |tx, index| match kind {
        ForgetKind::Voice => ops::forget_voice(tx, index, id.as_str()),
        ForgetKind::Person => ops::forget_person(tx, index, id.as_str()),
    })?;
    // Design §10: only forgetting the person purges their directory photos.
    // The forget is already committed, so a purge failure is not its failure.
    let photos_left = kind == ForgetKind::Person
        && match broscribe_integrations::forget_person_photos(data_dir, id.as_str()) {
            Ok(purge) => purge.status == broscribe_integrations::PhotoPurgeStatus::Partial,
            Err(e) => {
                tracing::warn!(
                    code = e.code(),
                    "removing cached directory photos of a forgotten person"
                );
                true
            }
        };
    Ok(Forgotten {
        outcome,
        photos_left,
    })
}

// ---- people: directory lookup and photos ----

/// A directory search result and the account generation it was read under.
#[derive(Clone, Debug, PartialEq)]
pub struct DirectoryMatch {
    pub integration: IntegrationId,
    pub generation: u64,
    pub source: Option<broscribe_integrations::AccountDirectorySource>,
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DirectorySearch {
    pub results: Vec<DirectoryMatch>,
    /// One line per signed-in account that could not be searched.
    pub errors: Vec<String>,
}

/// Each account's generation (its sign-in epoch) when read.
pub type AccountGenerations = BTreeMap<IntegrationId, u64>;

fn account_generations(data_dir: &Path) -> AccountGenerations {
    IntegrationId::ALL
        .into_iter()
        .filter_map(|id| {
            broscribe_integrations::account_generation(data_dir, id)
                .ok()
                .map(|generation| (id, generation))
        })
        .collect()
}

/// `people lookup` output. An account whose generation changed while the
/// search ran (signed out or replaced) contributes no results.
pub fn parse_lookup(
    value: &serde_json::Value,
    before: &AccountGenerations,
    after: &AccountGenerations,
) -> DirectorySearch {
    let mut search = DirectorySearch {
        errors: account_errors(value),
        ..DirectorySearch::default()
    };
    for result in value["results"].as_array().into_iter().flatten() {
        let Ok(integration) =
            serde_json::from_value::<IntegrationId>(result["integration"].clone())
        else {
            continue;
        };
        let source = serde_json::from_value::<broscribe_integrations::AccountDirectorySource>(
            result["person"]["source"].clone(),
        )
        .ok();
        let generation = if integration == IntegrationId::MicrosoftGraph {
            let Some(source) = &source else {
                continue;
            };
            if source.account.integration.as_str() != integration.as_str() {
                continue;
            }
            source.generation
        } else {
            let (Some(read_at), Some(now)) = (before.get(&integration), after.get(&integration))
            else {
                continue;
            };
            if read_at != now {
                continue;
            }
            *now
        };
        let text = |key: &str| {
            result["person"][key]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        search.results.push(DirectoryMatch {
            integration,
            generation,
            source,
            name: text("name"),
            email: text("email"),
        });
    }
    search
}

fn account_errors(value: &serde_json::Value) -> Vec<String> {
    value["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let integration = e["account"]["integration"]
                .as_str()
                .or_else(|| e["integration"].as_str())
                .unwrap_or("integration");
            let identity = e["account"]["account_id"]
                .as_str()
                .map(|handle| format!("{integration}/{handle}"))
                .unwrap_or_else(|| integration.to_owned());
            let label = e["account_label"]
                .as_str()
                .filter(|label| !label.is_empty());
            format!(
                "{}{}: {}",
                identity,
                label.map(|label| format!(" ({label})")).unwrap_or_default(),
                e["message"].as_str().unwrap_or("failed")
            )
        })
        .collect()
}

/// Searches the signed-in accounts' directories through `people lookup
/// --query-stdin` (the query never in argv; blocking: background executor
/// only). Never creates a local person.
fn search_directory(
    cli: &CliRunner,
    data_dir: &Path,
    query: &str,
) -> std::result::Result<DirectorySearch, String> {
    let before = account_generations(data_dir);
    let args = ["people", "lookup", "--query-stdin"].map(OsString::from);
    let stdin = serde_json::to_vec(&serde_json::json!({ "query": query }))
        .expect("a lookup query serializes");
    let value = cli
        .run_blocking_stdin(&args, ChildKind::Command, stdin)
        .map_err(|e| failure_text("Couldn't search the directory.", &e.user_text(), ""))?;
    Ok(parse_lookup(
        &value,
        &before,
        &account_generations(data_dir),
    ))
}

/// The `people edit` change linking `email` to `person`; `None` when they
/// already have that exact address (case-insensitive).
pub fn link_edit(person: &Person, email: &str) -> Option<PersonEditSpec> {
    let email = email.trim();
    (!email.is_empty() && !person.emails.iter().any(|e| e.eq_ignore_ascii_case(email))).then(|| {
        PersonEditSpec {
            add_emails: vec![email.to_owned()],
            ..PersonEditSpec::default()
        }
    })
}

/// Why `found` can no longer be linked, if so: the person was forgotten, the
/// result has no email, or the account changed generation since the search.
pub fn link_refusal(
    found: &DirectoryMatch,
    current_generation: u64,
    person_exists: bool,
) -> Option<&'static str> {
    if !person_exists {
        Some("This person was forgotten.")
    } else if found.email.is_none() {
        Some("This result has no email address to link.")
    } else if current_generation != found.generation {
        Some("The account changed since this search. Search again.")
    } else {
        None
    }
}

/// A photo request's result: the cached photo, if any account had one, and
/// per-account problems.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhotoOutcome {
    pub photo: Option<PathBuf>,
    pub errors: Vec<String>,
}

pub fn parse_photo(value: &serde_json::Value) -> PhotoOutcome {
    PhotoOutcome {
        photo: value["photos"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|p| p["path"].as_str())
            .map(PathBuf::from),
        errors: account_errors(value),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhotoSource {
    Google,
    Microsoft(broscribe_core::calendar::AccountRef),
}

impl PhotoSource {
    fn selection_key(&self) -> String {
        match self {
            Self::Google => "google".into(),
            Self::Microsoft(account) => {
                format!("{}/{}", account.integration.as_str(), account.account_id)
            }
        }
    }

    pub fn args(&self, person: &PersonId) -> Vec<OsString> {
        let mut args = vec!["people".into(), "photo".into(), person.as_str().into()];
        if let Self::Microsoft(account) = self {
            args.extend([
                "--account".into(),
                format!("{}/{}", account.integration.as_str(), account.account_id).into(),
            ]);
        }
        args
    }
}

#[derive(Clone, Debug)]
pub struct PhotoAccountChoice {
    pub source: PhotoSource,
    pub label: String,
    pub available: bool,
}

pub fn parse_photo_accounts(
    value: &serde_json::Value,
) -> std::result::Result<Vec<PhotoAccountChoice>, String> {
    let statuses: Vec<broscribe_integrations::IntegrationStatus> =
        serde_json::from_value(value["integrations"].clone()).map_err(|error| {
            tracing::warn!(category = ?error.classify(), "reading the photo accounts");
            failure_text(
                "Couldn't check the photo accounts.",
                &CliError::new("bad_output", "the photo accounts couldn't be read").user_text(),
                "",
            )
        })?;
    let mut choices = Vec::new();
    for status in statuses {
        if status.integration == IntegrationId::Google {
            choices.push(PhotoAccountChoice {
                source: PhotoSource::Google,
                label: "Google".into(),
                available: status.configured
                    && status.state.status == broscribe_integrations::AccountStatus::Active,
            });
        }
        for (index, account) in status.microsoft_accounts.into_iter().enumerate() {
            let available = account.status == broscribe_integrations::AccountStatus::Active
                && account.access_error.is_none();
            choices.push(PhotoAccountChoice {
                label: crate::views::integrations::microsoft_account_label(&account, index),
                source: PhotoSource::Microsoft(account.account),
                available,
            });
        }
    }
    Ok(choices)
}

#[derive(Debug, Default)]
struct PhotoAccounts {
    choices: Vec<PhotoAccountChoice>,
    selected: Option<PhotoAccountChoice>,
    generation: u64,
    loading: bool,
    error: Option<String>,
}

/// Asks the signed-in accounts for `person`'s photo through `people photo`,
/// which matches the person's emails exactly and re-checks that the person
/// exists and the account generation is unchanged before caching (blocking).
fn request_photo(
    cli: &CliRunner,
    lib: &Library,
    person: &PersonId,
    source: &PhotoSource,
) -> std::result::Result<PhotoOutcome, String> {
    match lib.load_person(person) {
        Ok(p) if p.emails.is_empty() => {
            return Err("Add an email address first: photos match by exact email.".into());
        }
        Ok(_) => {}
        Err(Error::NotFound(_)) => return Err("This person was forgotten.".into()),
        Err(e) => {
            return Err(failure_text(
                "Couldn't get the photo.",
                &core_user_text(&e),
                "",
            ));
        }
    }
    let args = source.args(person);
    let value = cli
        .run_blocking(&args, ChildKind::Command)
        .map_err(|e| failure_text("Couldn't get the photo.", &e.user_text(), ""))?;
    Ok(parse_photo(&value))
}

/// `people edit` arguments for a [`link_edit`] spec (sent on stdin) that the
/// child commits only while `found`'s account is still signed in at the
/// generation the result was read under: it checks under that account's
/// lock and holds it through the commit, so a sign-out or replacement
/// meanwhile adds nothing.
pub fn link_args(person: &PersonId, found: &DirectoryMatch) -> Vec<OsString> {
    let mut args = edit_args(person);
    let identity = found.source.as_ref().map_or_else(
        || found.integration.as_str().to_owned(),
        |source| {
            format!(
                "{}/{}",
                source.account.integration.as_str(),
                source.account.account_id
            )
        },
    );
    args.push(format!("--expect-generation={identity}:{}", found.generation).into());
    args
}

/// "Couldn't add the email address. {Cause}."
fn link_failure(cause: &str) -> String {
    failure_text("Couldn't add the email address.", cause, "")
}

/// Adds a directory result's email to `person`, then requests their photo
/// (blocking: background executor only).
fn link_directory_match(
    cli: &CliRunner,
    lib: &Library,
    data_dir: &Path,
    person: &PersonId,
    found: &DirectoryMatch,
) -> std::result::Result<PhotoOutcome, String> {
    let current = match lib.load_person(person) {
        Ok(p) => Some(p),
        Err(Error::NotFound(_)) => None,
        Err(e) => return Err(link_failure(&core_user_text(&e))),
    };
    // Early refusal without a child; the child re-checks at the commit.
    // The child rechecks Microsoft authority atomically under its account lock.
    let generation = match &found.source {
        Some(source) => source.generation,
        None => broscribe_integrations::account_generation(data_dir, found.integration).map_err(
            |e| {
                let public = e.public();
                tracing::warn!(code = %public.code, "reading the account generation");
                link_failure(&CliError::answered(&public.code, public.message).user_text())
            },
        )?,
    };
    if let Some(reason) = link_refusal(found, generation, current.is_some()) {
        return Err(reason.into());
    }
    let (Some(current), Some(email)) = (current, found.email.as_deref()) else {
        return Err("This result can no longer be linked.".into());
    };
    if let Some(spec) = link_edit(&current, email) {
        cli.run_blocking_stdin(
            &link_args(person, found),
            ChildKind::Command,
            spec_stdin(&spec),
        )
        .map_err(|e| link_failure(&e.user_text()))?;
    }
    let source = found.source.as_ref().map_or(PhotoSource::Google, |source| {
        PhotoSource::Microsoft(source.account.clone())
    });
    request_photo(cli, lib, person, &source)
}

/// Cached photos of the given people (display only; no network), read into
/// memory and keyed by their bytes ([`Image::id`]). GPUI caches images by
/// source, and a refreshed photo replaces the same file, so a path source
/// would keep showing the old picture. One batched lookup per load: the
/// integrations resolve their authorized photo directories once and drop any
/// photo whose account lost its authority while the bytes were being read.
fn cached_photos<'a>(
    data_dir: &Path,
    people: impl IntoIterator<Item = &'a PersonId>,
) -> HashMap<PersonId, Arc<Image>> {
    broscribe_integrations::cached_photos(
        data_dir,
        people.into_iter().map(|id| (id, id.as_str())),
        photo_image,
    )
    .into_iter()
    .map(|(id, image)| (id.clone(), image))
    .collect()
}

/// The photo at `path` (a file removed meanwhile, or of an unknown format, has none).
fn photo_image(path: &Path) -> Option<Arc<Image>> {
    let bytes = std::fs::read(path).ok()?;
    let format = image_format(&bytes)?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

/// The format by magic bytes: directories serve JPEG or PNG, rarely the others.
fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else if bytes.starts_with(b"BM") {
        Some(ImageFormat::Bmp)
    } else {
        None
    }
}

/// Images shown before that `now` no longer shows (replaced or removed, e.g.
/// by sign-out): their decoded frames leave GPUI's image cache.
pub fn dropped_photos(
    before: &HashMap<PersonId, Arc<Image>>,
    now: &HashMap<PersonId, Arc<Image>>,
) -> Vec<Arc<Image>> {
    let kept: HashSet<u64> = now.values().map(|image| image.id()).collect();
    before
        .values()
        .filter(|image| !kept.contains(&image.id()))
        .cloned()
        .collect()
}

// ---- people: view ----

struct Editor {
    /// Distinguishes this editor from later ones; a save's completion only
    /// touches the editor (and person) it was started from.
    generation: u64,
    /// The person as read when editing started (values and revision);
    /// reloads never move it, and saves are fenced by its revision.
    baseline: Option<PersonState>,
    /// Fields the latest read found changed elsewhere since `baseline`.
    external: Vec<PersonField>,
    name: Entity<InputState>,
    aliases: Entity<InputState>,
    emails: Entity<InputState>,
    notes: Entity<TextareaState>,
    saving: bool,
    name_error: Option<String>,
    last_field: FocusHandle,
    _subscriptions: Vec<Subscription>,
    error: Option<String>,
}

impl Editor {
    fn form(&self, cx: &App) -> PersonForm {
        PersonForm {
            name: self.name.read(cx).value().to_string(),
            aliases: self.aliases.read(cx).value().to_string(),
            emails: self.emails.read(cx).value().to_string(),
            notes: self.notes.read(cx).value().to_string(),
        }
    }

    fn dirty_fields(&self, cx: &App) -> Vec<PersonField> {
        let Some(baseline) = &self.baseline else {
            let form = self.form(cx);
            return [
                (PersonField::Name, form.name),
                (PersonField::Emails, form.emails),
                (PersonField::Notes, form.notes),
            ]
            .into_iter()
            .filter_map(|(field, text)| (!text.trim().is_empty()).then_some(field))
            .collect();
        };
        let mut form = self.form(cx);
        let empty_name = form.name.trim().is_empty() && !baseline.person.name.is_empty();
        if form.name.trim().is_empty() {
            form.name = baseline.person.name.clone();
            if form.name.is_empty() {
                form.name = "Me".into();
            }
        }
        let mut fields = edit_spec(baseline, &form)
            .map(|spec| spec.fields())
            .unwrap_or_default();
        if empty_name && !fields.contains(&PersonField::Name) {
            fields.insert(PersonField::Name);
        } else if baseline.person.name.is_empty() && self.name.read(cx).value().trim().is_empty() {
            fields.retain(|field| *field != PersonField::Name);
        }
        fields.into_iter().collect()
    }
}

#[derive(Clone, Copy)]
enum ListAction {
    Edit,
    Forget(ForgetKind),
}

struct PendingSelection {
    person: PersonId,
    open_detail: bool,
    return_focus: FocusHandle,
}

/// The short Undo offered after a saved person edit (design §4 People: not
/// for forget-voice or forget-person, which have none).
struct PersonUndo {
    person: PersonId,
    undo_id: String,
    running: bool,
    saved_message: String,
    /// Withdraws the offer after [`UNDO_WINDOW`].
    _expiry: Task<()>,
}

pub struct PeopleView {
    importing_contacts: bool,
    contacts_tip: Option<bool>,
    pub(super) import_contacts_result: Option<Result<String, String>>,
    state: Entity<AppState>,
    embedded: bool,
    setup_deps: Option<crate::views::setup::SetupDeps>,
    focus_handle: FocusHandle,
    detail_focus: FocusHandle,
    single_detail: bool,
    single_forward: bool,
    list_scroll: gpui::ScrollHandle,
    row_popup: Option<kit::RowPopup>,
    search: Entity<InputState>,
    people_index: PeopleIndex,
    visible_rows: Vec<usize>,
    type_select: Option<(Instant, String)>,
    discard_focus: FocusHandle,
    edit_focus: FocusHandle,
    pending_selection: Option<PendingSelection>,
    forget_voice_focus: FocusHandle,
    forget_person_focus: FocusHandle,
    people: Vec<PersonRow>,
    list_loading: bool,
    list_error: Option<String>,
    list_generation: u64,
    seen_revisions: (u64, u64),
    /// The library changed since the last load while the view was hidden:
    /// it reloads once when People is shown again.
    stale: bool,
    selected: Option<PersonId>,
    detail: Option<PersonDetail>,
    detail_loading: bool,
    list_action_on_load: Option<ListAction>,
    detail_error: Option<String>,
    detail_generation: u64,
    editor: Option<Editor>,
    add_focus: FocusHandle,
    editor_generation: u64,
    confirm: Option<ForgetKind>,
    /// People whose forget is running.
    forgetting: HashSet<PersonId>,
    /// Result of the last edit or forget.
    message: Option<String>,
    message_kind: StatusKind,
    record_after_forget: bool,
    forget_recheck: Option<broscribe_core::learning::LearningRequest>,
    /// Undo of the last saved edit, while offered.
    undo: Option<PersonUndo>,
    playback: SamplePlayback,
    /// Cached directory photos by person (display only).
    photos: HashMap<PersonId, Arc<Image>>,
    directory_input: Entity<InputState>,
    directory_focus: FocusHandle,
    directory: DirectoryPane,
    photo_accounts: PhotoAccounts,
    _subscriptions: Vec<Subscription>,
}

/// The selected person's directory search, link and photo requests. Every
/// result is fenced by `generation` and the person it was started for.
#[derive(Debug, Default)]
struct DirectoryPane {
    open: bool,
    generation: u64,
    searching: bool,
    /// The result being linked.
    linking: Option<usize>,
    photo_busy: bool,
    search: Option<DirectorySearch>,
    message: Option<String>,
}

impl DirectoryPane {
    /// Forgets results and invalidates requests in flight (another person).
    fn reset(&mut self) {
        self.generation += 1;
        self.searching = false;
        self.linking = None;
        self.photo_busy = false;
        self.search = None;
        self.message = None;
    }

    fn busy(&self) -> bool {
        self.linking.is_some() || self.photo_busy
    }

    /// Starts a search, returning its generation. Refused while a link or
    /// photo request runs: starting over would orphan its result and busy state.
    fn start_search(&mut self) -> Option<u64> {
        if self.busy() {
            self.message = Some("Wait for the link or photo request to finish.".into());
            return None;
        }
        self.reset();
        self.searching = true;
        Some(self.generation)
    }

    /// Starts linking result `ix` (one link or photo request at a time).
    fn start_link(&mut self, ix: usize) -> Option<u64> {
        if self.busy() {
            return None;
        }
        self.linking = Some(ix);
        self.message = None;
        Some(self.generation)
    }

    /// Starts a photo request (one link or photo request at a time).
    fn start_photo(&mut self) -> Option<u64> {
        if self.busy() {
            return None;
        }
        self.photo_busy = true;
        self.message = None;
        Some(self.generation)
    }
}

/// What the user sees after a photo request.
pub fn photo_message(outcome: &PhotoOutcome) -> String {
    let mut message = if outcome.photo.is_some() {
        "Photo updated.".to_owned()
    } else {
        "The selected account has no photo for these email addresses.".to_owned()
    };
    for error in &outcome.errors {
        message.push(' ');
        message.push_str(error);
    }
    message
}

fn playback_of(view: &mut PeopleView) -> &mut SamplePlayback {
    &mut view.playback
}

impl PeopleView {
    pub(crate) fn embedded(mut self, deps: crate::views::setup::SetupDeps) -> Self {
        self.embedded = true;
        self.setup_deps = Some(deps);
        self
    }

    pub(crate) fn list_is_visible(&self, layout: crate::ui::PaneLayout) -> bool {
        (!self.people.is_empty() || self.list_error.is_some())
            && (layout != crate::ui::PaneLayout::Single || !self.single_detail)
    }

    pub(crate) fn detail_is_visible(&self, layout: crate::ui::PaneLayout) -> bool {
        (self.selected.is_some() || self.adding_person())
            && (layout != crate::ui::PaneLayout::Single || self.single_detail)
    }

    pub(crate) fn selected_person(&self) -> Option<&PersonId> {
        self.selected.as_ref()
    }

    pub(crate) fn can_undo_for_focus(&self, window: &Window, cx: &App) -> bool {
        self.editor.is_none()
            && self.undo.as_ref().is_some_and(|undo| !undo.running)
            && self.detail_focus.contains_focused(window, cx)
    }

    pub(crate) fn detail_focus(&self) -> &FocusHandle {
        &self.detail_focus
    }

    pub(crate) fn can_switch_single_pane(&self) -> bool {
        self.selected.is_some() && (self.single_detail || self.single_forward)
    }

    pub(crate) fn show_single_detail(
        &mut self,
        detail: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.selected.is_none() || self.single_detail == detail {
            return false;
        }
        self.single_detail = detail;
        self.single_forward = !detail;
        let focus = if detail {
            &self.detail_focus
        } else {
            &self.focus_handle
        }
        .clone();
        window.on_next_frame(move |window, cx| focus.focus(window, cx));
        cx.notify();
        true
    }

    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let seen_revisions = revisions(&state, cx);
        let directory_input = cx.new(|cx| InputState::new(window, cx).placeholder("Name or email"));
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search people")
                .clean_on_escape()
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &search,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.search_changed(window, cx);
                    }
                },
            ),
            // Retained while another route shows: a change only marks the
            // list stale; it reloads (once) on People.
            cx.observe(&state, |this: &mut Self, state, cx| {
                let now = revisions(&state, cx);
                if now != this.seen_revisions {
                    this.seen_revisions = now;
                    this.stale = true;
                }
                if matches!(state.read(cx).route(), Route::People { .. }) {
                    if this.stale {
                        this.reload(cx);
                    }
                    if this.forget_recheck.is_some() {
                        cx.notify();
                    }
                } else if this.embedded {
                    this.row_popup = None;
                    if let Some(undo) = this.undo.take()
                        && this.message_kind == StatusKind::Done
                        && this.message.as_ref() == Some(&undo.saved_message)
                    {
                        this.message = None;
                    }
                    if this.playback.active().is_some() {
                        this.playback.stop(&state, cx);
                    }
                }
            }),
            cx.subscribe_in(
                &directory_input,
                window,
                |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.search_directory(cx);
                    }
                },
            ),
        ];
        let mut view = Self {
            importing_contacts: false,
            contacts_tip: None,
            import_contacts_result: None,
            state,
            embedded: false,
            setup_deps: None,
            focus_handle: cx.focus_handle(),
            detail_focus: cx.focus_handle(),
            single_detail: false,
            single_forward: false,
            list_scroll: gpui::ScrollHandle::new(),
            row_popup: None,
            search,
            people_index: PeopleIndex::default(),
            visible_rows: Vec::new(),
            type_select: None,
            discard_focus: cx.focus_handle(),
            edit_focus: cx.focus_handle(),
            pending_selection: None,
            forget_voice_focus: cx.focus_handle(),
            forget_person_focus: cx.focus_handle(),
            people: Vec::new(),
            list_loading: false,
            list_error: None,
            list_generation: 0,
            seen_revisions,
            stale: false,
            selected: None,
            detail: None,
            detail_loading: false,
            list_action_on_load: None,
            detail_error: None,
            detail_generation: 0,
            editor: None,
            add_focus: cx.focus_handle(),
            editor_generation: 0,
            confirm: None,
            forgetting: HashSet::new(),
            message: None,
            message_kind: StatusKind::Info,
            record_after_forget: false,
            forget_recheck: None,
            undo: None,
            playback: SamplePlayback::default(),
            photos: HashMap::new(),
            directory_input,
            directory_focus: cx.focus_handle(),
            directory: DirectoryPane::default(),
            photo_accounts: PhotoAccounts::default(),
            _subscriptions: subscriptions,
        };
        view.reload(cx);
        view
    }

    #[cfg(test)]
    pub(crate) fn people(&self) -> &[PersonRow] {
        &self.people
    }

    #[cfg(test)]
    pub(crate) fn detail(&self) -> Option<&PersonDetail> {
        self.detail.as_ref()
    }

    /// Reloads the list and the selected person (background executor).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.stale = false;
        self.list_error = None;
        self.list_generation += 1;
        let generation = self.list_generation;
        self.list_loading = true;
        let lib = self.state.read(cx).library().clone();
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let task = cx.background_executor().spawn(async move {
            load_people(&lib).map(|people| {
                let photos = cached_photos(&data_dir, people.iter().map(|row| &row.summary.id));
                (people, photos)
            })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                if view.list_generation != generation {
                    return;
                }
                view.list_loading = false;
                match result {
                    Ok((people, photos)) => {
                        for image in dropped_photos(&view.photos, &photos) {
                            ImageSource::Image(image).remove_asset(cx);
                        }
                        view.people_index = person_picker::index_people(
                            people.iter().map(|row| row.summary.clone()).collect(),
                        );
                        view.people = people;
                        view.filter_rows(cx);
                        view.photos = photos;
                        view.list_error = None;
                    }
                    Err(e) => {
                        tracing::warn!(code = e.code(), "reading people");
                        view.list_error = Some(core_user_text(&e))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        // An open editor keeps its own baseline, so the detail reloads too:
        // that is how changes made elsewhere meanwhile are noticed.
        if self.selected.is_some() {
            self.load_detail(cx);
        }
        cx.notify();
    }

    /// Shows `id` in the detail pane.
    pub fn select(&mut self, id: PersonId, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&id) {
            return;
        }
        self.playback.stop(&self.state, cx);
        self.selected = Some(id);
        self.detail = None;
        self.list_action_on_load = None;
        self.editor = None;
        self.confirm = None;
        self.message = None;
        self.record_after_forget = false;
        self.undo = None;
        self.pending_selection = None;
        self.directory.open = false;
        self.directory.reset();
        self.load_detail(cx);
    }

    pub(crate) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.single_detail
            && crate::ui::PaneLayout::for_window(window) == crate::ui::PaneLayout::Single
        {
            self.single_detail = false;
            self.single_forward = true;
        }
        self.search
            .update(cx, |search, cx| search.focus(window, cx));
        cx.notify();
    }

    fn filter_rows(&mut self, cx: &App) {
        self.visible_rows =
            person_picker::matching_people(&self.people_index, &self.search.read(cx).value());
    }

    fn search_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_rows(cx);
        let hides_selected = self.selected.as_ref().is_some_and(|id| {
            !self
                .visible_rows
                .iter()
                .any(|&index| &self.people[index].summary.id == id)
        });
        if hides_selected && let Some(&index) = self.visible_rows.first() {
            self.request_selection(self.people[index].summary.id.clone(), false, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn request_selection(
        &mut self,
        person: PersonId,
        open_detail: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.as_ref() != Some(&person)
            && let Some(editor) = &self.editor
        {
            if editor.saving {
                return;
            }
            if !editor.dirty_fields(cx).is_empty() {
                let return_focus = self
                    .pending_selection
                    .take()
                    .map(|pending| pending.return_focus)
                    .unwrap_or_else(|| editor.last_field.clone());
                self.pending_selection = Some(PendingSelection {
                    person,
                    open_detail,
                    return_focus,
                });
                self.discard_focus.focus(window, cx);
                cx.notify();
                return;
            }
        }
        self.select(person, cx);
        if let Some(index) = self
            .visible_rows
            .iter()
            .position(|&i| self.selected.as_ref() == Some(&self.people[i].summary.id))
        {
            self.list_scroll.scroll_to_item(index);
        }
        if open_detail
            && self.embedded
            && self.editor.is_none()
            && (crate::ui::PaneLayout::for_window(window) != crate::ui::PaneLayout::Single
                || !self.show_single_detail(true, window, cx))
        {
            self.detail_focus.focus(window, cx);
        }
    }

    fn resolve_selection(&mut self, discard: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_selection.take() else {
            return;
        };
        if discard {
            self.editor = None;
            self.focus_handle.focus(window, cx);
            self.request_selection(pending.person, pending.open_detail, window, cx);
        } else {
            // A link put its target in the route before the editor could
            // ask. Restore the selection so the same link can be tried again.
            self.state.update(cx, |state, cx| {
                if matches!(state.route(), Route::People { person: Some(id) } if id == &pending.person)
                {
                    state.navigate(Route::People { person: self.selected.clone() }, cx);
                }
            });
            pending.return_focus.focus(window, cx);
        }
        cx.notify();
    }

    // ---- directory lookup and photos ----

    fn search_directory(&mut self, cx: &mut Context<Self>) {
        let Some(person) = self.selected.clone() else {
            return;
        };
        let query = self.directory_input.read(cx).value().trim().to_owned();
        if query.is_empty() {
            self.directory.message = Some("Type a name or email address to search.".into());
            cx.notify();
            return;
        }
        let Some(generation) = self.directory.start_search() else {
            cx.notify();
            return;
        };
        let cli = self.state.read(cx).cli().clone();
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let task = cx
            .background_executor()
            .spawn(async move { search_directory(&cli, &data_dir, &query) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                if view.directory.generation != generation
                    || view.selected.as_ref() != Some(&person)
                {
                    return;
                }
                view.directory.searching = false;
                match result {
                    Ok(search) => {
                        if search.results.is_empty() {
                            view.directory.message = Some("No directory matches.".into());
                        }
                        view.directory.search = Some(search);
                    }
                    Err(message) => view.directory.message = Some(message),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn link_directory_match(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (Some(person), Some(found)) = (
            self.selected.clone(),
            self.directory
                .search
                .as_ref()
                .and_then(|s| s.results.get(ix))
                .cloned(),
        ) else {
            return;
        };
        let Some(generation) = self.directory.start_link(ix) else {
            return;
        };
        let cli = self.state.read(cx).cli().clone();
        let lib = self.state.read(cx).library().clone();
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let added_email = found.email.clone();
        let target = person.clone();
        let task = cx
            .background_executor()
            .spawn(async move { link_directory_match(&cli, &lib, &data_dir, &target, &found) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                if view.directory.generation != generation
                    || view.selected.as_ref() != Some(&person)
                {
                    return;
                }
                view.directory.linking = None;
                view.directory.message = Some(match result {
                    Ok(outcome) => {
                        view.reload(cx);
                        format!(
                            "Added {}. {}",
                            added_email.as_deref().unwrap_or("email"),
                            photo_message(&outcome)
                        )
                    }
                    Err(message) => message,
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn load_photo_accounts(&mut self, cx: &mut Context<Self>) {
        if self.photo_accounts.loading || self.directory.busy() {
            return;
        }
        self.photo_accounts.loading = true;
        self.photo_accounts.error = None;
        self.photo_accounts.generation += 1;
        let generation = self.photo_accounts.generation;
        let cli = self.state.read(cx).cli().clone();
        let task = cx.background_executor().spawn(async move {
            let args = ["integrations", "status"].map(OsString::from);
            cli.run_blocking(&args, ChildKind::Command)
                .map_err(|error| {
                    failure_text("Couldn't check the photo accounts.", &error.user_text(), "")
                })
                .and_then(|value| parse_photo_accounts(&value))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.photo_accounts.generation != generation {
                    return;
                }
                this.photo_accounts.loading = false;
                match result {
                    Ok(choices) => {
                        if let Some(selected) = &this.photo_accounts.selected
                            && !choices
                                .iter()
                                .any(|choice| choice.source == selected.source && choice.available)
                        {
                            this.photo_accounts.selected = None;
                        }
                        this.photo_accounts.choices = choices;
                    }
                    Err(error) => {
                        this.photo_accounts.selected = None;
                        this.photo_accounts.choices.clear();
                        this.photo_accounts.error = Some(error);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn choose_photo_account(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.directory.busy() || self.photo_accounts.loading {
            return;
        }
        if let Some(choice) = self
            .photo_accounts
            .choices
            .get(index)
            .filter(|choice| choice.available)
        {
            self.photo_accounts.selected = Some(choice.clone());
            self.photo_accounts.error = None;
        }
        cx.notify();
    }

    fn request_photo(&mut self, cx: &mut Context<Self>) {
        let Some(person) = self.selected.clone() else {
            return;
        };
        let Some(source) = self
            .photo_accounts
            .selected
            .as_ref()
            .map(|choice| choice.source.clone())
        else {
            self.load_photo_accounts(cx);
            return;
        };
        let Some(generation) = self.directory.start_photo() else {
            return;
        };
        let cli = self.state.read(cx).cli().clone();
        let lib = self.state.read(cx).library().clone();
        let target = person.clone();
        let task = cx
            .background_executor()
            .spawn(async move { request_photo(&cli, &lib, &target, &source) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                if view.directory.generation != generation
                    || view.selected.as_ref() != Some(&person)
                {
                    return;
                }
                view.directory.photo_busy = false;
                view.directory.message = Some(match result {
                    Ok(outcome) => {
                        view.reload(cx);
                        photo_message(&outcome)
                    }
                    Err(message) => message,
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn load_detail(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        self.detail_generation += 1;
        let generation = self.detail_generation;
        self.detail_error = None;
        self.detail_loading = true;
        let app = self.state.read(cx);
        let lib = app.library().clone();
        let model = app.config().profile.speaker_embedding.clone();
        let thresholds = app.config().thresholds(&model).copied();
        let task = cx.background_executor().spawn({
            let id = id.clone();
            async move {
                let mut detail = load_detail(&lib, &id)?;
                detail.identity_checks = Some(
                    ops::identity_checks(&lib, Some(&id), &model, thresholds.as_ref())
                        .map_err(|error| core_user_text(&error)),
                );
                Ok(detail)
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                view.finish_detail(&id, generation, result, cx)
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// A read of `id`'s detail finished. Only the newest read of the person
    /// still selected lands; an open editor keeps its baseline and only
    /// learns which fields changed elsewhere.
    fn finish_detail(
        &mut self,
        id: &PersonId,
        generation: u64,
        result: Result<PersonDetail>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation || self.selected.as_ref() != Some(id) {
            return;
        }
        self.detail_loading = false;
        match result {
            Ok(detail) => {
                if let Some(editor) = self.editor.as_mut()
                    && let Some(baseline) = &editor.baseline
                    && baseline.person.id == *id
                {
                    editor.external = changed_fields(&baseline.revision, &detail.revision);
                }
                self.detail = Some(detail);
                self.detail_error = None;
            }
            Err(Error::NotFound(_)) => {
                // Forgotten or deleted elsewhere: nothing of theirs can be
                // saved without re-creating them.
                if !self.adding_person() && self.editor.take().is_some() {
                    self.message = Some(
                        "This person was forgotten or deleted elsewhere; your unsaved \
                         changes couldn't be kept."
                            .into(),
                    );
                }
                self.selected = None;
                self.detail = None;
                self.detail_error = None;
                self.confirm = None;
            }
            Err(e) => {
                tracing::warn!(code = e.code(), "reading a person");
                self.detail_error = Some(core_user_text(&e))
            }
        }
        cx.notify();
    }

    // ---- editing ----

    fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = self.detail.as_ref().filter(|_| self.editor.is_none()) else {
            return;
        };
        let person = &detail.person;
        if self.forgetting.contains(&person.id) {
            return;
        }
        let baseline = PersonState {
            person: person.clone(),
            notes: detail.notes.clone(),
            revision: detail.revision.clone(),
        };
        self.open_editor(Some(baseline), window, cx);
    }

    fn adding_person(&self) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|editor| editor.baseline.is_none())
    }

    fn start_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_some() {
            return;
        }
        if self.embedded
            && crate::ui::PaneLayout::for_window(window) == crate::ui::PaneLayout::Single
        {
            self.single_detail = true;
            self.single_forward = false;
        }
        self.open_editor(None, window, cx);
    }

    fn open_editor(
        &mut self,
        baseline: Option<PersonState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name_value, aliases_value, emails_value, notes_value) = baseline
            .as_ref()
            .map(|state| {
                (
                    state.person.name.clone(),
                    state.person.aliases.join(", "),
                    state.person.emails.join(", "),
                    state.notes.clone(),
                )
            })
            .unwrap_or_default();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Name")
                .default_value(name_value)
        });
        let aliases = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Other names, separated by commas")
                .default_value(aliases_value)
        });
        let emails = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Email addresses, separated by commas")
                .default_value(emails_value)
        });
        let notes = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Notes")
                .default_value(notes_value)
        });
        name.update(cx, |input, cx| input.focus(window, cx));
        self.confirm = None;
        self.message = None;
        self.record_after_forget = false;
        self.undo = None;
        self.editor_generation += 1;
        let mut subscriptions: Vec<_> = [&name, &aliases, &emails]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.save_edit(cx);
                    }
                })
            })
            .collect();
        let name_focus = name.focus_handle(cx);
        for focus in [
            name_focus.clone(),
            aliases.focus_handle(cx),
            emails.focus_handle(cx),
            notes.focus_handle(cx),
        ] {
            let handle = focus.clone();
            subscriptions.push(cx.on_focus(&focus, window, move |this, _, _| {
                if let Some(editor) = &mut this.editor {
                    editor.last_field = handle.clone();
                }
            }));
        }
        if self.single_detail
            && crate::ui::PaneLayout::for_window(window) == crate::ui::PaneLayout::Single
        {
            let focus = name_focus.clone();
            window.on_next_frame(move |window, cx| focus.focus(window, cx));
        }
        self.editor = Some(Editor {
            generation: self.editor_generation,
            baseline,
            external: Vec::new(),
            name,
            aliases,
            emails,
            notes,
            saving: false,
            name_error: None,
            last_field: name_focus,
            _subscriptions: subscriptions,
            error: None,
        });
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.as_ref().is_some_and(|editor| !editor.saving) {
            let adding = self.adding_person();
            self.editor = None;
            self.pending_selection = None;
            if adding {
                if self.embedded
                    && crate::ui::PaneLayout::for_window(window) == crate::ui::PaneLayout::Single
                {
                    self.single_detail = false;
                }
                let focus = self.add_focus.clone();
                window.on_next_frame(move |window, cx| focus.focus(window, cx));
            } else {
                self.edit_focus.focus(window, cx);
            }
            cx.notify();
        }
    }

    fn dismiss_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_selection.is_some() {
            self.resolve_selection(false, window, cx);
        } else {
            self.cancel_edit(window, cx);
        }
        cx.stop_propagation();
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        if editor.saving || self.pending_selection.is_some() {
            return;
        }
        let form = editor.form(cx);
        // The user's changes from what they started editing, never from a
        // reload meanwhile (that would revert changes made elsewhere).
        let Some(baseline) = editor.baseline.clone() else {
            self.save_new_person(form, cx);
            return;
        };
        let spec = match edit_spec(&baseline, &form) {
            Ok(spec) => spec,
            Err(message) => {
                if let Some(editor) = self.editor.as_mut() {
                    editor.name_error = Some(message);
                }
                cx.notify();
                return;
            }
        };
        if spec.fields().is_empty() {
            self.editor = None;
            cx.notify();
            return;
        }
        let cli = self.state.read(cx).cli().clone();
        let lib = self.state.read(cx).library().clone();
        let saved_name = form.name.trim().to_owned();
        let generation = editor.generation;
        if let Some(editor) = self.editor.as_mut() {
            editor.saving = true;
            editor.name_error = None;
            editor.error = None;
        }
        cx.notify();
        let person = baseline.person.id;
        let task = cx.background_executor().spawn({
            let person = person.clone();
            async move { save_person_edit(&cli, &lib, &person, &spec) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                view.finish_save(&person, generation, &saved_name, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn save_new_person(&mut self, form: PersonForm, cx: &mut Context<Self>) {
        let Some(editor) = self
            .editor
            .as_mut()
            .filter(|editor| editor.baseline.is_none())
        else {
            return;
        };
        let name = match broscribe_core::names::validate_name(&form.name) {
            Ok(name) => name.to_owned(),
            Err(e) => {
                editor.name_error = Some(core_user_text(&e));
                cx.notify();
                return;
            }
        };
        let spec = ops::NewPersonSpec {
            name,
            emails: split_list(&form.emails, &[',', ';', ' ', '\n', '\t']),
            notes: form.notes,
        };
        let cli = self.state.read(cx).cli().clone();
        let generation = editor.generation;
        editor.saving = true;
        editor.name_error = None;
        editor.error = None;
        let task = cli.run_with_stdin(
            crate::views::settings::args(["people", "add", "--spec-stdin"]),
            ChildKind::Command,
            serde_json::to_vec(&spec).expect("the person spec is serializable"),
            cx.background_executor(),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await.and_then(|v| {
                serde_json::from_value::<Person>(v["person"].clone()).map_err(|_| {
                    CliError::new("bad_output", "The CLI did not return the saved person.")
                })
            });
            this.update(cx, |view, cx| {
                let current = view
                    .editor
                    .as_ref()
                    .is_some_and(|e| e.generation == generation && e.baseline.is_none());
                match result {
                    Ok(person) => {
                        if current {
                            view.select(person.id, cx);
                            view.message = Some(format!("Added {}.", person.name));
                            view.message_kind = StatusKind::Done;
                        }
                        view.state.update(cx, |state, cx| {
                            state.library_changed(
                                &crate::watch::Changes {
                                    people: true,
                                    ..Default::default()
                                },
                                cx,
                            )
                        });
                        view.reload(cx);
                    }
                    Err(error) if current => {
                        if let Some(editor) = &mut view.editor {
                            editor.saving = false;
                            editor.error = Some(failure_text(
                                "Couldn't add the person.",
                                &error.user_text(),
                                "",
                            ));
                        }
                    }
                    Err(_) => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A save of `person` from editor `generation` finished. Only that editor,
    /// still open for that person, is closed or shown the refusal; another
    /// person's (or a newer) editor and its unsaved changes are left alone.
    fn finish_save(
        &mut self,
        person: &PersonId,
        generation: u64,
        saved_name: &str,
        result: std::result::Result<Option<String>, SaveRefusal>,
        cx: &mut Context<Self>,
    ) {
        let current = self.selected.as_ref() == Some(person)
            && self
                .editor
                .as_ref()
                .is_some_and(|e| e.generation == generation);
        match result {
            Ok(undo_id) => {
                if current {
                    self.editor = None;
                    let saved = format!("Saved {}.", saved_name);
                    self.message = Some(saved.clone());
                    self.message_kind = StatusKind::Done;
                    self.undo =
                        undo_id.map(|undo_id| Self::offer_undo(person.clone(), undo_id, saved, cx));
                }
                self.reload(cx);
            }
            Err(refusal) => {
                if current && let Some(editor) = self.editor.as_mut() {
                    editor.saving = false;
                    if let SaveRefusal::Conflict(fields) = &refusal {
                        for field in fields {
                            if !editor.external.contains(field) {
                                editor.external.push(*field);
                            }
                        }
                    }
                    editor.error = Some(refusal.message());
                }
            }
        }
        cx.notify();
    }

    /// Offers Undo for the edit `undo_id` for [`UNDO_WINDOW`].
    fn offer_undo(
        person: PersonId,
        undo_id: String,
        saved_message: String,
        cx: &mut Context<Self>,
    ) -> PersonUndo {
        let expiry = cx.spawn({
            let undo_id = undo_id.clone();
            async move |this, cx| {
                cx.background_executor().timer(UNDO_WINDOW).await;
                this.update(cx, |view, cx| {
                    view.expire_undo(&undo_id, cx);
                })
                .ok();
            }
        });
        PersonUndo {
            person,
            undo_id,
            running: false,
            saved_message,
            _expiry: expiry,
        }
    }

    fn expire_undo(&mut self, undo_id: &str, cx: &mut Context<Self>) {
        if let Some(undo) = self
            .undo
            .as_ref()
            .filter(|undo| undo.undo_id == undo_id && !undo.running)
        {
            if self.message_kind == StatusKind::Done
                && self.message.as_ref() == Some(&undo.saved_message)
            {
                self.message = None;
            }
            self.undo = None;
            cx.notify();
        }
    }

    /// Restores the values the last saved edit replaced, through `people
    /// undo-edit`; core refuses once those fields changed again or the
    /// person was forgotten, and never touches voice evidence.
    fn undo_edit(&mut self, cx: &mut Context<Self>) {
        let Some(undo) = self.undo.as_mut() else {
            return;
        };
        if undo.running {
            return;
        }
        undo.running = true;
        let person = undo.person.clone();
        let undo_id = undo.undo_id.clone();
        let cli = self.state.read(cx).cli().clone();
        let task = cx.background_executor().spawn({
            let person = person.clone();
            let undo_id = undo_id.clone();
            async move { undo_person_edit(&cli, &person, &undo_id) }
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                view.finish_undo(&person, &undo_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    /// The Undo `undo_id` of `person` finished: its offer ends either way,
    /// and the outcome shows only while that person is still shown.
    fn finish_undo(
        &mut self,
        person: &PersonId,
        undo_id: &str,
        result: std::result::Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .undo
            .as_ref()
            .is_some_and(|undo| undo.undo_id == undo_id && &undo.person == person);
        if current {
            self.undo = None;
        }
        if current && self.selected.as_ref() == Some(person) {
            self.message_kind = if result.is_ok() {
                StatusKind::Done
            } else {
                StatusKind::Failed
            };
            self.message = Some(match &result {
                Ok(()) => "Undone: the previous details are back.".to_owned(),
                Err(message) => message.clone(),
            });
        }
        if result.is_ok() {
            self.reload(cx);
        }
        cx.notify();
    }

    // ---- forgetting ----

    fn forgetting_selected(&self) -> bool {
        self.selected
            .as_ref()
            .is_some_and(|id| self.forgetting.contains(id))
    }

    fn ask_forget(&mut self, kind: ForgetKind, window: &mut Window, cx: &mut Context<Self>) {
        let opener = match kind {
            ForgetKind::Voice => &self.forget_voice_focus,
            ForgetKind::Person => &self.forget_person_focus,
        }
        .clone();
        self.ask_forget_from(kind, opener, window, cx);
    }

    fn ask_forget_from(
        &mut self,
        kind: ForgetKind,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.detail.as_ref().filter(|_| {
            self.confirm.is_none() && self.editor.is_none() && !self.forgetting_selected()
        }) else {
            return;
        };
        let target = detail.person.id.clone();
        let spec = self.forget_spec(kind, detail);
        self.message = None;
        self.record_after_forget = false;
        self.confirm = Some(kind);
        cx.notify();

        let view = cx.weak_entity();
        let cancel = view.clone();
        let model = view.clone();
        let target_on_cancel = target.clone();
        let target_on_draw = target.clone();
        kit::confirm_dialog(
            opener,
            window,
            cx,
            move |cx| {
                let view = model.upgrade()?;
                let view = view.read(cx);
                let pending = view.forgetting.contains(&target_on_draw);
                let mut spec = spec.clone();
                spec.action_enabled = !pending && view.selected.as_ref() == Some(&target_on_draw);
                spec.cancel_enabled = !pending;
                spec.status = pending.then(|| {
                    (
                        kit::confirm_line("forget-pending", "Removing the saved information…"),
                        kit::StatusKind::Pending,
                    )
                });
                Some(spec)
            },
            move |_, cx| {
                cancel
                    .update(cx, |view, cx| {
                        if view.forgetting.contains(&target_on_cancel) {
                            return false;
                        }
                        if view.selected.as_ref() == Some(&target_on_cancel) {
                            view.confirm = None;
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(true)
            },
            move |window, cx| {
                let _ = view.update(cx, |view, cx| {
                    if view.selected.as_ref() == Some(&target) && view.confirm == Some(kind) {
                        view.confirm_forget(window, cx);
                    }
                });
                false
            },
        );
    }

    fn confirm_forget(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(kind), Some(detail)) = (self.confirm, &self.detail) else {
            return;
        };
        if self.forgetting.contains(&detail.person.id) {
            return;
        }
        // The sample may be this person's enrollment file.
        self.playback.stop(&self.state, cx);
        let id = detail.person.id.clone();
        self.forgetting.insert(id.clone());
        let name = display_name(&detail.person);
        let is_self = detail.person.is_self;
        let lib = self.state.read(cx).library().clone();
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let task = cx.background_executor().spawn({
            let id = id.clone();
            async move { forget(&lib, &data_dir, &id, kind) }
        });
        cx.notify();
        let window = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                let current = view.selected.as_ref() == Some(&id) && view.confirm == Some(kind);
                let removed = current && kind == ForgetKind::Person && result.is_ok();
                view.finish_forget(&id, &name, kind, result, cx);
                if current && view.message_kind == StatusKind::Done {
                    view.record_after_forget = is_self;
                }
                if current {
                    let _ = window.update(cx, |_, window, cx| {
                        window.close_dialog(cx);
                        let layout = crate::ui::PaneLayout::for_window(window);
                        let focus = if removed {
                            Some(&view.focus_handle)
                        } else if kind == ForgetKind::Voice && view.detail_is_visible(layout) {
                            Some(&view.forget_voice_focus)
                        } else {
                            None
                        };
                        if let Some(focus) = focus {
                            focus.focus(window, cx);
                        }
                    });
                }
            })
            .ok();
        })
        .detach();
    }

    /// Forgetting `person` finished. Only their own confirmation, selection
    /// and detail are cleared; when the user has moved on to someone else,
    /// that person's selection, editor, draft and confirmation stay as they are.
    fn finish_forget(
        &mut self,
        person: &PersonId,
        name: &str,
        kind: ForgetKind,
        result: Result<Forgotten>,
        cx: &mut Context<Self>,
    ) {
        self.forgetting.remove(person);
        let still_shown = self.selected.as_ref() == Some(person);
        if still_shown {
            self.confirm = None;
        }
        let message_kind = if result.is_ok() {
            StatusKind::Done
        } else {
            StatusKind::Failed
        };
        let message = match &result {
            Ok(forgotten) => forget_message(name, kind, forgotten),
            Err(e) => failure_text(&format!("Couldn't forget {name}."), &core_user_text(e), ""),
        };
        if let Ok(Forgotten { outcome, .. }) = result {
            if self.undo.as_ref().is_some_and(|u| u.person == *person) {
                self.undo = None;
            }
            if kind == ForgetKind::Person {
                let previous_position = self
                    .visible_rows
                    .iter()
                    .position(|&index| &self.people[index].summary.id == person);
                self.people.retain(|row| &row.summary.id != person);
                self.people_index = person_picker::index_people(
                    self.people.iter().map(|row| row.summary.clone()).collect(),
                );
                self.filter_rows(cx);
                if still_shown {
                    self.selected = None;
                    self.detail = None;
                    self.detail_error = None;
                    self.detail_loading = false;
                    self.editor = None;
                    self.single_detail = false;
                    self.single_forward = false;
                    if let Some(&index) = previous_position.and_then(|position| {
                        self.visible_rows
                            .get(position)
                            .or_else(|| self.visible_rows.last())
                    }) {
                        self.select(self.people[index].summary.id.clone(), cx);
                        self.list_scroll.scroll_to_item(
                            previous_position.unwrap().min(self.visible_rows.len() - 1),
                        );
                    }
                }
            }
            self.reload(cx);
            self.state.update(cx, |state, cx| {
                let active =
                    broscribe_core::learning::job_id(&state.config().profile.speaker_embedding);
                let request = outcome
                    .learning
                    .iter()
                    .find(|request| request.job_id == active)
                    .or_else(|| outcome.learning.first());
                if let Some(request) = request {
                    self.forget_recheck = Some(request.clone());
                    state.recheck_requested(request.clone(), cx);
                }
                state.refresh_review_count(cx);
            });
        }
        if still_shown || self.selected.is_none() || self.message.is_none() {
            self.message = Some(message);
            self.message_kind = message_kind;
        }
        cx.notify();
    }

    // ---- rendering ----

    pub(super) fn import_contacts(&mut self, cx: &mut Context<Self>) {
        if self.importing_contacts {
            return;
        }
        self.importing_contacts = true;
        cx.notify();
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import contacts".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = rx
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .and_then(|paths| paths.into_iter().next());
            this.update(cx, |this, cx| {
                let Some(path) = path else {
                    this.importing_contacts = false;
                    cx.notify();
                    return;
                };
                let spec = match super::setup::people_file_spec(&path) {
                    Ok(spec) => spec,
                    Err(message) => {
                        this.import_contacts_result = Some(Err(message));
                        this.importing_contacts = false;
                        cx.notify();
                        return;
                    }
                };
                // The contacts file's path goes on stdin, never in argv.
                let task = this.state.read(cx).cli().run_with_stdin(
                    super::setup::people_file_args(),
                    ChildKind::Command,
                    spec,
                    cx.background_executor(),
                );
                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    this.update(cx, |this, cx| {
                        this.importing_contacts = false;
                        this.import_contacts_result = Some(match result {
                            Ok(v) => {
                                let count = v["people"]["contacts"].as_u64().unwrap_or(0);
                                this.reload(cx);
                                Ok(format!(
                                    "Imported {count} {} · {} new · {} updated",
                                    crate::ui::format::plural(count, "contact", "contacts"),
                                    v["people"]["created"].as_u64().unwrap_or(0),
                                    v["people"]["updated"].as_u64().unwrap_or(0)
                                ))
                            }
                            Err(e) => Err(failure_text(
                                "Couldn't import the contacts.",
                                &e.user_text(),
                                "",
                            )),
                        });
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            })
            .ok();
        })
        .detach();
    }

    fn contacts_import_control(&self, empty: bool, cx: &Context<Self>) -> impl IntoElement {
        const NOTE: &str = "Imports names and email addresses from a vCard (.vcf) or CSV file. Nothing else is read.";
        let action = Button::new("people-import-contacts")
            .label("Import contacts…")
            .outline()
            .tab_stop(true)
            .disabled(self.importing_contacts)
            .on_click(cx.listener(|this, _, _, cx| this.import_contacts(cx)));
        let controls = if empty && self.contacts_tip == Some(true) {
            kit::banner(
                "people-import-contacts-note",
                kit::StatusKind::Pending,
                Role::Note,
                NOTE,
                cx,
            )
            .child(
                super::settings::row().child(action).child(
                    Button::new("people-import-contacts-dismiss")
                        .label("Dismiss")
                        .ghost()
                        .tab_stop(true)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.contacts_tip = Some(false);
                            cx.notify();
                        })),
                ),
            )
        } else {
            kit::stack()
                .items_start()
                .gap_1()
                .child(action)
                .child(muted("people-import-contacts-note", NOTE, cx))
        };
        controls.when_some(self.import_contacts_result.as_ref(), |el, result| {
            el.child(match result {
                Ok(text) => kit::status_line(
                    "people-import-contacts-result",
                    kit::StatusKind::Done,
                    text.clone(),
                    cx,
                ),
                Err(text) => kit::status_line(
                    "people-import-contacts-result",
                    kit::StatusKind::Failed,
                    text.clone(),
                    cx,
                ),
            })
        })
    }

    fn on_list_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        if key.modifiers.control
            || key.modifiers.platform
            || key.modifiers.alt
            || self.visible_rows.is_empty()
            || self.pending_selection.is_some()
        {
            return;
        }
        let current = self
            .visible_rows
            .iter()
            .position(|&i| self.selected.as_ref() == Some(&self.people[i].summary.id));
        let last = self.visible_rows.len() - 1;
        let index = match key.key.as_str() {
            "up" => current.map_or(last, |i| i.saturating_sub(1)),
            "down" => current.map_or(0, |i| (i + 1).min(last)),
            "home" => 0,
            "end" => last,
            "enter" => {
                let person = self.people[self.visible_rows[current.unwrap_or(0)]]
                    .summary
                    .id
                    .clone();
                self.run_list_action(person, ListAction::Edit, window, cx);
                cx.stop_propagation();
                return;
            }
            _ => {
                let typed = key.key_char.as_deref().unwrap_or(&key.key);
                if typed.chars().count() != 1 || !typed.chars().all(char::is_alphabetic) {
                    return;
                }
                let now = Instant::now();
                let mut prefix = self
                    .type_select
                    .take()
                    .filter(|(at, _)| now.duration_since(*at) < TYPE_SELECT_WINDOW)
                    .map_or_else(String::new, |(_, prefix)| prefix);
                prefix.push_str(typed);
                let folded = broscribe_core::names::folded_name(&prefix);
                self.type_select = Some((now, prefix));
                let Some(index) = self.visible_rows.iter().position(|&i| {
                    broscribe_core::names::folded_name(&self.people_index[i].name)
                        .starts_with(&folded)
                }) else {
                    return;
                };
                index
            }
        };
        let person = self.people[self.visible_rows[index]].summary.id.clone();
        self.request_selection(person, false, window, cx);
        cx.stop_propagation();
    }

    fn run_list_action(
        &mut self,
        id: PersonId,
        action: ListAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_selection(id.clone(), matches!(action, ListAction::Edit), window, cx);
        if self.selected.as_ref() != Some(&id) || self.editor.is_some() {
            return;
        }
        if self.detail.is_some() {
            self.apply_list_action(action, window, cx);
        } else if self.detail_loading {
            self.list_action_on_load = Some(action);
        }
    }

    fn apply_list_action(
        &mut self,
        action: ListAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            ListAction::Edit => self.start_edit(window, cx),
            ListAction::Forget(kind) => {
                self.ask_forget_from(kind, self.focus_handle.clone(), window, cx)
            }
        }
    }

    fn close_row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(popup) = self.row_popup.take() {
            popup.restore_focus(window, cx);
            cx.notify();
        }
    }

    fn open_row_menu(
        &mut self,
        id: PersonId,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.weak_entity();
        let item = |label, action| {
            let id = id.clone();
            let view = view.clone();
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let _ = view.update(cx, |view, cx| {
                    view.close_row_menu(window, cx);
                    view.run_list_action(id.clone(), action, window, cx);
                });
            })
        };
        self.row_popup = Some(kit::RowPopup::open(
            position,
            &self.focus_handle,
            window,
            cx,
            |menu| {
                menu.item(item("Edit", ListAction::Edit))
                    .separator()
                    .item(item("Forget voice", ListAction::Forget(ForgetKind::Voice)))
                    .item(item(
                        "Forget person",
                        ListAction::Forget(ForgetKind::Person),
                    ))
            },
            Self::close_row_menu,
        ));
        cx.notify();
    }

    fn add_button(&self, cx: &Context<Self>) -> Button {
        button(
            "people-add",
            "Add",
            self.editor.is_none(),
            cx,
            Self::start_add,
        )
        .ghost()
        .small()
        .icon(gpui::assets::IconName::Plus)
        .accessibility_label("Add person…")
        .tooltip("Add person…")
        .track_focus(&self.add_focus)
        .debug_selector(|| "people-add".into())
    }

    fn render_list(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let query = self.search.read(cx).value();
        let focused = self.focus_handle.contains_focused(window, cx);
        let list = div()
            .id("people-list")
            .role(Role::ListBox)
            .aria_label("People")
            .track_focus(&self.focus_handle.clone().tab_index(0).tab_stop(true))
            .key_context("PeopleList")
            .on_action(cx.listener(|this, _: &kit::OpenRowMenu, window, cx| {
                let selected = this.selected.clone().and_then(|id| {
                    let row = this
                        .visible_rows
                        .iter()
                        .position(|&i| this.people[i].summary.id == id)?;
                    Some((id, this.list_scroll.bounds_for_item(row)?))
                });
                if let Some((id, row)) = selected {
                    let viewport = this.list_scroll.bounds();
                    let position = gpui::point(
                        row.left() + gpui::px(12.),
                        (row.bottom() + this.list_scroll.offset().y)
                            .clamp(viewport.top(), viewport.bottom()),
                    );
                    this.open_row_menu(id, position, window, cx);
                }
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ForgetSelectedPerson, window, cx| {
                if let Some(id) = this.selected.clone() {
                    this.run_list_action(id, ListAction::Forget(ForgetKind::Person), window, cx);
                }
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(Self::on_list_key))
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .children(
                self.visible_rows
                    .iter()
                    .enumerate()
                    .map(|(ix, &index)| self.person_row(ix, &self.people[index], focused, cx)),
            );
        kit::desk_card("people-list-pane", cx)
            .gap_2()
            .when_else(
                self.embedded,
                |list| list.w_full(),
                |list| list.w(gpui::rems(LIST_WIDTH)).flex_none(),
            )
            .h_full()
            .p_3()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        text_line(
                            "people-count",
                            Role::Label,
                            count(self.visible_rows.len() as u64, "person", "people"),
                        )
                        .text_role(TextRole::Caption)
                        .text_color(theme.muted_foreground),
                    )
                    .child(self.add_button(cx)),
            )
            .child(
                Input::new(&self.search)
                    .prefix(gpui::component::Icon::new(gpui::assets::IconName::Search))
                    .cleanable(true)
                    .aria_label("Search people"),
            )
            .child(self.contacts_import_control(false, cx))
            .when_some(self.list_error.as_ref(), |pane, error| {
                pane.child(kit::retry_line_with_id(
                    "people-list-error",
                    "people-list-retry",
                    people_load_failure(error),
                    cx,
                    |this, _, cx| this.reload(cx),
                ))
            })
            .when(self.people.is_empty() && self.list_loading, |pane| {
                pane.child(kit::status_line(
                    "people-list-loading",
                    StatusKind::Pending,
                    "Loading people…",
                    cx,
                ))
            })
            .when(
                !query.trim().is_empty() && self.visible_rows.is_empty() && !self.people.is_empty(),
                |pane| {
                    pane.child(text_line(
                        "people-none",
                        Role::Status,
                        format!("Nobody matches “{}”.", query.trim()),
                    ))
                },
            )
            .child(list)
            .children(
                self.row_popup
                    .as_ref()
                    .map(|popup| popup.render("people-row-menu", cx, Self::close_row_menu)),
            )
    }

    fn person_row(
        &self,
        ix: usize,
        row: &PersonRow,
        focused: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let person = &row.summary;
        let selected = self.selected.as_ref() == Some(&person.id);
        let name = person_picker::display_name(&person.name, person.is_self, &person.id);
        let email = person.emails.first().map_or("No email", String::as_str);
        let meetings = speaker_meetings(person.meetings);
        let samples = if row.voice_samples == 0 {
            "No voice yet".into()
        } else {
            count(row.voice_samples as u64, "voice sample", "voice samples")
        };
        let label = format!(
            "{name}{}, {email}, {meetings}, {samples}",
            if person.is_self { ", you" } else { "" }
        );
        let id = person.id.clone();
        let menu_id = id.clone();
        let metadata = kit::row_metadata_colour(selected, cx);
        kit::list_row(("person-row", ix), selected, focused, cx)
            .flex_shrink_0()
            .role(Role::ListBoxOption)
            .aria_label(label)
            .aria_selected(selected)
            .aria_position_in_set(ix + 1)
            .aria_size_of_set(self.visible_rows.len())
            .when(selected && focused, |row| row.aria_active_descendant())
            .items_center()
            .py_1()
            .tooltip(kit::title_tooltip(name.clone()))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.request_selection(id.clone(), true, window, cx);
                if this.pending_selection.is_none()
                    && (!this.single_detail
                        || crate::ui::PaneLayout::for_window(window)
                            != crate::ui::PaneLayout::Single)
                {
                    this.focus_handle.focus(window, cx);
                }
            }))
            .child(kit::avatar(
                &name,
                if person.is_self {
                    kit::Disc::Me
                } else {
                    kit::Disc::Person(person.hue)
                },
                self.photos.get(&person.id),
                32.,
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_role(TextRole::Label)
                                    .when(selected, |line| {
                                        line.font_weight(gpui::FontWeight::SEMIBOLD)
                                    })
                                    .child(name),
                            )
                            .when(person.is_self, |row| {
                                row.child(kit::badge(kit::BadgeTone::Neutral, "You", cx))
                            }),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_role(TextRole::Caption)
                            .text_color(metadata)
                            .child(email.to_owned()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_role(TextRole::Caption)
                            .text_color(metadata)
                            .child(format!("{meetings} · {samples}")),
                    ),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    this.open_row_menu(menu_id.clone(), event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
    }

    fn person_header(&self, id: &PersonId, cx: &Context<Self>) -> Option<impl IntoElement> {
        let summary = self
            .people_index
            .position(id)
            .and_then(|i| self.people.get(i))
            .map(|row| &row.summary);
        let detail = self.detail.as_ref();
        let (name, is_self, hue, email) = if let Some(detail) = detail {
            let person = &detail.person;
            (
                display_name(person),
                person.is_self,
                person.hue(),
                person.emails.first(),
            )
        } else {
            let person = summary?;
            (
                person_picker::display_name(&person.name, person.is_self, id),
                person.is_self,
                person.hue,
                person.emails.first(),
            )
        };
        let disc = if is_self {
            kit::Disc::Me
        } else {
            kit::Disc::Person(hue)
        };
        let mut facts = vec![email.cloned().unwrap_or_else(|| "No email".into())];
        if let Some(summary) = summary {
            facts.push(speaker_meetings(summary.meetings));
            facts.push(format!("{} of speech", length(summary.talk_s)));
        }
        Some(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(kit::avatar(&name, disc, self.photos.get(id), 64., cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    title("person-name", name.clone())
                                        .aria_level(if self.embedded { 2 } else { 1 }),
                                )
                                .when(is_self, |row| {
                                    row.child(kit::badge(kit::BadgeTone::Neutral, "You", cx))
                                }),
                        )
                        .child(
                            text_line("person-summary", Role::Label, facts.join(" · "))
                                .text_role(TextRole::Caption)
                                .text_color(cx.theme().muted_foreground),
                        ),
                )
                .child(
                    button(
                        "person-edit",
                        "Edit",
                        detail.is_some() && self.editor.is_none() && !self.forgetting_selected(),
                        cx,
                        |this: &mut Self, window, cx| this.start_edit(window, cx),
                    )
                    .track_focus(&self.edit_focus)
                    .outline()
                    .accessibility_label(format!("Edit {name}")),
                ),
        )
    }

    fn render_detail(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        if self.adding_person() {
            return kit::as_card(kit::stack().id("person-detail"), cx)
                .track_focus(&self.detail_focus)
                .flex_1()
                .min_w_0()
                .h_full()
                .p_4()
                .overflow_y_scroll()
                .child(heading("add-person-heading", "Add person"))
                .child(self.render_editor(cx))
                .into_any_element();
        }
        let pane = kit::as_card(kit::stack().id("person-detail"), cx)
            .track_focus(&self.detail_focus)
            .key_context("PeopleDetail")
            .on_action(cx.listener(|this, _: &Undo, _, cx| {
                if this.editor.is_none() && this.undo.is_some() {
                    this.undo_edit(cx);
                    cx.stop_propagation();
                } else {
                    cx.propagate();
                }
            }))
            .role(Role::Group)
            .aria_label("Person details")
            .gap(kit::GROUP_GAP)
            .flex_1()
            .min_w_0()
            .h_full()
            .p_4()
            .overflow_y_scroll()
            .when(!self.embedded && self.selected.is_none(), |pane| {
                pane.child(title("people-title", "People"))
            })
            .children(
                self.selected
                    .as_ref()
                    .and_then(|id| self.person_header(id, cx)),
            )
            .children(self.message.as_ref().map(|message| {
                let undo = self
                    .undo
                    .as_ref()
                    .filter(|undo| self.selected.as_ref() == Some(&undo.person));
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(kit::CONTROL_GAP)
                    .child(kit::status_line(
                        "people-message",
                        self.message_kind,
                        message.clone(),
                        cx,
                    ))
                    .children(undo.map(|undo| {
                        button(
                            "person-undo-edit",
                            if undo.running { "Undoing…" } else { "Undo" },
                            !undo.running,
                            cx,
                            |this: &mut Self, _, cx| this.undo_edit(cx),
                        )
                        .ghost()
                        .accessibility_label("Undo the last edit of this person")
                        .children(Kbd::binding_for_action_in(
                            &Undo,
                            &self.detail_focus,
                            window,
                        ))
                    }))
                    .when(self.record_after_forget, |line| {
                        line.child(self.record_voice_button("person-record-after-forget", cx))
                    })
            }))
            .children(self.recheck_panel(cx));
        let Some(detail) = &self.detail else {
            return if let Some(error) = &self.detail_error {
                pane.child(kit::retry_line(
                    "person-detail-state",
                    failure_text("Couldn't load this person.", error, ""),
                    cx,
                    |this, _, cx| this.load_detail(cx),
                ))
            } else if self.detail_loading {
                pane.child(kit::status_line(
                    "person-detail-loading",
                    StatusKind::Pending,
                    "Loading voice samples and meetings…",
                    cx,
                ))
            } else if self.people.is_empty() && self.list_loading {
                pane
            } else {
                pane.child(kit::empty_state(
                    "person-detail-empty-title",
                    "No person selected",
                    text_line(
                        "person-detail-state",
                        Role::Label,
                        "Select a person to see their voice samples and meetings.",
                    ),
                    false,
                    None,
                    None,
                    cx,
                ))
            }
            .into_any_element();
        };
        let person = &detail.person;
        let name = display_name(person);
        let mut pane = if self.editor.is_some() {
            pane.child(self.render_editor(cx))
        } else {
            pane.child(fact_row(
                "person-aliases",
                "Other names",
                person.aliases.join(", "),
                cx,
            ))
            .child(fact_row(
                "person-emails",
                "Emails",
                person.emails.join(", "),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .child(self.directory_popover(&name, cx)),
            )
            .child(fact_row(
                "person-notes",
                "Notes",
                detail.notes.trim().to_owned(),
                cx,
            ))
        };

        pane = pane.child(self.render_identity_checks(detail, cx));
        pane = pane.child(
            heading(
                "person-samples-heading",
                format!("Voice samples ({})", detail.samples.len()),
            )
            .mt_2(),
        );
        if detail.samples.is_empty() {
            let words = if person.is_self {
                "No voice samples yet. Record your voice to help BroScribe recognise you.".into()
            } else {
                format!(
                    "No voice samples yet. BroScribe learns {name}’s voice when you name them in a meeting."
                )
            };
            pane = pane
                .child(text_line("person-samples-none", Role::Label, words))
                .when(person.is_self && !self.record_after_forget, |pane| {
                    pane.child(self.record_voice_button("person-record-voice", cx))
                });
        } else {
            pane = pane.child(
                div()
                    .id("voice-samples")
                    .w_full()
                    .role(Role::List)
                    .aria_label("Voice samples")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(
                        detail
                            .samples
                            .iter()
                            .enumerate()
                            .map(|(ix, sample)| self.sample_row(ix, sample, cx)),
                    ),
            );
        }

        pane = pane.child(heading("person-meetings-heading", "Meetings").mt_2());
        if detail.meetings.is_empty() {
            pane = pane.child(muted(
                "person-meetings-none",
                "No meetings to show yet.",
                cx,
            ));
        } else {
            let id = person.id.clone();
            pane = pane
                .child(
                    div()
                        .id("person-meetings")
                        .role(Role::List)
                        .aria_label("Meetings")
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(
                            detail
                                .meetings
                                .iter()
                                .take(PERSON_MEETINGS_SHOWN)
                                .enumerate()
                                .map(|(ix, meeting)| {
                                    let title =
                                        meeting.title.as_deref().unwrap_or("Untitled meeting");
                                    let date =
                                        crate::ui::format::local_date_time(&meeting.started_at);
                                    let duration = length(meeting.duration_s);
                                    let route = Route::meeting(meeting.meeting_id.clone());
                                    kit::list_row(("person-meeting", ix), false, false, cx)
                                        .role(Role::ListItem)
                                        .tab_index(0)
                                        .debug_selector(move || format!("person-meeting-{ix}"))
                                        .aria_label(format!(
                                            "Open {title}, {}, {}",
                                            crate::ui::format::spoken_date_time(
                                                &meeting.started_at
                                            ),
                                            spoken_length(meeting.duration_s)
                                        ))
                                        .items_center()
                                        .tooltip(kit::title_tooltip(title.to_owned()))
                                        .on_click(cx.listener({
                                            let route = route.clone();
                                            move |this, _, _, cx| {
                                                this.state.update(cx, |state, cx| {
                                                    state.navigate(route.clone(), cx)
                                                })
                                            }
                                        }))
                                        .on_key_down(cx.listener(
                                            move |this, ev: &KeyDownEvent, _, cx| {
                                                if ev.keystroke.key == "enter" {
                                                    this.state.update(cx, |state, cx| {
                                                        state.navigate(route.clone(), cx)
                                                    });
                                                    cx.stop_propagation();
                                                }
                                            },
                                        ))
                                        .child(kit::row_body(
                                            title.to_owned(),
                                            Some(format!("{date} · {duration}")),
                                            false,
                                            cx,
                                        ))
                                }),
                        ),
                )
                .when(detail.meetings.len() > PERSON_MEETINGS_SHOWN, |pane| {
                    pane.child(
                        button(
                            "person-meetings-all",
                            format!("Show all {} meetings in Library", detail.meetings.len()),
                            true,
                            cx,
                            move |this: &mut Self, _, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.navigate(Route::LibraryPerson { person: id.clone() }, cx)
                                });
                            },
                        )
                        .link()
                        .debug_selector(|| "person-meetings-all".into()),
                    )
                });
        }
        pane.when(self.editor.is_none(), |pane| {
            pane.child(self.forget_section(&name, cx))
        })
        .into_any_element()
    }

    fn render_identity_checks(
        &self,
        detail: &PersonDetail,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let checks = detail.identity_checks.as_ref();
        let message = match checks {
            _ if self.detail_loading => "Checking confirmed identifications…".into(),
            Some(Ok(checks)) if !checks.alerts.is_empty() => count(
                checks.alerts.len() as u64,
                "confirmed name to check",
                "confirmed names to check",
            ),
            Some(Ok(checks)) if checks.compared > 0 => {
                "No mismatches found in the voices checked.".into()
            }
            Some(Ok(_)) => "No independent voice samples to compare yet.".into(),
            Some(Err(error)) => {
                failure_text("Couldn't check confirmed identifications.", error, "")
            }
            None => String::new(),
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(heading("person-identification-heading", "Identifications"))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        button(
                            "person-recheck-identifications",
                            "Recheck identifications",
                            !self.detail_loading,
                            cx,
                            |this: &mut Self, _, cx| this.load_detail(cx),
                        )
                        .outline(),
                    )
                    .when(
                        checks.is_some_and(|result| {
                            result
                                .as_ref()
                                .is_ok_and(|checks| !checks.alerts.is_empty())
                        }),
                        |el| {
                            el.child(
                                button(
                                    "person-open-identification-review",
                                    "Open Review",
                                    true,
                                    cx,
                                    |this: &mut Self, _, cx| {
                                        this.playback.stop(&this.state, cx);
                                        this.state.update(cx, |state, cx| {
                                            state.navigate(Route::ReviewQueue, cx)
                                        });
                                    },
                                )
                                .link(),
                            )
                        },
                    ),
            )
            .children((!message.is_empty()).then(|| {
                kit::status_line(
                    "person-identity-checks-status",
                    if self.detail_loading {
                        StatusKind::Pending
                    } else if checks.is_some_and(|checks| checks.is_err()) {
                        StatusKind::Failed
                    } else {
                        StatusKind::Info
                    },
                    message,
                    cx,
                )
            }))
            .children(
                checks
                    .and_then(|result| result.as_ref().ok())
                    .filter(|checks| checks.unreadable_meetings > 0)
                    .map(|checks| {
                        muted(
                            "person-identity-checks-incomplete",
                            count(
                                checks.unreadable_meetings as u64,
                                "meeting could not be checked",
                                "meetings could not be checked",
                            ),
                            cx,
                        )
                    }),
            )
    }

    fn recheck_panel(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        use crate::learning::{self, RecheckText};
        let request = self.forget_recheck.as_ref()?;
        let state = self.state.read(cx);
        let recheck = state.recheck();
        let current = recheck
            .status
            .as_ref()
            .filter(|status| status.job_id == request.job_id);
        let since = Some(request.generation);
        let failure = state
            .recheck_error()
            .map(|error| learning::recheck_failure(error, current.is_some()));
        let text = if let Some(status) = current {
            RecheckText {
                outcome: learning::recheck_outcome(status, request),
                lines: learning::recheck_lines(
                    status,
                    None,
                    since,
                    state
                        .jobs()
                        .active
                        .iter()
                        .find(|job| job.job_id == request.job_id.as_str())
                        .and_then(|job| job.message.as_deref()),
                ),
                links: learning::affected_links(recheck, since),
            }
        } else {
            RecheckText {
                lines: vec!["Checking other speakers…".into()],
                ..RecheckText::default()
            }
        };
        Some(learning::render_recheck(
            "people-recheck",
            text,
            failure,
            learning::recheck_phase(current, since),
            &self.state,
            |this, _, cx| {
                this.forget_recheck = None;
                cx.notify();
            },
            cx,
        ))
    }

    fn record_voice_button(&self, id: &'static str, cx: &Context<Self>) -> Button {
        button(
            id,
            "Record your voice",
            self.setup_deps.is_some(),
            cx,
            |this: &mut Self, _, cx| {
                if let Some(deps) = &this.setup_deps {
                    match crate::ui::show_setup_window(&this.state, deps, cx) {
                        Ok(()) => crate::ui::setup_window::request_step(
                            crate::views::setup::Step::Voice,
                            cx,
                        ),
                        Err(_) => {
                            this.message = Some("Couldn’t open voice setup. Try again.".into());
                            this.message_kind = StatusKind::Failed;
                            cx.notify();
                        }
                    }
                }
            },
        )
        .outline()
    }

    fn forget_section(&self, name: &str, cx: &Context<Self>) -> impl IntoElement {
        let notes = [
            (
                ForgetKind::Voice,
                "person-forget-voice",
                "person-forget-voice-note",
                "Forget voice",
                format!(
                    "Deletes {name}’s voice samples so BroScribe stops recognising them. Names in meetings stay."
                ),
                &self.forget_voice_focus,
            ),
            (
                ForgetKind::Person,
                "person-forget",
                "person-forget-person-note",
                "Forget person",
                format!(
                    "Deletes {name}, their notes and voice samples, and unnames their speakers."
                ),
                &self.forget_person_focus,
            ),
        ];
        kit::stack()
            .id("person-forget-section")
            .role(Role::Group)
            .aria_label("Forget")
            .gap_2()
            .child(heading("person-forget-heading", "Forget"))
            .children(
                notes
                    .into_iter()
                    .map(|(kind, id, note, label, words, focus)| {
                        kit::stack()
                            .gap_1()
                            .items_start()
                            .child(
                                button(
                                    id,
                                    label,
                                    !self.forgetting_selected(),
                                    cx,
                                    move |this: &mut Self, window, cx| {
                                        this.ask_forget(kind, window, cx);
                                    },
                                )
                                .track_focus(focus)
                                .danger()
                                .outline(),
                            )
                            .child(muted(note, words, cx))
                    }),
            )
    }

    fn sample_row(&self, ix: usize, sample: &VoiceSample, cx: &Context<Self>) -> impl IntoElement {
        use gpui::assets::IconName;
        let kind = match sample.kind {
            VoiceprintKind::Enrollment => "Your voice reading",
            VoiceprintKind::Confirmation => "Named by you",
            VoiceprintKind::LiveConfirmation => "Named while recording",
            VoiceprintKind::Hint => "From an import",
            VoiceprintKind::Auto => "Named automatically",
            VoiceprintKind::Captions => "Named from captions",
        };
        let title = sample.meeting_title.as_deref().unwrap_or(
            if sample.kind == VoiceprintKind::Enrollment {
                "Your voice reading"
            } else {
                "Untitled meeting"
            },
        );
        let weak = sample.strength == Strength::Weak;
        let mut facts = Vec::new();
        let mut spoken_facts = Vec::new();
        if title != kind {
            facts.push(kind.to_owned());
            spoken_facts.push(kind.to_owned());
        }
        if let Some(date) = &sample.meeting_date {
            facts.push(crate::ui::format::local_date(date));
            spoken_facts.push(crate::ui::format::spoken_local_date(date));
        }
        if let Some(clip) = &sample.clip {
            let duration = clip.end_s - clip.start_s;
            facts.push(length(duration));
            spoken_facts.push(spoken_length(duration));
        } else {
            facts.push("Audio no longer available".into());
            spoken_facts.push("Audio no longer available".into());
        }
        if weak {
            spoken_facts.push("Unconfirmed".into());
        }
        let spoken = std::iter::once(title.to_owned())
            .chain(spoken_facts)
            .collect::<Vec<_>>()
            .join(", ");
        let key: SharedString = format!("sample-{}", sample.id).into();
        let status = self.playback.status_of(&key);
        let pending = status.is_some_and(|words| matches!(words, LOADING | PREPARING_CACHE));
        let playing = self.playback.is_playing(&key) || pending;
        let play = sample.clip.clone().map(|clip| {
            let (label, icon) = if playing {
                ("Stop", IconName::Square)
            } else {
                ("Play", IconName::Play)
            };
            button(
                ("sample-play", ix),
                label,
                true,
                cx,
                move |this: &mut Self, _, cx| {
                    let state = this.state.clone();
                    this.playback
                        .toggle(key.clone(), clip.clone(), &state, cx, playback_of);
                },
            )
            .icon(icon)
            .ghost()
            .small()
            .accessibility_label(format!("{label} sample: {spoken}"))
            .debug_selector(move || format!("person-sample-play-{ix}"))
        });
        let title = if let Some(meeting) = &sample.meeting_id {
            let meeting = meeting.clone();
            button(
                ("sample-meeting", ix),
                title.to_owned(),
                true,
                cx,
                move |this: &mut Self, _, cx| {
                    this.state.update(cx, |state, cx| {
                        state.navigate(Route::meeting(meeting.clone()), cx)
                    });
                },
            )
            .link()
            .small()
            .into_any_element()
        } else {
            div()
                .truncate()
                .text_role(TextRole::Label)
                .child(title.to_owned())
                .into_any_element()
        };
        kit::list_row(("sample-row", ix), false, false, cx)
            .role(Role::ListItem)
            .aria_label(spoken)
            .items_start()
            .py_1()
            .when_some(sample.device.clone(), |row, device| {
                row.tooltip(kit::title_tooltip(device))
            })
            .children(play)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(title)
                            .when(weak, |row| {
                                row.child(kit::badge(kit::BadgeTone::Outline, "Unconfirmed", cx))
                            }),
                    )
                    .child(muted(("sample-facts", ix), facts.join(" · "), cx))
                    .children(status.map(|words| {
                        if pending {
                            kit::status_line(
                                ("sample-status", ix),
                                StatusKind::Pending,
                                words.to_owned(),
                                cx,
                            )
                            .into_any_element()
                        } else {
                            let clip = sample.clip.clone();
                            let key: SharedString = format!("sample-{}", sample.id).into();
                            kit::retry_line_with_id(
                                ("sample-status", ix),
                                ("sample-retry", ix),
                                words.to_owned(),
                                cx,
                                move |this: &mut Self, _, cx| {
                                    if let Some(clip) = &clip {
                                        this.playback.play(
                                            key.clone(),
                                            clip.clone(),
                                            &this.state,
                                            cx,
                                            playback_of,
                                        );
                                    }
                                },
                            )
                            .into_any_element()
                        }
                    })),
            )
    }

    fn directory_popover(&self, name: &str, cx: &Context<Self>) -> impl IntoElement {
        let person_name = name.to_owned();
        let name = name.to_owned();
        let view = cx.weak_entity();
        Popover::new("person-directory-popover")
            .anchor(gpui::Anchor::TopLeft)
            .trigger(
                Button::new("person-directory")
                    .label("Find in directory…")
                    .ghost()
                    .small()
                    .track_focus(&self.directory_focus)
                    .debug_selector(|| "person-directory".to_owned()),
            )
            .open(self.directory.open)
            .when(
                !self.directory.searching
                    && !self.directory.busy()
                    && self
                        .photo_accounts
                        .choices
                        .iter()
                        .any(|choice| choice.available),
                |popover| popover.track_focus(&self.directory_input.focus_handle(cx)),
            )
            .on_open_change(cx.listener(move |this, open, window, cx| {
                this.directory.open = *open;
                if *open {
                    this.directory_input.update(cx, |input, cx| {
                        input.set_value(person_name.clone(), window, cx);
                    });
                    this.load_photo_accounts(cx);
                } else {
                    this.directory_focus.focus(window, cx);
                }
                cx.notify();
            }))
            .content(move |_, _, cx| {
                view.update(cx, |this, cx| {
                    this.render_directory(&name, cx).into_any_element()
                })
                .unwrap_or_else(|_| div().into_any_element())
            })
    }

    /// Directory calls remain bound to the person and the signed-in account.
    fn render_directory(&self, name: &str, cx: &Context<Self>) -> impl IntoElement {
        let dir = &self.directory;
        let accounts = &self.photo_accounts;
        let enabled = !dir.searching && !dir.busy();
        let options = accounts
            .choices
            .iter()
            .map(|choice| {
                let label = if choice.available {
                    choice.label.clone()
                } else {
                    format!("{} (unavailable)", choice.label)
                };
                let mut item =
                    crate::ui::select::OptionItem::new(choice.source.selection_key(), label);
                item.disabled = !choice.available;
                item
            })
            .collect();
        let selected = accounts
            .selected
            .as_ref()
            .map(|choice| choice.source.selection_key())
            .unwrap_or_default();
        let has_choice = !selected.is_empty();
        let picker = crate::ui::select::Picker::new(
            "person-photo-account",
            "Photo from",
            options,
            selected,
            cx,
            |this: &mut Self, value, _, cx| {
                if value.is_empty() {
                    this.photo_accounts.selected = None;
                    cx.notify();
                } else if let Some(index) = this
                    .photo_accounts
                    .choices
                    .iter()
                    .position(|choice| choice.source.selection_key() == value)
                {
                    this.choose_photo_account(index, cx);
                }
            },
        )
        .clearable("Choose an account")
        .disabled(accounts.loading || dir.busy());
        let no_accounts = !accounts.loading
            && accounts.error.is_none()
            && !accounts.choices.iter().any(|choice| choice.available);
        div()
            .id("person-directory-content")
            .role(Role::Group)
            .aria_label("Work directory")
            .w(gpui::rems(25.))
            .max_h(gpui::rems(27.5))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .child(heading("person-directory-heading", "Work directory"))
            .when(no_accounts, |section| {
                section
                    .child(text_line(
                        "person-directory-none",
                        Role::Label,
                        "Connect a Microsoft 365 or Google account to search your work directory.",
                    ))
                    .child(
                        button(
                            "person-directory-integrations",
                            "Connect an account…",
                            true,
                            cx,
                            |this: &mut Self, _, cx| {
                                this.state.update(cx, |_, cx| {
                                    cx.emit(crate::state::AppEvent::OpenSettings(Some(
                                        crate::state::SettingsPage::Calendars,
                                    )))
                                });
                            },
                        )
                        .link(),
                    )
            })
            .when(!no_accounts, |section| {
                section.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&self.directory_input)
                                    .aria_label("Search the directory")
                                    .disabled(!enabled),
                            ),
                        )
                        .child(
                            button(
                                "person-directory-search",
                                if dir.searching {
                                    "Searching…"
                                } else {
                                    "Search"
                                },
                                enabled,
                                cx,
                                |this: &mut Self, _, cx| this.search_directory(cx),
                            )
                            .small(),
                        ),
                )
            })
            .children(
                dir.search
                    .as_ref()
                    .map(|search| self.directory_results(search, name, cx)),
            )
            .children(dir.message.as_ref().map(|message| {
                kit::status_line(
                    "person-directory-status",
                    StatusKind::Info,
                    message.clone(),
                    cx,
                )
            }))
            .children(accounts.error.as_ref().map(|error| {
                kit::status_line("photo-account-error", StatusKind::Failed, error.clone(), cx)
            }))
            .child(text_line("person-photo-label", Role::Label, "Photo from"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().min_w_0().flex_1().child(picker))
                    .child(
                        button(
                            "person-photo-accounts",
                            "Refresh",
                            !accounts.loading && !dir.busy(),
                            cx,
                            |this: &mut Self, _, cx| this.load_photo_accounts(cx),
                        )
                        .icon(gpui::assets::IconName::RefreshCw)
                        .ghost()
                        .small(),
                    ),
            )
            .child(
                button(
                    "person-photo",
                    if dir.photo_busy {
                        "Getting photo…"
                    } else {
                        "Get photo"
                    },
                    has_choice && enabled && !accounts.loading,
                    cx,
                    |this: &mut Self, _, cx| this.request_photo(cx),
                )
                .accessibility_label(format!("Get {name}'s photo from the directory"))
                .debug_selector(|| "person-photo".to_owned())
                .outline(),
            )
            .when(!has_choice, |section| {
                section.child(muted("person-photo-reason", "Choose an account first", cx))
            })
    }

    fn directory_results(
        &self,
        search: &DirectorySearch,
        name: &str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("person-directory-results")
            .role(Role::List)
            .aria_label("Directory results")
            .flex()
            .flex_col()
            .gap_1()
            .children(search.results.iter().enumerate().map(|(ix, found)| {
                let who = found
                    .name
                    .as_deref()
                    .or(found.email.as_deref())
                    .unwrap_or("Unnamed");
                let source = found.source.as_ref().map_or(PhotoSource::Google, |source| {
                    PhotoSource::Microsoft(source.account.clone())
                });
                let origin = self
                    .photo_accounts
                    .choices
                    .iter()
                    .find(|choice| choice.source == source)
                    .map_or_else(
                        || {
                            match found.integration {
                                IntegrationId::MicrosoftGraph => "Microsoft",
                                IntegrationId::Google => "Google",
                                IntegrationId::SystemCalendar => "System Calendar",
                            }
                            .to_owned()
                        },
                        |choice| choice.label.clone(),
                    );
                let text = match &found.email {
                    Some(email) if found.name.is_some() => format!("{who} · {email} · {origin}"),
                    _ => format!("{who} · {origin}"),
                };
                div()
                    .id(("person-directory-result", ix))
                    .role(Role::ListItem)
                    .aria_label(text.clone())
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().text_role(TextRole::Body).child(text))
                    .map(|row| match &found.email {
                        Some(email) => row.child(
                            button(
                                ("person-directory-link", ix),
                                if self.directory.linking == Some(ix) {
                                    "Adding…"
                                } else {
                                    "Add email"
                                },
                                !self.directory.busy(),
                                cx,
                                move |this: &mut Self, _, cx| this.link_directory_match(ix, cx),
                            )
                            .outline()
                            .small()
                            .tooltip(format!("Add {email} to {name} and get their photo")),
                        ),
                        None => row.child(muted(("person-directory-no-email", ix), "No email", cx)),
                    })
            }))
            .when(
                search.results.is_empty() && search.errors.is_empty(),
                |rows| {
                    rows.child(text_line(
                        "person-directory-no-results",
                        Role::Label,
                        "No directory matches.",
                    ))
                },
            )
            .children(search.errors.iter().enumerate().map(|(ix, error)| {
                kit::status_line(
                    ("person-directory-error", ix),
                    StatusKind::Failed,
                    error.clone(),
                    cx,
                )
            }))
    }

    fn render_editor(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(editor) = &self.editor else {
            return div().into_any_element();
        };
        let enabled = !editor.saving && self.pending_selection.is_none();
        let actions = kit::confirm_actions(
            button(
                "person-cancel-edit",
                "Cancel",
                enabled,
                cx,
                |this: &mut Self, window, cx| this.cancel_edit(window, cx),
            )
            .outline(),
            button(
                "person-save",
                if editor.saving { "Saving…" } else { "Save" },
                enabled,
                cx,
                |this: &mut Self, _, cx| this.save_edit(cx),
            )
            .map(|button| kit::primary(button, !enabled, cx)),
            cx,
        );
        div()
            .id("person-editor")
            .role(Role::Form)
            .aria_label(if editor.baseline.is_some() {
                "Edit person"
            } else {
                "Add person"
            })
            .key_context("PersonEditor")
            .flex()
            .flex_col()
            .gap_2()
            .on_action(cx.listener(|this, _: &SavePerson, _, cx| {
                this.save_edit(cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &CancelPersonEdit, window, cx| {
                this.dismiss_editor(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &gpui::base::actions::Cancel, window, cx| {
                    this.dismiss_editor(window, cx);
                }),
            )
            .child(kit::field(
                "person-name-label",
                "Name",
                None,
                |label| {
                    Input::new(&editor.name)
                        .aria_label(label)
                        .disabled(!enabled)
                },
                cx,
            ))
            .children(editor.name_error.as_ref().map(|message| {
                kit::status_line("person-name-error", StatusKind::Failed, message.clone(), cx)
            }))
            .when(editor.baseline.is_some(), |form| {
                form.child(kit::field(
                    "person-aliases-label",
                    "Other names",
                    Some(("person-aliases-help".into(), "Separate with commas".into())),
                    |label| {
                        Input::new(&editor.aliases)
                            .aria_label(label)
                            .disabled(!enabled)
                    },
                    cx,
                ))
            })
            .child(kit::field(
                "person-emails-label",
                "Emails",
                Some(("person-emails-help".into(), "Separate with commas".into())),
                |label| {
                    Input::new(&editor.emails)
                        .aria_label(label)
                        .disabled(!enabled)
                },
                cx,
            ))
            .child(kit::field(
                "person-notes-label",
                "Notes",
                None,
                |label| {
                    Textarea::new(&editor.notes)
                        .h(gpui::rems(7.5))
                        .aria_label(label)
                        .disabled(!enabled)
                },
                cx,
            ))
            .children(external_change_notice(&editor.external).map(|notice| {
                kit::status_line("person-edit-external", StatusKind::Attention, notice, cx)
            }))
            .child(actions)
            .when(editor.saving, |form| {
                form.child(kit::status_line(
                    "person-save-status",
                    StatusKind::Pending,
                    format!("Saving {}…", editor.name.read(cx).value().trim()),
                    cx,
                ))
            })
            .children(editor.error.as_ref().map(|message| {
                kit::status_line("person-edit-error", StatusKind::Failed, message.clone(), cx)
            }))
            .when(self.pending_selection.is_some(), |form| {
                let words = editor.baseline.as_ref().map_or_else(
                    || "Discard the new person?".to_owned(),
                    |baseline| {
                        format!(
                            "Discard your changes to {}?",
                            display_name(&baseline.person)
                        )
                    },
                );
                let fields = field_list(&editor.dirty_fields(cx));
                form.child(
                    div()
                        .id("person-discard")
                        .role(Role::AlertDialog)
                        .aria_label(words.clone())
                        .child(
                            kit::confirm_box(kit::ConfirmTone::Warning, cx)
                                .child(heading("person-discard-heading", words))
                                .child(text_line(
                                    "person-discard-fields",
                                    Role::Label,
                                    format!("Your edits to {fields} aren’t saved."),
                                ))
                                .child(kit::confirm_actions(
                                    button(
                                        "person-keep-editing",
                                        "Keep editing",
                                        true,
                                        cx,
                                        |this: &mut Self, window, cx| {
                                            this.resolve_selection(false, window, cx)
                                        },
                                    )
                                    .key_context("PersonDiscard")
                                    .on_action(cx.listener(
                                        |this, _: &KeepEditingPerson, window, cx| {
                                            this.resolve_selection(false, window, cx);
                                            cx.stop_propagation();
                                        },
                                    ))
                                    .track_focus(&self.discard_focus)
                                    .map(|button| kit::primary(button, false, cx)),
                                    button(
                                        "person-discard-changes",
                                        "Discard changes",
                                        true,
                                        cx,
                                        |this: &mut Self, window, cx| {
                                            this.resolve_selection(true, window, cx)
                                        },
                                    )
                                    .outline(),
                                    cx,
                                )),
                        ),
                )
            })
            .into_any_element()
    }

    fn forget_spec(&self, kind: ForgetKind, detail: &PersonDetail) -> kit::ConfirmSpec {
        use kit::confirm_line as line;
        let name = display_name(&detail.person);
        let samples = detail.samples.len();
        let is_self = detail.person.is_self;
        let (title, action) = match (kind, is_self) {
            (ForgetKind::Voice, true) => ("Forget your voice?".into(), "Forget voice"),
            (ForgetKind::Person, true) => ("Forget yourself?".into(), "Forget person"),
            (ForgetKind::Voice, false) => (format!("Forget {name}'s voice?"), "Forget voice"),
            (ForgetKind::Person, false) => (format!("Forget {name}?"), "Forget person"),
        };
        let mut spec = kit::ConfirmSpec::new(
            line("forget-confirm-title", title),
            "forget-confirm",
            "forget-confirm-footer",
            "forget-confirm-cancel",
            line("forget-confirm-yes", action),
        );
        let sample_line = (samples > 0).then(|| {
            line(
                "forget-goes-samples",
                format!(
                    "{}{}",
                    if is_self { "Your " } else { "" },
                    count(samples as u64, "voice sample", "voice samples")
                ),
            )
        });
        match kind {
            ForgetKind::Voice => {
                spec.goes.extend(sample_line);
                if detail
                    .samples
                    .iter()
                    .any(|sample| sample.kind == VoiceprintKind::Enrollment)
                {
                    spec.goes.push(line(
                        "forget-goes-reading",
                        if is_self {
                            "Your voice reading"
                        } else {
                            "Their voice reading"
                        },
                    ));
                }
                if is_self {
                    spec.goes.push(line(
                        "forget-goes-speaker-data",
                        "The voice data of speakers named as you",
                    ));
                    spec.changes.push(line("forget-changes-recognition",
                        "BroScribe stops recognising your voice in system audio and in imported recordings until you record your voice again."));
                    spec.stays = vec![
                        line("forget-stays-person", "Your name, notes and meetings"),
                        line(
                            "forget-stays-speaker-names",
                            "Me on your lines: the microphone track still marks them",
                        ),
                    ];
                } else {
                    spec.goes.push(line(
                        "forget-goes-speaker-data",
                        format!("The voice data of speakers named {name}"),
                    ));
                    spec.changes.push(line("forget-changes-recognition", format!("BroScribe stops recognising or suggesting {name} until you name them in a meeting again.")));
                    spec.stays = vec![
                        line(
                            "forget-stays-person",
                            format!("{name}, their notes and transcripts"),
                        ),
                        line(
                            "forget-stays-speaker-names",
                            format!("The name on every speaker already named {name}"),
                        ),
                    ];
                }
            }
            ForgetKind::Person => {
                spec.goes.push(line(
                    "forget-goes-person",
                    if is_self {
                        "Your person and notes".into()
                    } else {
                        format!("{name} and their notes")
                    },
                ));
                spec.goes.extend(sample_line);
                spec.goes
                    .push(line("forget-goes-photos", "Any cached directory photos"));
                let meetings = self
                    .people
                    .iter()
                    .find(|row| row.summary.id == detail.person.id)
                    .map_or(0, |row| row.summary.meetings);
                if meetings > 0 {
                    let count = crate::ui::format::count(meetings as u64, "meeting", "meetings");
                    spec.changes.push(line(
                        "forget-changes-speakers",
                        format!("Speakers named {name} in {count} become unnamed"),
                    ));
                }
                spec.changes.push(line(
                    "forget-changes-attendance",
                    if is_self {
                        "You leave participant lists".into()
                    } else {
                        format!("{name} leaves participant lists")
                    },
                ));
                spec.stays.push(line(
                    "forget-stays-meetings",
                    "The meetings, their transcripts and recordings",
                ));
                if is_self {
                    spec.stays.push(line(
                        "forget-stays-mic",
                        "New recordings still mark your microphone track as Me",
                    ));
                }
            }
        }
        spec
    }
}

fn revisions(state: &Entity<AppState>, cx: &App) -> (u64, u64) {
    let s = state.read(cx);
    (s.people_revision(), s.meetings_revision())
}

fn display_name(person: &Person) -> String {
    person_picker::display_name(&person.name, person.is_self, &person.id)
}

fn forget_message(name: &str, kind: ForgetKind, forgotten: &Forgotten) -> String {
    let speakers = crate::ui::format::count(
        forgotten.outcome.speakers_changed as u64,
        "speaker",
        "speakers",
    );
    let meetings = crate::ui::format::count(
        forgotten.outcome.meetings_changed.len() as u64,
        "meeting",
        "meetings",
    );
    let subject = match kind {
        ForgetKind::Voice => format!("{name}'s voice"),
        ForgetKind::Person => name.to_owned(),
    };
    let mut message = format!("Forgot {subject}; {speakers} in {meetings} updated.");
    if forgotten.photos_left {
        message = format!("{message} Cached directory photos of {name} couldn't all be removed.");
    }
    message
}

impl Focusable for PeopleView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PeopleView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.detail_loading
            && let Some(action) = self.list_action_on_load.take()
            && let Some(id) = self.detail.as_ref().map(|detail| detail.person.id.clone())
        {
            let view = cx.weak_entity();
            window.defer(cx, move |window, cx| {
                let _ = view.update(cx, |view, cx| {
                    if view.selected.as_ref() == Some(&id) {
                        view.apply_list_action(action, window, cx);
                    }
                });
            });
        }
        let theme = cx.theme();
        let page = div()
            .id("people")
            .role(Role::Main)
            .aria_label("People")
            .size_full()
            .flex()
            .text_color(theme.foreground);
        if self.people.is_empty()
            && !self.adding_person()
            && !self.list_loading
            && self.list_error.is_none()
            && self.selected.is_none()
            && self.detail.is_none()
        {
            if self.contacts_tip.is_none() {
                self.contacts_tip = Some(crate::ui::once::claim(
                    &self.state.read(cx).paths().data_dir,
                    "setup-contacts-tip",
                ));
            }
            return page.flex_col().p_4().track_focus(&self.focus_handle.clone().tab_index(0).tab_stop(true))
                .when(!self.embedded, |el| el.child(title("people-title", "People")))
                .child(kit::empty_state(
                    "people-empty-title", "No people yet",
                    crate::ui::text_line("people-list-empty", Role::Label,
                        "People appear when you name a speaker or import a meeting with participants."),
                    true, None, None, cx,
                ))
                .child(self.add_button(cx))
                .child(self.contacts_import_control(true, cx))
                .children(self.message.as_ref().map(|message| div().flex().items_center().gap_2()
                    .child(kit::status_line("people-message", self.message_kind, message.clone(), cx))
                    .when(self.record_after_forget, |line| line.child(self.record_voice_button("person-record-after-forget", cx)))))
                .children(self.recheck_panel(cx));
        }
        // The list and the person are two cards on the desk, 24px from the
        // window's edge.
        let pad = kit::DESK_PADDING;
        if !self.embedded {
            return page
                .flex_row()
                .gap_4()
                .p(pad)
                .child(self.render_list(window, cx))
                .child(self.render_detail(window, cx));
        }
        let layout = crate::ui::PaneLayout::for_window(window);
        let on_desk = |id: &'static str, pane: AnyElement| {
            div().id(id).size_full().flex().py(pad).child(pane)
        };
        let content = if layout != crate::ui::PaneLayout::Single {
            crate::ui::content_split(
                "people-split",
                on_desk(
                    "people-list-desk",
                    self.render_list(window, cx).into_any_element(),
                )
                .pl(pad)
                .pr_2()
                .into_any_element(),
                on_desk("people-detail-desk", self.render_detail(window, cx))
                    .pl_2()
                    .pr(pad)
                    .into_any_element(),
                window,
                cx,
            )
        } else if self.detail_is_visible(layout) {
            on_desk("people-detail-desk", self.render_detail(window, cx))
                .px(pad)
                .into_any_element()
        } else {
            on_desk(
                "people-list-desk",
                self.render_list(window, cx).into_any_element(),
            )
            .px(pad)
            .into_any_element()
        };
        page.child(content)
    }
}

#[cfg(test)]
#[path = "people/u47_tests.rs"]
mod u47_tests;

#[cfg(test)]
mod tests {
    use broscribe_core::schema::VoiceprintSource;
    use broscribe_platform::AudioPlayer as _;
    use broscribe_platform::fakes::FakeAudioPlayer;

    use super::*;

    #[cfg(unix)]
    #[gpui::test]
    fn photo_control_requires_an_explicit_account_and_keeps_errors_scoped_to_it(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let p = person();
        lib.write(|tx| tx.save_person(&p)).unwrap();
        let blocked = "11111111111111111111111111111111";
        let selected = "22222222222222222222222222222222";
        let account = |handle: &str, is_blocked: bool| {
            serde_json::json!({
                "account":{"integration":"microsoft-graph","account_id":handle},
                "identity":{"authority_host":"login.microsoftonline.com","resource_tenant_id":"synthetic-tenant",
                    "object_id":handle,"client_id":"synthetic-client","display_name":"Synthetic account","verified_addresses":[]},
                "generation":4,"selection_generation":1,"status":"active",
                "access_error":if is_blocked {Some("conditional_access_blocked")} else {None},
                "granted_scopes":["User.Read","User.ReadBasic.All"],"selected_calendars":[],
                "calendar":"not_requested","directory":"available","shared_calendars":"not_requested",
                "microsoft365_groups":"not_requested","target_results":[]
            })
        };
        let status_path = dir.path().join("status.json");
        std::fs::write(
            &status_path,
            serde_json::to_vec(&serde_json::json!({"integrations":[
                {"integration":"microsoft-graph","configured":true,"state":{"status":"signed_out"},
                    "microsoft_accounts":[account(blocked,true),account(selected,false)]},
                {"integration":"google","configured":true,"state":{"status":"active"}}
            ]}))
            .unwrap(),
        )
        .unwrap();
        let cli = dir.path().join("synthetic-cli");
        let requests = dir.path().join("photo-requests");
        let photo = dir.path().join("synthetic-photo.jpg");
        std::fs::write(&cli, format!(
            "#!/bin/sh\ncase \"$*\" in\n*\"integrations status\"*) cat '{}';;\n*\"people photo {} --account microsoft-graph/{selected}\"*) if [ -f '{}' ]; then printf '%s\\n' request >> '{}'; printf '%s\\n' '{{\"error\":{{\"code\":\"account_not_found\",\"message\":\"The selected synthetic account was removed; refresh photo accounts.\"}}}}'; exit 1; fi; printf '%s\\n' request >> '{}'; printf '%s\\n' '{{\"photos\":[{{\"path\":\"{}\"}}],\"errors\":[]}}';;\n*) printf '%s\\n' unexpected >> '{}'; printf '%s\\n' '{{\"error\":{{\"code\":\"unexpected_source\",\"message\":\"Photo source was not the explicitly selected account\"}}}}'; exit 1;;\nesac\n",
            status_path.display(), p.id, requests.display(), requests.display(), requests.display(), photo.display(), requests.display())).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let paths = broscribe_platform::resolve_paths(&broscribe_platform::fakes::FakePaths::new(
            dir.path(),
        ))
        .unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(paths, Default::default(), lib, cli),
                cx,
            )
        });
        cx.update(gpui::init);
        use crate::ui::kit::dialog_gate::{click, draw};
        let mut view = None;
        let window = cx.add_window(|window, cx| {
            let entity = cx.new(|cx| PeopleView::new(state, window, cx));
            entity.update(cx, |view, cx| view.select(p.id.clone(), cx));
            view = Some(entity.clone());
            gpui::component::Root::new(entity, window, cx)
        });
        let view = view.unwrap();
        cx.run_until_parked();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(1000.)));
        click(cx, "person-directory", Role::Button);
        assert!(!requests.exists(), "Opening the popover is read-only");
        assert!(cx.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            tree.read(tree.find("person-photo-reason")).1 == "Choose an account first"
        }));
        view.read_with(cx, |this, _| {
            assert!(
                this.photo_accounts.selected.is_none(),
                "No first-account default"
            );
            assert_eq!(this.photo_accounts.choices.len(), 3);
            assert!(!this.photo_accounts.choices[0].available);
            assert_eq!(
                this.photo_accounts.choices[2].source.args(&p.id),
                ["people", "photo", p.id.as_str()].map(OsString::from)
            );
        });
        assert!(!requests.exists(), "Opening the picker is read-only");
        click(cx, "person-photo-account", Role::ComboBox);
        cx.simulate_input("Microsoft account 2");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        draw(cx);
        view.read_with(cx, |this, _| {
            let choice = this.photo_accounts.selected.as_ref().unwrap();
            assert!(choice.label.starts_with("Microsoft account 2 · Synthetic account"));
            assert!(!choice.label.contains(selected));
            assert!(matches!(&choice.source, PhotoSource::Microsoft(account) if account.account_id.as_str() == selected));
        });
        assert!(
            !requests.exists(),
            "Choosing the source does not fetch a photo"
        );
        for expected in [
            "Photo updated.",
            "Couldn't get the photo. The selected synthetic account was removed; refresh photo \
             accounts.",
        ] {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let get = cx.debug_bounds("person-photo").unwrap();
            cx.simulate_click(get.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            view.read_with(cx, |this, _| {
                assert_eq!(this.directory.message.as_deref(), Some(expected))
            });
        }
        assert_eq!(
            std::fs::read_to_string(requests).unwrap(),
            "request\nrequest\n",
            "No implicit fallback to another account or automatic retry"
        );
    }

    fn person() -> Person {
        let mut p = Person::new(PersonId::random(), "Ana Lima");
        p.aliases = vec!["Ana".into()];
        p.emails = vec!["ana@example.org".into()];
        p
    }

    fn state_of(p: &Person, notes: &str) -> PersonState {
        PersonState {
            person: p.clone(),
            notes: notes.into(),
            revision: PersonRevision::of(p, notes).unwrap(),
        }
    }

    #[test]
    fn edit_spec_sends_only_changed_fields_fenced_and_on_stdin() {
        let p = person();
        let baseline = state_of(&p, "");
        let form = PersonForm {
            name: " Ana Lima ".into(),
            aliases: "ana, Aninha".into(),
            emails: "ana.l@example.org; ANA@example.org".into(),
            notes: "Prefers mornings.\n".into(),
        };
        let spec = edit_spec(&baseline, &form).unwrap();
        assert_eq!(spec.expected.as_ref(), Some(&baseline.revision));
        assert_eq!(spec.name, None);
        assert_eq!(spec.add_aliases, vec!["Aninha".to_owned()]);
        assert!(spec.remove_aliases.is_empty() && spec.aliases.is_none());
        assert_eq!(spec.add_emails, vec!["ana.l@example.org".to_owned()]);
        assert!(spec.remove_emails.is_empty() && spec.emails.is_none());
        assert_eq!(spec.notes.as_deref(), Some("Prefers mornings.\n"));
        assert_eq!(
            spec.fields().into_iter().collect::<Vec<_>>(),
            vec![
                PersonField::Aliases,
                PersonField::Emails,
                PersonField::Notes
            ]
        );
        // Notes and every value travel on stdin; argv names only the person.
        let args = edit_args(&p.id);
        assert_eq!(
            args,
            ["people", "edit", p.id.as_str(), "--spec-stdin"].map(OsString::from)
        );
        let stdin: serde_json::Value = serde_json::from_slice(&spec_stdin(&spec)).unwrap();
        assert_eq!(stdin["notes"], "Prefers mornings.\n");
        assert_eq!(stdin["expected"]["name"], baseline.revision.name.as_str());

        let removal = PersonForm {
            name: "Ana L.".into(),
            aliases: String::new(),
            emails: "ana@example.org".into(),
            notes: String::new(),
        };
        let spec = edit_spec(&baseline, &removal).unwrap();
        assert_eq!(spec.name.as_deref(), Some("Ana L."));
        assert_eq!(spec.remove_aliases, vec!["Ana".to_owned()]);
        assert_eq!(spec.notes, None);

        let unchanged = PersonForm {
            name: "Ana Lima".into(),
            aliases: "Ana".into(),
            emails: "ana@example.org".into(),
            notes: "same\n".into(),
        };
        assert!(
            edit_spec(&state_of(&p, "same"), &unchanged)
                .unwrap()
                .fields()
                .is_empty()
        );
        let empty = PersonForm::default();
        assert!(edit_spec(&baseline, &empty).is_err());
    }

    #[test]
    fn clips_follow_the_sample_source() {
        let p = person();
        let enrollment = VoiceprintSource {
            kind: VoiceprintKind::Enrollment,
            meeting_id: None,
            sha256: Some("ab".repeat(32)),
            start: 3.0,
            end: 7.5,
            ranges: Vec::new(),
            speaker_id: None,
            speaker_namespace: String::new(),
        };
        assert_eq!(
            clip_of(&p.id, &enrollment),
            Some(Clip {
                source: ClipSource::Enrollment {
                    person_id: p.id.clone(),
                    sha256: "ab".repeat(32),
                },
                start_s: 3.0,
                end_s: 7.5,
            })
        );
        let hint = VoiceprintSource {
            kind: VoiceprintKind::Hint,
            meeting_id: None,
            sha256: None,
            start: 0.0,
            end: 4.0,
            ranges: Vec::new(),
            speaker_id: None,
            speaker_namespace: String::new(),
        };
        assert_eq!(clip_of(&p.id, &hint), None);
        let empty_range = VoiceprintSource {
            kind: VoiceprintKind::Enrollment,
            meeting_id: None,
            sha256: Some("cd".repeat(32)),
            start: 2.0,
            end: 2.0,
            ranges: Vec::new(),
            speaker_id: None,
            speaker_namespace: String::new(),
        };
        assert_eq!(clip_of(&p.id, &empty_range), None);
    }

    #[test]
    fn plans_native_cache_or_preparation() {
        let player = FakeAudioPlayer::new(60.0);
        let mid = MeetingId::random();
        let clip = Clip {
            source: ClipSource::Meeting {
                meeting_id: mid.clone(),
                sha256: None,
            },
            start_s: 1.0,
            end_s: 4.0,
        };
        let cache = |exists| PlaybackCache {
            path: PathBuf::from("/cache/playback/x.wav"),
            exists,
            meeting_id: mid.clone(),
            sha256: "x".into(),
        };
        let native = ClipFiles::Found {
            audio: PathBuf::from("/lib/m/audio.m4a"),
            cache: Some(cache(false)),
            mix_required: false,
        };
        assert_eq!(
            plan_clip(native, &clip, |p| player.can_play_natively(p)),
            ClipPlan::Play {
                path: PathBuf::from("/lib/m/audio.m4a"),
                start_s: 1.0,
                end_s: 4.0,
            }
        );
        let cached = ClipFiles::Found {
            audio: PathBuf::from("/lib/m/audio.ogg"),
            cache: Some(cache(true)),
            mix_required: false,
        };
        assert_eq!(
            plan_clip(cached, &clip, |p| player.can_play_natively(p)),
            ClipPlan::Play {
                path: PathBuf::from("/cache/playback/x.wav"),
                start_s: 1.0,
                end_s: 4.0,
            }
        );
        let uncached = ClipFiles::Found {
            audio: PathBuf::from("/lib/m/audio.ogg"),
            cache: Some(cache(false)),
            mix_required: false,
        };
        assert_eq!(
            plan_clip(uncached, &clip, |p| player.can_play_natively(p)),
            ClipPlan::PrepareCache {
                meeting_id: mid.clone(),
                sha256: "x".into(),
            }
        );
        assert_eq!(
            plan_clip(ClipFiles::Unavailable("gone".into()), &clip, |p| player
                .can_play_natively(p)),
            ClipPlan::Unavailable("gone".into())
        );
    }

    #[test]
    fn missing_enrollment_file_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let id = lib.new_person_id();
        let source = ClipSource::Enrollment {
            person_id: id.clone(),
            sha256: "ef".repeat(32),
        };
        let cache_dir = dir.path().join("cache");
        assert!(matches!(
            locate_clip(&lib, &cache_dir, &source).unwrap(),
            ClipFiles::Unavailable(_)
        ));
        let wav = lib
            .person_dir(&id)
            .join(ENROLLMENT_DIR)
            .join(format!("{}.wav", "ef".repeat(32)));
        std::fs::create_dir_all(wav.parent().unwrap()).unwrap();
        std::fs::write(&wav, b"RIFF").unwrap();
        assert_eq!(
            locate_clip(&lib, &cache_dir, &source).unwrap(),
            ClipFiles::Found {
                audio: wav,
                cache: None,
                mix_required: false,
            }
        );
    }

    #[test]
    fn source_addressed_native_sample_requires_mix_cache() {
        let mid = MeetingId::random();
        let clip = Clip {
            source: ClipSource::Meeting {
                meeting_id: mid.clone(),
                sha256: None,
            },
            start_s: 1.0,
            end_s: 2.0,
        };
        let cache = PlaybackCache {
            path: "/cache/mix.wav".into(),
            exists: false,
            meeting_id: mid.clone(),
            sha256: "sha".into(),
        };
        let files = ClipFiles::Found {
            audio: "/external/source.m4a".into(),
            cache: Some(cache),
            mix_required: true,
        };
        assert_eq!(
            plan_clip(files, &clip, |_| true),
            ClipPlan::PrepareCache {
                meeting_id: mid,
                sha256: "sha".into()
            }
        );
    }

    #[gpui::test]
    fn sample_end_timer_stops_before_the_next_poll(cx: &mut gpui::TestAppContext) {
        struct Samples {
            playback: SamplePlayback,
        }
        fn field(view: &mut Samples) -> &mut SamplePlayback {
            &mut view.playback
        }
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let person = PersonId::random();
        let wav = lib.person_dir(&person).join(ENROLLMENT_DIR).join("sha.wav");
        std::fs::create_dir_all(wav.parent().unwrap()).unwrap();
        std::fs::write(&wav, b"synthetic").unwrap();
        let paths = broscribe_platform::resolve_paths(&broscribe_platform::fakes::FakePaths::new(
            dir.path(),
        ))
        .unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    paths,
                    Default::default(),
                    lib,
                    dir.path().join("unused-cli"),
                ),
                cx,
            )
        });
        state.update(cx, |state, _| {
            state.set_player(Box::new(FakeAudioPlayer::new(60.0)))
        });
        let view = cx.new(|_| Samples {
            playback: SamplePlayback::default(),
        });
        view.update(cx, |view, cx| {
            view.playback.play(
                "short".into(),
                Clip {
                    source: ClipSource::Enrollment {
                        person_id: person,
                        sha256: "sha".into(),
                    },
                    start_s: 1.0,
                    end_s: 1.1,
                },
                &state,
                cx,
                field,
            )
        });
        cx.run_until_parked();
        state.update(cx, |state, _| {
            assert!(state.player_mut().unwrap().is_playing())
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(101));
        cx.run_until_parked();
        state.update(cx, |state, _| {
            assert!(!state.player_mut().unwrap().is_playing())
        });
    }

    #[test]
    fn forget_voice_keeps_photos_and_forget_person_purges_them() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let person = person();
        lib.write(|tx| tx.save_person(&person)).unwrap();
        let enrollment = lib.person_dir(&person.id).join("enrollment");
        std::fs::create_dir_all(&enrollment).unwrap();
        std::fs::write(enrollment.join("synthetic.wav"), b"synthetic").unwrap();
        let photo = broscribe_integrations::account_dir(
            dir.path(),
            broscribe_integrations::IntegrationId::Google,
        )
        .join("photos")
        .join(format!("{}.img", person.id));
        std::fs::create_dir_all(photo.parent().unwrap()).unwrap();
        std::fs::write(&photo, b"synthetic").unwrap();
        let voice = forget(&lib, dir.path(), &person.id, ForgetKind::Voice).unwrap();
        assert!(!enrollment.exists());
        // Design §10: forgetting the voice leaves directory photos alone.
        assert!(photo.exists());
        assert!(!voice.photos_left);
        assert_eq!(lib.load_person(&person.id).unwrap().name, person.name);
        let forgotten = forget(&lib, dir.path(), &person.id, ForgetKind::Person).unwrap();
        assert!(!photo.exists());
        assert!(!forgotten.photos_left, "every store was purged");
        assert!(!forget_message("Synthetic", ForgetKind::Person, &forgotten).contains("photos"));
    }

    #[test]
    fn photo_purge_failure_after_commit_does_not_fail_the_forget() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let person = person();
        lib.write(|tx| tx.save_person(&person)).unwrap();
        // A directory where the photo file should be: removing it fails.
        let photo = broscribe_integrations::account_dir(
            dir.path(),
            broscribe_integrations::IntegrationId::Google,
        )
        .join("photos")
        .join(format!("{}.img", person.id));
        std::fs::create_dir_all(photo.join("blocker")).unwrap();
        // The purge reports that store as left behind, not as an error.
        let purge =
            broscribe_integrations::forget_person_photos(dir.path(), person.id.as_str()).unwrap();
        assert_eq!(
            purge.status,
            broscribe_integrations::PhotoPurgeStatus::Partial
        );
        assert_eq!(
            purge
                .failures
                .iter()
                .map(|f| f.integration)
                .collect::<Vec<_>>(),
            [IntegrationId::Google]
        );
        // The forget commits; the purge that follows it fails again and is
        // reported beside its success.
        let forgotten = forget(&lib, dir.path(), &person.id, ForgetKind::Person).unwrap();
        assert!(forgotten.outcome.person_deleted);
        assert!(
            lib.load_person(&person.id).is_err(),
            "the person is forgotten"
        );
        assert!(forgotten.photos_left);
        assert!(photo.exists(), "the blocked store keeps its entry");
        let message = forget_message("Synthetic", ForgetKind::Person, &forgotten);
        assert!(message.starts_with("Forgot Synthetic;"), "{message}");
        assert!(
            message.ends_with("Cached directory photos of Synthetic couldn't all be removed."),
            "{message}"
        );
    }

    fn found(generation: u64, email: Option<&str>) -> DirectoryMatch {
        DirectoryMatch {
            integration: IntegrationId::Google,
            generation,
            source: None,
            name: Some("Casey Example".into()),
            email: email.map(Into::into),
        }
    }

    #[test]
    fn people_directory_lookup_drops_results_of_changed_accounts() {
        let value = serde_json::json!({
            "results": [
                {"integration": "google", "person": {"remote_id": "a", "name": "Casey Example", "email": "casey@example.invalid"}},
                {"integration": "google", "person": {"remote_id": "b", "name": " ", "email": null}},
                {"integration": "microsoft-graph", "person": {"remote_id": "c", "name": "Robin Example", "email": "robin@example.invalid"}},
                {"integration": "unknown", "person": {"remote_id": "d"}}
            ],
            "errors": [{"integration": "microsoft-graph", "code": "transient", "message": "network error: synthetic"}]
        });
        let before = AccountGenerations::from([
            (IntegrationId::Google, 2),
            (IntegrationId::MicrosoftGraph, 1),
        ]);
        let after = AccountGenerations::from([
            (IntegrationId::Google, 2),
            (IntegrationId::MicrosoftGraph, 2),
        ]);
        let search = parse_lookup(&value, &before, &after);
        assert_eq!(
            search.results,
            [
                found(2, Some("casey@example.invalid")),
                DirectoryMatch {
                    integration: IntegrationId::Google,
                    generation: 2,
                    source: None,
                    name: None,
                    email: None,
                },
            ]
        );
        assert_eq!(search.errors, ["microsoft-graph: network error: synthetic"]);
    }

    #[test]
    fn corporate_directory_links_keep_the_exact_account_and_epoch() {
        let handles = [
            "11111111111111111111111111111111",
            "22222222222222222222222222222222",
        ];
        let value = serde_json::json!({"results": handles.iter().map(|handle| serde_json::json!({
            "integration":"microsoft-graph","person":{"name":"Synthetic","email":"synthetic@example.invalid",
                "source":{"account":{"integration":"microsoft-graph","account_id":handle},"generation":9}}
        })).collect::<Vec<_>>()});
        let results = parse_lookup(
            &value,
            &AccountGenerations::new(),
            &AccountGenerations::new(),
        )
        .results;
        assert_eq!(results.len(), 2);
        let person = person();
        for (found, handle) in results.iter().zip(handles) {
            assert_eq!(found.generation, 9);
            assert!(
                link_args(&person.id, found).contains(&OsString::from(format!(
                    "--expect-generation=microsoft-graph/{handle}:9"
                )))
            );
        }
        assert_ne!(results[0].source, results[1].source);
    }

    #[test]
    fn directory_error_labels_do_not_confuse_accounts_on_the_same_connector() {
        let failed = "11111111111111111111111111111111";
        let successful = "22222222222222222222222222222222";
        let output = serde_json::json!({
            "results":[{"integration":"microsoft-graph","person":{"name":"Synthetic","email":"synthetic@example.invalid",
                "source":{"account":{"integration":"microsoft-graph","account_id":successful},"generation":8}}}],
            "errors":[{"integration":"microsoft-graph","account":{"integration":"microsoft-graph","account_id":failed},
                "account_label":"Synthetic unavailable account","code":"forbidden","message":"Access denied"}]
        });
        let search = parse_lookup(
            &output,
            &AccountGenerations::new(),
            &AccountGenerations::new(),
        );
        assert_eq!(search.results.len(), 1);
        assert_eq!(
            search.results[0]
                .source
                .as_ref()
                .unwrap()
                .account
                .account_id
                .as_str(),
            successful
        );
        assert_eq!(search.results[0].generation, 8);
        assert_eq!(search.errors.len(), 1);
        assert!(search.errors[0].contains(&format!("microsoft-graph/{failed}")));
        assert!(search.errors[0].contains("Synthetic unavailable account"));
        assert!(!search.errors[0].contains(successful));
    }

    #[test]
    fn people_directory_link_adds_only_a_new_exact_email_and_is_fenced() {
        let p = person();
        assert_eq!(link_edit(&p, "ANA@example.org"), None);
        let edit = link_edit(&p, " casey@example.invalid ").unwrap();
        assert_eq!(edit.add_emails, ["casey@example.invalid"]);
        assert!(edit.remove_emails.is_empty() && edit.name.is_none() && edit.notes.is_none());
        let ok = found(3, Some("casey@example.invalid"));
        assert_eq!(link_refusal(&ok, 3, true), None);
        assert!(link_refusal(&ok, 3, false).unwrap().contains("forgotten"));
        assert!(
            link_refusal(&ok, 4, true)
                .unwrap()
                .contains("account changed")
        );
        assert!(
            link_refusal(&found(3, None), 3, true)
                .unwrap()
                .contains("no email")
        );
        // The child commits only for the search's account generation; the
        // email travels on stdin.
        assert_eq!(
            link_args(&p.id, &ok),
            [
                "people".to_owned(),
                "edit".to_owned(),
                p.id.as_str().to_owned(),
                "--spec-stdin".to_owned(),
                "--expect-generation=google:3".to_owned(),
            ]
            .map(OsString::from)
        );
        let stdin: serde_json::Value = serde_json::from_slice(&spec_stdin(&edit)).unwrap();
        assert_eq!(
            stdin,
            serde_json::json!({"add_emails": ["casey@example.invalid"]})
        );
    }

    #[test]
    fn people_directory_requests_check_person_and_account_before_any_network() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        // A missing CLI: any request that got past the fences would fail to spawn.
        let cli =
            crate::test_support::runner(dir.path().join("no-such-cli"), lib.root().to_path_buf());
        let mut without_email = person();
        without_email.emails.clear();
        lib.write(|tx| tx.save_person(&without_email)).unwrap();
        let photo = request_photo(&cli, &lib, &without_email.id, &PhotoSource::Google).unwrap_err();
        assert!(photo.contains("email"), "{photo}");
        let forgotten = PersonId::random();
        assert!(
            request_photo(&cli, &lib, &forgotten, &PhotoSource::Google)
                .unwrap_err()
                .contains("forgotten")
        );
        let data_dir = dir.path().join("data");
        let state = broscribe_integrations::account_dir(&data_dir, IntegrationId::Google)
            .join("state.json");
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &state,
            &broscribe_integrations::AccountState {
                generation: 5,
                ..Default::default()
            },
        )
        .unwrap();
        let stale = found(4, Some("casey@example.invalid"));
        let refused =
            link_directory_match(&cli, &lib, &data_dir, &without_email.id, &stale).unwrap_err();
        assert!(refused.contains("account changed"), "{refused}");
        let current = found(5, Some("casey@example.invalid"));
        let refused =
            link_directory_match(&cli, &lib, &data_dir, &forgotten, &current).unwrap_err();
        assert!(refused.contains("forgotten"), "{refused}");
        assert!(
            lib.load_person(&without_email.id)
                .unwrap()
                .emails
                .is_empty()
        );
    }

    /// Stages a cached Google directory photo of `person` as the integration
    /// keeps one: a current, active Google Workspace account issued to the
    /// configured client, with directory permission both granted and still
    /// requested. Returns its generation-scoped photo path.
    fn stage_google_photo(
        data_dir: &std::path::Path,
        person: &PersonId,
        bytes: &[u8],
    ) -> std::path::PathBuf {
        use broscribe_integrations::{AccountState, AccountStatus, account_dir};
        std::fs::write(
            broscribe_core::config::Config::path(data_dir),
            "[integration.google]\nclient_id = \"synthetic-client\"\nworkspace = true\n",
        )
        .unwrap();
        let account = account_dir(data_dir, IntegrationId::Google);
        std::fs::create_dir_all(&account).unwrap();
        let state = AccountState {
            generation: 3,
            status: AccountStatus::Active,
            client_id: Some("synthetic-client".into()),
            scopes: vec![broscribe_integrations::config::GOOGLE_DIRECTORY_READONLY.into()],
            ..Default::default()
        };
        std::fs::write(
            account.join("state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let path = account
            .join("photos")
            .join(state.generation.to_string())
            .join(format!("{person}.img"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn people_photo_outcome_and_cached_photos_for_display() {
        let outcome = parse_photo(&serde_json::json!({
            "person_id": "p-synthetic",
            "photos": [{"integration": "google", "path": "/synthetic/p.img"}],
            "errors": [{"integration": "microsoft-graph", "message": "access denied"}]
        }));
        assert_eq!(outcome.photo, Some(PathBuf::from("/synthetic/p.img")));
        assert_eq!(
            photo_message(&outcome),
            "Photo updated. microsoft-graph: access denied"
        );
        assert!(photo_message(&PhotoOutcome::default()).starts_with("The selected account"));
        let dir = tempfile::tempdir().unwrap();
        let with_photo = person();
        let without = Person::new(PersonId::random(), "Robin Example");
        stage_google_photo(dir.path(), &with_photo.id, b"\x89PNG\r\n\x1a\nsynthetic-1");
        let shown = cached_photos(dir.path(), [&with_photo.id, &without.id]);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[&with_photo.id].format, ImageFormat::Png);
        // Unknown bytes are not shown.
        assert!(photo_image(&dir.path().join("missing.img")).is_none());
        std::fs::write(dir.path().join("text.img"), b"synthetic").unwrap();
        assert!(photo_image(&dir.path().join("text.img")).is_none());
    }

    /// GPUI caches images by source: a refreshed photo at the same path must
    /// get a new source, and sign-out must drop what was shown.
    #[test]
    fn people_photo_source_follows_the_bytes_and_sign_out_clears_it() {
        use broscribe_platform::fakes::{FakeBrowserLauncher, FakeSecretStore};
        let dir = tempfile::tempdir().unwrap();
        let p = person();
        let path = stage_google_photo(dir.path(), &p.id, &[0xFF, 0xD8, 0xFF, 0xE0, 1]);
        let first = cached_photos(dir.path(), [&p.id]);
        let unchanged = cached_photos(dir.path(), [&p.id]);
        assert_eq!(first[&p.id].id(), unchanged[&p.id].id());
        assert!(dropped_photos(&first, &unchanged).is_empty());
        std::fs::write(&path, [0xFF, 0xD8, 0xFF, 0xE0, 2]).unwrap();
        let refreshed = cached_photos(dir.path(), [&p.id]);
        assert_ne!(first[&p.id].id(), refreshed[&p.id].id());
        let dropped = dropped_photos(&first, &refreshed);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].id(), first[&p.id].id());
        broscribe_integrations::Integrations::new(
            dir.path(),
            broscribe_integrations::IntegrationsConfig::default(),
            Arc::new(FakeSecretStore::default()),
            Arc::new(FakeBrowserLauncher::default()),
            Arc::new(|_| {}),
        )
        .unwrap()
        .logout(IntegrationId::Google)
        .unwrap();
        let signed_out = cached_photos(dir.path(), [&p.id]);
        assert!(signed_out.is_empty());
        assert_eq!(dropped_photos(&refreshed, &signed_out).len(), 1);
    }

    #[test]
    fn people_directory_search_waits_for_link_and_photo_requests() {
        let mut pane = DirectoryPane::default();
        let first = pane.start_search().unwrap();
        pane.searching = false;
        let link = pane.start_link(0).unwrap();
        assert_eq!(link, first);
        // Search while linking: refused, the link keeps its generation and state.
        assert_eq!(pane.start_search(), None);
        assert_eq!((pane.generation, pane.linking), (link, Some(0)));
        assert!(pane.message.as_deref().unwrap().contains("Wait"));
        assert_eq!(pane.start_photo(), None);
        pane.linking = None;
        let photo = pane.start_photo().unwrap();
        assert_eq!(pane.start_search(), None);
        assert_eq!(pane.start_link(1), None);
        assert_eq!((pane.generation, pane.photo_busy), (photo, true));
        pane.photo_busy = false;
        assert!(pane.start_search().unwrap() > photo);
        // Another person invalidates everything in flight.
        pane.start_photo().unwrap();
        pane.reset();
        assert!(!pane.busy() && !pane.searching);
    }

    #[gpui::test]
    fn save_completion_leaves_another_persons_editor_alone(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let a = person();
        let b = Person::new(PersonId::random(), "Bea Example");
        lib.write(|tx| {
            tx.save_person(&a)?;
            tx.save_person(&b)
        })
        .unwrap();
        let paths = broscribe_platform::resolve_paths(&broscribe_platform::fakes::FakePaths::new(
            dir.path(),
        ))
        .unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    paths,
                    Default::default(),
                    lib,
                    dir.path().join("unused-cli"),
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| PeopleView::new(state, window, cx));
        window
            .update(cx, |view, _, cx| view.select(a.id.clone(), cx))
            .unwrap();
        cx.run_until_parked();
        let a_editor = window
            .update(cx, |view, window, cx| {
                view.start_edit(window, cx);
                view.editor.as_ref().unwrap().generation
            })
            .unwrap();
        // A's save is in flight; the user moves on to B and starts editing.
        window
            .update(cx, |view, _, cx| view.select(b.id.clone(), cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                view.start_edit(window, cx);
                let editor = view.editor.as_ref().unwrap();
                editor
                    .name
                    .update(cx, |input, cx| input.set_value("Bea Unsaved", window, cx));
                view.finish_save(&a.id, a_editor, &a.name, Ok(None), cx);
                let editor = view.editor.as_ref().expect("B's editor stays open");
                assert_eq!(editor.name.read(cx).value().as_ref(), "Bea Unsaved");
                assert_eq!(view.message, None);
                view.finish_save(
                    &a.id,
                    a_editor,
                    &a.name,
                    Err(SaveRefusal::Failed("synthetic failure".into())),
                    cx,
                );
                assert_eq!(view.editor.as_ref().unwrap().error, None);
                // B's own save completes normally.
                let b_editor = view.editor.as_ref().unwrap().generation;
                view.finish_save(&b.id, b_editor, &b.name, Ok(None), cx);
                assert!(view.editor.is_none());
                assert_eq!(
                    view.message.as_deref(),
                    Some(format!("Saved {}.", b.name).as_str())
                );
            })
            .unwrap();
    }

    fn people_window(
        dir: &Path,
        lib: Library,
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<AppState>, gpui::WindowHandle<PeopleView>) {
        let paths =
            broscribe_platform::resolve_paths(&broscribe_platform::fakes::FakePaths::new(dir))
                .unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    paths,
                    Default::default(),
                    lib,
                    dir.join("unused-cli"),
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let window = cx.add_window({
            let state = state.clone();
            |window, cx| PeopleView::new(state, window, cx)
        });
        (state, window)
    }

    #[gpui::test]
    fn people_draw_one_title_with_or_without_a_selected_person(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let (_state, window) = people_window(dir.path(), lib.clone(), cx);
        cx.update_window(window.into(), |_, window, cx| {
            crate::ui::a11y_tree::draw_and_read(window, cx).assert_title("people-title");
        })
        .unwrap();

        let person = Person::new(PersonId::random(), "Synthetic Person");
        lib.write(|tx| tx.save_person(&person)).unwrap();
        window
            .update(cx, |view, _, cx| {
                view.detail = Some(load_detail(&lib, &person.id).unwrap());
                view.selected = Some(person.id);
                cx.notify();
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            tree.assert_title("person-name");
            tree.assert_heading("person-meetings-heading", 2);
            tree.assert_heading("person-samples-heading", 2);
        })
        .unwrap();
    }

    #[gpui::test]
    fn a_meeting_sample_uses_the_meetings_local_date_regardless_of_stored_offset(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let (_state, window) = people_window(dir.path(), lib, cx);
        window
            .update(cx, |view, _, cx| {
                let at = DateTime::parse_from_rfc3339("2026-01-01T23:30:00Z").unwrap();
                let other = DateTime::parse_from_rfc3339("2026-01-02T01:30:00+02:00").unwrap();
                let expected = crate::ui::format::spoken_local_date(&at);
                let mut sample = VoiceSample {
                    id: "synthetic-sample".into(),
                    kind: VoiceprintKind::Confirmation,
                    strength: Strength::Confirmed,
                    device: None,
                    meeting_id: None,
                    meeting_title: Some("Synthetic meeting".into()),
                    meeting_date: Some(at),
                    clip: None,
                };
                let read = |sample: &VoiceSample| {
                    let element = view.sample_row(0, sample, cx).into_element();
                    assert_eq!(gpui::Element::a11y_role(&element), Some(Role::ListItem));
                    let mut node = gpui::accesskit::Node::new(Role::ListItem);
                    gpui::Element::write_a11y_info(&element, &mut node);
                    node.label()
                        .expect("sample's visible description")
                        .to_owned()
                };
                let first = read(&sample);
                sample.meeting_date = Some(other);
                assert_eq!(read(&sample), first);
                assert!(first.contains(&expected), "{first}");
                assert!(
                    first.contains("Synthetic meeting") && first.contains("Named by you"),
                    "{first}"
                );
            })
            .unwrap();
    }

    #[test]
    fn editor_spec_keeps_changes_made_elsewhere_and_refuses_the_same_field() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let p = person();
        lib.write(|tx| tx.save_person(&p)).unwrap();
        let baseline = ops::person_state(&lib, &p.id).unwrap();
        // Elsewhere (another window, the CLI): a new email and new notes.
        let elsewhere = PersonEditSpec {
            add_emails: vec!["lima@example.org".into()],
            notes: Some("theirs".into()),
            ..PersonEditSpec::default()
        };
        ops::with_index(&lib, |tx, index| {
            ops::edit_person(tx, index, &p.id, &elsewhere)
        })
        .unwrap();
        let now = ops::person_state(&lib, &p.id).unwrap();
        assert_eq!(
            changed_fields(&baseline.revision, &now.revision),
            vec![PersonField::Emails, PersonField::Notes]
        );
        // Editing the notes from the old baseline is refused, nothing written.
        let both = PersonForm {
            name: "Ana L. Lima".into(),
            aliases: "Ana".into(),
            emails: "ana@example.org".into(),
            notes: "mine".into(),
        };
        let spec = edit_spec(&baseline, &both).unwrap();
        let refused = ops::with_index(&lib, |tx, index| ops::edit_person(tx, index, &p.id, &spec))
            .unwrap_err();
        assert_eq!(refused.code(), "stale_person");
        assert_eq!(ops::person_state(&lib, &p.id).unwrap(), now);
        // Only renaming applies, keeping the email and notes added elsewhere.
        let rename = PersonForm {
            notes: String::new(),
            ..both
        };
        let spec = edit_spec(&baseline, &rename).unwrap();
        assert!(spec.notes.is_none() && spec.remove_emails.is_empty());
        let saved =
            ops::with_index(&lib, |tx, index| ops::edit_person(tx, index, &p.id, &spec)).unwrap();
        assert!(saved.undo_id.is_some());
        let after = ops::person_state(&lib, &p.id).unwrap();
        assert_eq!(after.person.name, "Ana L. Lima");
        assert!(after.person.emails.contains(&"lima@example.org".to_owned()));
        assert_eq!(after.notes.trim_end(), "theirs");
        assert!(conflict_message(&[PersonField::Notes]).contains("notes"));
    }

    #[gpui::test]
    fn reload_while_editing_keeps_baseline_and_draft(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let a = person();
        lib.write(|tx| tx.save_person(&a)).unwrap();
        // As saved: with the hue the library assigned.
        let a = lib.load_person(&a.id).unwrap();
        let (_state, window) = people_window(dir.path(), lib.clone(), cx);
        window
            .update(cx, |view, _, cx| view.select(a.id.clone(), cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                view.start_edit(window, cx);
                let editor = view.editor.as_ref().unwrap();
                editor
                    .aliases
                    .update(cx, |input, cx| input.set_value("Ana, Aninha", window, cx));
            })
            .unwrap();
        // Someone else renames Ana and adds an email while the editor is open.
        let mut elsewhere = a.clone();
        elsewhere.name = "Ana Souza".into();
        elsewhere.emails.push("souza@example.org".into());
        lib.write(|tx| tx.save_person(&elsewhere)).unwrap();
        window.update(cx, |view, _, cx| view.reload(cx)).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, cx| {
                assert_eq!(view.detail().unwrap().person.name, "Ana Souza");
                let editor = view.editor.as_ref().expect("the editor stays open");
                assert_eq!(
                    editor.baseline.as_ref().unwrap().person,
                    a,
                    "the baseline never moves"
                );
                assert_eq!(
                    editor.external,
                    vec![PersonField::Name, PersonField::Emails]
                );
                assert_eq!(editor.aliases.read(cx).value().as_ref(), "Ana, Aninha");
                assert_eq!(editor.name.read(cx).value().as_ref(), "Ana Lima");
                // A late reply of an older read never rebinds the editor.
                let stale = view.detail_generation - 1;
                view.finish_detail(&a.id, stale, Err(Error::NotFound("gone".into())), cx);
                assert!(view.editor.is_some() && view.selected.as_ref() == Some(&a.id));
            })
            .unwrap();
    }

    /// WRT-08: a failed list read is an Alert with its cause in words, and
    /// a real Try again beside it reloads the list.
    #[gpui::test]
    fn a_failed_people_list_is_an_alert_with_try_again(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let a = person();
        lib.write(|tx| tx.save_person(&a)).unwrap();
        std::fs::create_dir_all(Index::path(lib.root()).parent().unwrap()).unwrap();
        std::fs::write(Index::path(lib.root()), b"not a database").unwrap();
        let (state, window) = people_window(dir.path(), lib.clone(), cx);
        state.update(cx, |s, cx| {
            s.set_local_jobs_allowed(false, cx);
            s.navigate(Route::people(), cx);
        });
        let view = window
            .update(cx, |view, _, cx| {
                view.reload(cx);
                cx.entity()
            })
            .unwrap();
        cx.run_until_parked();
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.refresh().unwrap();
        let named = vcx
            .update_window(window.into(), |_, window, cx| {
                view.update(cx, |view, cx| {
                    crate::ui::spoken_log::while_building(|| Render::render(view, window, cx)).1
                })
            })
            .unwrap();
        let (_, role, text) = named
            .iter()
            .find(|(id, ..)| id == "people-list-error")
            .expect("the failure is a node");
        assert_eq!(*role, Role::Alert);
        assert!(
            text.starts_with("Couldn't load people. ") && text.ends_with(" Try again."),
            "{text}"
        );
        assert!(!text.contains("lib") && !text.contains("schema"), "{text}");

        // The index is repaired (gone: a new library); Try again reloads.
        std::fs::remove_file(Index::path(lib.root())).unwrap();
        let before = view.read_with(&vcx, |view, _| view.list_generation);
        let retry = vcx
            .debug_bounds("people-list-retry")
            .expect("Try again is beside the Alert");
        vcx.simulate_click(retry.center(), gpui::Modifiers::none());
        vcx.run_until_parked();
        view.read_with(&vcx, |view, _| {
            assert_eq!(view.list_generation, before + 1, "Try again reloads");
            assert_eq!(view.list_error, None);
        });
    }

    /// The retained view reads nothing while another route shows: a change
    /// (watcher or window activation) only marks it stale, and showing
    /// People again reloads once, keeping the selection and the draft.
    #[gpui::test]
    fn hidden_people_reload_once_when_shown_again(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let a = person();
        lib.write(|tx| tx.save_person(&a)).unwrap();
        let (state, window) = people_window(dir.path(), lib.clone(), cx);
        // No queue worker child: only the view's own reads are counted.
        state.update(cx, |s, cx| {
            s.set_local_jobs_allowed(false, cx);
            s.navigate(Route::people(), cx);
        });
        window
            .update(cx, |view, _, cx| view.select(a.id.clone(), cx))
            .unwrap();
        cx.run_until_parked();
        let loaded = window
            .update(cx, |view, window, cx| {
                view.start_edit(window, cx);
                let editor = view.editor.as_ref().unwrap();
                editor
                    .aliases
                    .update(cx, |input, cx| input.set_value("Ana, Aninha", window, cx));
                view.list_generation
            })
            .unwrap();
        state.update(cx, |s, cx| s.navigate(Route::library(), cx));
        let mut elsewhere = a.clone();
        elsewhere.name = "Ana Souza".into();
        lib.write(|tx| tx.save_person(&elsewhere)).unwrap();
        state.update(cx, |s, cx| {
            s.library_changed(
                &crate::watch::Changes {
                    people: true,
                    ..Default::default()
                },
                cx,
            );
            s.refresh_all(cx);
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.list_generation, loaded, "no read while hidden");
                assert_eq!(view.detail().unwrap().person.name, "Ana Lima");
            })
            .unwrap();
        state.update(cx, |s, cx| s.navigate(Route::people(), cx));
        cx.run_until_parked();
        window
            .update(cx, |view, _, cx| {
                assert_eq!(view.list_generation, loaded + 1, "one reload on return");
                assert_eq!(view.detail().unwrap().person.name, "Ana Souza");
                assert_eq!(view.selected.as_ref(), Some(&a.id));
                let editor = view.editor.as_ref().expect("the editor stays open");
                assert_eq!(editor.aliases.read(cx).value().as_ref(), "Ana, Aninha");
                assert_eq!(editor.external, vec![PersonField::Name]);
            })
            .unwrap();
    }

    /// The directory query (a name or email) reaches the actual lookup
    /// child only on stdin, never in its argv.
    #[cfg(unix)]
    #[gpui::test]
    fn directory_query_travels_only_on_stdin(cx: &mut gpui::TestAppContext) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let p = person();
        lib.write(|tx| tx.save_person(&p)).unwrap();
        let cli = dir.path().join("synthetic-cli");
        let d = dir.path().display();
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\n[ \"$4 $5\" = 'people lookup' ] || {{ printf '{{}}'; exit 0; }}\n\
                 printf '%s\\n' \"$*\" >> '{d}/args.log'\ncat > '{d}/stdin.json'\n\
                 printf '{{\"results\":[],\"errors\":[]}}'\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = lib.root().to_owned();
        let paths = broscribe_platform::resolve_paths(&broscribe_platform::fakes::FakePaths::new(
            dir.path(),
        ))
        .unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(paths, Default::default(), lib, cli),
                cx,
            )
        });
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| PeopleView::new(state, window, cx));
        let query = "synthetic-marker <synthetic-marker@example.org>";
        window
            .update(cx, |view, window, cx| {
                view.select(p.id.clone(), cx);
                view.directory_input
                    .update(cx, |input, cx| input.set_value(query, window, cx));
                view.search_directory(cx);
            })
            .unwrap();
        cx.run_until_parked();
        let args = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [format!(
                "--library {} --json people lookup --query-stdin",
                root.display()
            )]
        );
        assert!(!args.contains("synthetic-marker"), "{args}");
        let sent: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("stdin.json")).unwrap()).unwrap();
        assert_eq!(sent, serde_json::json!({ "query": query }));
        window
            .update(cx, |view, _, _| {
                assert!(!view.directory.searching);
                assert_eq!(
                    view.directory.message.as_deref(),
                    Some("No directory matches.")
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn forget_dialogs_keep_their_identity_and_restore_their_openers(cx: &mut gpui::TestAppContext) {
        use crate::ui::kit::dialog_gate::{Expected, cancel_both_ways, click};

        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let person = person();
        lib.write(|tx| tx.save_person(&person)).unwrap();
        let (state, _) = people_window(dir.path(), lib, cx);
        let mut entity = None;
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|cx| PeopleView::new(state, window, cx));
            view.update(cx, |view, cx| view.select(person.id.clone(), cx));
            entity = Some(view.clone());
            gpui::component::Root::new(view, window, cx)
        });
        let view = entity.unwrap();
        cx.run_until_parked();
        let vcx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        for (id, title, action, lines) in [
            (
                "person-forget-voice",
                format!("Forget {}'s voice?", display_name(&person)),
                "Forget voice",
                vec![
                    (
                        "forget-goes-speaker-data",
                        format!("The voice data of speakers named {}", display_name(&person)),
                    ),
                    (
                        "forget-changes-recognition",
                        format!(
                            "BroScribe stops recognising or suggesting {} until you name them in a meeting again.",
                            display_name(&person)
                        ),
                    ),
                    (
                        "forget-stays-speaker-names",
                        format!(
                            "The name on every speaker already named {}",
                            display_name(&person)
                        ),
                    ),
                ],
            ),
            (
                "person-forget",
                format!("Forget {}?", display_name(&person)),
                "Forget person",
                vec![
                    (
                        "forget-goes-person",
                        format!("{} and their notes", display_name(&person)),
                    ),
                    (
                        "forget-changes-attendance",
                        format!("{} leaves participant lists", display_name(&person)),
                    ),
                    (
                        "forget-stays-meetings",
                        "The meetings, their transcripts and recordings".to_owned(),
                    ),
                ],
            ),
        ] {
            let lines: Vec<_> = lines
                .iter()
                .map(|(id, words)| (*id, words.as_str()))
                .collect();
            cancel_both_ways(
                vcx,
                id,
                Expected {
                    title: &title,
                    cancel_id: "forget-confirm-cancel",
                    action_id: "forget-confirm-yes",
                    action,
                    lines: &lines,
                    headings: &["What goes", "What changes", "What stays"],
                },
                |vcx| click(vcx, id, Role::Button),
            );
            assert!(view.read_with(vcx, |view, _| view.confirm.is_none()
                && view.forgetting.is_empty()));
        }
    }

    #[gpui::test]
    fn a_pending_forget_keeps_its_confirmation_open(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let person = person();
        lib.write(|tx| tx.save_person(&person)).unwrap();
        let (state, _) = people_window(dir.path(), lib, cx);
        let mut person_view = None;
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|cx| PeopleView::new(state, window, cx));
            person_view = Some(view.clone());
            gpui::component::Root::new(view, window, cx)
        });
        let view = person_view.unwrap();
        view.update(cx, |view, cx| view.select(person.id.clone(), cx));
        cx.update_window(window.into(), |_, win, cx| win.draw(cx).clear(cx))
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, win, cx| {
            view.update(cx, |view, cx| {
                view.ask_forget(ForgetKind::Voice, win, cx);
                view.forgetting.insert(person.id.clone());
                cx.notify();
            });
        })
        .unwrap();
        cx.simulate_keystrokes(window.into(), "escape");
        cx.update_window(window.into(), |_, win, cx| {
            view.update(cx, |view, cx| {
                view.start_edit(win, cx);
                view.ask_forget(ForgetKind::Person, win, cx);
                assert!(matches!(view.confirm, Some(ForgetKind::Voice)));
                assert!(view.editor.is_none());
            });
        })
        .unwrap();
    }

    #[gpui::test]
    fn forget_completion_leaves_another_persons_editor_alone(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let a = person();
        let b = Person::new(PersonId::random(), "Bea Example");
        lib.write(|tx| {
            tx.save_person(&a)?;
            tx.save_person(&b)
        })
        .unwrap();
        let (_state, window) = people_window(dir.path(), lib.clone(), cx);
        window
            .update(cx, |view, _, cx| {
                view.select(a.id.clone(), cx);
            })
            .unwrap();
        cx.run_until_parked();
        // A's forget is running; the user moves on to B and starts editing.
        window
            .update(cx, |view, _, cx| {
                view.forgetting.insert(a.id.clone());
                view.select(b.id.clone(), cx);
            })
            .unwrap();
        cx.run_until_parked();
        let outcome = forget(&lib, dir.path(), &a.id, ForgetKind::Person).unwrap();
        window
            .update(cx, |view, window, cx| {
                view.start_edit(window, cx);
                let editor = view.editor.as_ref().unwrap();
                editor
                    .name
                    .update(cx, |input, cx| input.set_value("Bea Draft", window, cx));
                view.finish_forget(&a.id, "Ana Lima", ForgetKind::Person, Ok(outcome), cx);
                assert_eq!(view.selected.as_ref(), Some(&b.id));
                assert_eq!(view.detail().unwrap().person.id, b.id);
                let editor = view.editor.as_ref().expect("B's editor stays open");
                assert_eq!(editor.name.read(cx).value().as_ref(), "Bea Draft");
                assert!(view.forgetting.is_empty());
                assert!(view.message.as_deref().unwrap().contains("Ana Lima"));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, cx| {
                let editor = view.editor.as_ref().expect("the reload keeps B's draft");
                assert_eq!(editor.name.read(cx).value().as_ref(), "Bea Draft");
                assert!(view.people().iter().all(|row| row.summary.id != a.id));
            })
            .unwrap();
    }

    /// Releasing a view releases only what it still holds: its playing
    /// sample stops, a newer owner's sample keeps playing, and a preparation
    /// it started is cancelled. GPUI releases a dropped view when it next
    /// flushes its effects (`cx.update`).
    #[gpui::test]
    fn releasing_the_view_stops_its_sample(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(dir.path(), FakeAudioPlayer::new(60.0), cx);
        let person = PersonId::random();
        let wav = state.read_with(cx, |s, _| {
            s.library()
                .person_dir(&person)
                .join(ENROLLMENT_DIR)
                .join("sha.wav")
        });
        std::fs::create_dir_all(wav.parent().unwrap()).unwrap();
        std::fs::write(&wav, b"synthetic").unwrap();
        let clip = Clip {
            source: ClipSource::Enrollment {
                person_id: person,
                sha256: "sha".into(),
            },
            start_s: 1.0,
            end_s: 30.0,
        };
        let playing = |cx: &mut gpui::TestAppContext| {
            state.update(cx, |state, _| state.player_mut().unwrap().is_playing())
        };
        let start = |cx: &mut gpui::TestAppContext| {
            let view = cx.new(|_| Samples {
                playback: SamplePlayback::default(),
            });
            view.update(cx, |view, cx| {
                view.playback
                    .play("long".into(), clip.clone(), &state, cx, samples_of)
            });
            cx.run_until_parked();
            view
        };

        // The view holding the player stops it.
        let view = start(cx);
        assert!(playing(cx));
        drop(view);
        cx.update(|_| {});
        assert!(!playing(cx), "the released view's sample stopped");

        // A view another view took the player from leaves it playing.
        let older = start(cx);
        let newer = start(cx);
        assert!(newer.read_with(cx, |v, _| v.playback.is_playing("long")));
        drop(older);
        cx.update(|_| {});
        assert!(playing(cx), "the newer owner's sample keeps playing");

        // A view released while the player prepares its file cancels that
        // preparation and stops the player it took over.
        let preparing = cx.new(|_| Samples {
            playback: SamplePlayback::default(),
        });
        let cancel = preparing.update(cx, |view, cx| {
            // Starting installs the release hook; the file is then found
            // and the player prepares it, as `play` does once located.
            view.playback
                .play("pending".into(), clip.clone(), &state, cx, samples_of);
            let generation = view.playback.generation;
            let (owner, preparation) = state.update(cx, |s, _| {
                let owner = s.claim_player();
                (owner, s.player_owned(owner).unwrap().prepare(&wav).unwrap())
            });
            let cancel = preparation.cancel_handle();
            let pending = PendingStart {
                owner,
                start_s: 1.0,
                end_s: 30.0,
                retry: None,
            };
            run_preparation(
                &mut view.playback,
                &state,
                generation,
                preparation,
                pending,
                cx,
                samples_of,
            );
            cancel
        });
        drop(preparing);
        cx.update(|_| {});
        assert!(cancel.is_cancelled(), "the preparation was cancelled");
        assert!(!playing(cx), "the released owner stopped the player");
        cx.run_until_parked();
        assert!(!playing(cx), "the cancelled preparation never plays");
    }

    struct Samples {
        playback: SamplePlayback,
    }

    fn samples_of(view: &mut Samples) -> &mut SamplePlayback {
        &mut view.playback
    }

    fn state_with(
        dir: &Path,
        player: FakeAudioPlayer,
        cx: &mut gpui::TestAppContext,
    ) -> Entity<AppState> {
        let lib = Library::open(dir.join("lib")).unwrap();
        let paths =
            broscribe_platform::resolve_paths(&broscribe_platform::fakes::FakePaths::new(dir))
                .unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    paths,
                    Default::default(),
                    lib,
                    dir.join("unused-cli"),
                ),
                cx,
            )
        });
        state.update(cx, |state, _| state.set_player(Box::new(player)));
        state
    }

    /// Claims the player and prepares `path` as the app does, running the
    /// probe here.
    fn prepared(
        state: &Entity<AppState>,
        path: &Path,
        cx: &mut Context<Samples>,
    ) -> (u64, Result<PreparedPlayback>) {
        let (owner, prep) = state.update(cx, |s, _| {
            let owner = s.claim_player();
            (owner, s.player_owned(owner).unwrap().prepare(path).unwrap())
        });
        (owner, prep.run())
    }

    #[gpui::test]
    fn a_superseded_preparation_never_plays(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(dir.path(), FakeAudioPlayer::new(60.0), cx);
        let view = cx.new(|_| Samples {
            playback: SamplePlayback::default(),
        });
        let wav = dir.path().join("synthetic.wav");
        view.update(cx, |view, cx| {
            // The probe answers (off the UI loop in the app) …
            let (owner, prepared) = prepared(&state, &wav, cx);
            assert!(prepared.is_ok());
            // … but a newer request was made meanwhile.
            let _newer = state.update(cx, |s, _| {
                s.player_owned(owner).unwrap().prepare(&wav).unwrap()
            });
            view.playback.active = Some("old".into());
            view.playback.owner = Some(owner);
            let generation = view.playback.generation;
            let pending = PendingStart {
                owner,
                start_s: 1.0,
                end_s: 2.0,
                retry: None,
            };
            start_prepared(view, &state, generation, pending, prepared, cx, samples_of);
            assert_eq!(view.playback.active(), None);
            assert_eq!(view.playback.status_of("old"), None, "superseded is silent");
        });
        state.update(cx, |state, _| {
            assert!(!state.player_mut().unwrap().is_playing())
        });
    }

    #[gpui::test]
    fn a_file_the_player_cannot_play_whole_plays_from_the_cache(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("recording.m4a");
        let cached = dir.path().join("playback-cache.wav");
        let mut player = FakeAudioPlayer::new(60.0);
        player.needs_cache = vec![original.clone()];
        let state = state_with(dir.path(), player, cx);
        let view = cx.new(|_| Samples {
            playback: SamplePlayback::default(),
        });
        let clip = Clip {
            source: ClipSource::Meeting {
                meeting_id: MeetingId::random(),
                sha256: None,
            },
            start_s: 1.0,
            end_s: 2.0,
        };
        let files = |cache: Option<PlaybackCache>| ClipFiles::Found {
            audio: original.clone(),
            cache,
            mix_required: false,
        };
        let start = |view: &mut Samples, files: ClipFiles, cx: &mut Context<Samples>| {
            let (owner, prepared) = prepared(&state, &original, cx);
            assert_eq!(prepared.as_ref().unwrap_err().code(), PLAYBACK_NEEDS_CACHE);
            view.playback.active = Some("k".into());
            view.playback.status = Some(LOADING.into());
            view.playback.owner = Some(owner);
            let generation = view.playback.generation;
            let pending = PendingStart {
                owner,
                start_s: 1.0,
                end_s: 2.0,
                retry: Some((files, clip.clone())),
            };
            start_prepared(view, &state, generation, pending, prepared, cx, samples_of);
        };
        // With the playable cache present, the cache plays the same range.
        let with_cache = files(Some(PlaybackCache {
            path: cached.clone(),
            exists: true,
            meeting_id: MeetingId::random(),
            sha256: "synthetic".into(),
        }));
        view.update(cx, |view, cx| {
            start(view, with_cache, cx);
            assert_eq!(view.playback.status_of("k"), Some(LOADING), "not a failure");
            assert!(!view.playback.is_playing("k"));
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert!(view.playback.is_playing("k"));
            assert_eq!(view.playback.status_of("k"), None);
        });
        state.update(cx, |state, _| {
            assert_eq!(
                state.player_mut().unwrap().poll(),
                PlayerState::Playing { position_s: 1.0 }
            )
        });
        // A file without a cache (an enrollment passage) says so.
        view.update(cx, |view, cx| {
            view.playback.stop(&state, cx);
            start(view, files(None), cx);
            assert_eq!(
                view.playback.status_of("k"),
                Some("Playback is not supported for this format.")
            );
        });
    }
}
