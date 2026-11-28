//! `broscribe mcp`: the MCP server on stdio (design §11, rmcp 3.9.0).
//!
//! The read tools are `search_meetings`, `search_utterances`, `get_meeting`, `generation_status`, `get_transcript`, `list_people`, `get_person`, `list_action_items`, `list_decisions`, `list_speaker_reviews`, `people_review`, `speaker_voice_matches`, `recheck_identifications`, `speaker_duplicates`, `list_templates`, `get_template_result`, `search_templates`, `export_meeting`, `get_live_preview`, `get_diagnostics`, `get_recording_status`, `get_daemon_status`, `list_recording_prompts`, `get_queue_status`, `get_identity_status`, `list_live_speaker_reviews`, `get_speaker_samples`, `list_person_samples`, `get_library_filters`, `get_library_disk_usage`, `check_library`, `get_meeting_timeline`, `list_meeting_review_notes`, `show_meeting_record`, `find_transcript`, `get_about`, `get_version`, `get_maintenance_status`, `get_app_permissions`, `get_detection_status`, `dump_reader_structure`, `list_audio_devices`, `get_microphone_pin`, `get_audio_check_status`, `list_models`, `check_models`, `get_setup_model_check`, `get_voice_enrollment_status`, `get_setup_status`, `get_voice_passage`, `get_setup_diagnostics`, `list_providers`, `list_provider_models`, `get_summary_instructions`, `get_template_definition`, `get_calendar_status`, `list_calendar_accounts`, `list_calendars`, `get_calendar_login_attempt`, `search_directory`, `get_agent_tools_status`, `get_mcp_status`, `get_app_setting`, `get_text_size`, `get_theme`, `get_speaker_grouping`, `get_library_folder`, `get_default_language`, `get_word_list`, `get_window_settings`, `get_import_handling`, `get_consent_reminder`, `get_automatic_summaries`, `preview_import`, `preview_index_repair`, `search_people`, `list_library_meetings`, `poll_daemon_events`, `preview_identity_reresolution`.
//! The write tools are `edit_person`, `keep_identification`, `add_person`, `check_review_note`, `edit_action`, `cancel_generation`, `restore_generation`, `forget_person`, `forget_voice`, `reject_speaker`, `dismiss_speaker_review`, `restore_speaker_review`, `name_speaker`, `undo_live_speaker_name`, `tag_meeting`, `set_meeting_title`, `update_action_status`, `start_recording`, `pause_recording`, `resume_recording`, `stop_recording`, `process_recording_now`, `bookmark_recording`, `note_recording`, `set_recording_language`, `tag_recording`, `set_recording_participant`, `set_recording_title`, `set_recording_auto_stop`, `retry_recording`, `answer_recording_prompt`, `attribute_live_turn`, `dismiss_live_speaker_review`, `attribute_transcript_lines`, `queue_reprocess_meeting`, `import_recording`, `repair_library_index`, `check_recording_file`, `prepare_meeting_audio`, `export_meeting_clip`, `export_person_clip`, `undo_person_edit`, `start_daemon`, `install_daemon`, `show_app`, `request_app_permission`, `set_app_setting`, `set_text_size`, `set_theme`, `set_speaker_grouping`, `set_default_language`, `add_words`, `remove_word`, `set_import_handling`, `enable_consent_reminder`, `set_automatic_summaries`, `set_microphone_pin`, `pull_model`, `use_model`, `configure_model_profile`, `import_contacts`, `set_self_person`, `enroll_voice`, `check_microphone`, `try_setup_transcription`, `set_setup_deferred`, `setup_recording_access`, `run_audio_check`, `complete_setup_step`, `create_template`, `edit_template`, `set_template_automatic`, `set_summary_instructions`, `clear_summary_instructions`, `configure_provider`, `disable_provider`, `test_provider`, `prefer_provider`, `cache_person_photo`, `configure_google_calendar`, `select_system_calendars`, `select_calendars`, `remove_selected_calendar`, `sync_calendars`, `sync_calendar_account`, `cancel_calendar_login`, `install_agent_tools`, `apply_directory_email`, `connect_system_calendar`, `answer_agent_notice`, `stop_daemon`, `export_meeting_file`, `edit_meeting_session`.
//!
//! The resource template `meeting://<id>/transcript.md` returns the rendered
//! `meeting.md`. See design §11 for the scope of these tools.
//!
//! - Reads open a short-lived read-only index connection per request (or read
//!   the meeting files) and never take the library lock. `search_meetings`,
//!   `search_utterances` and `list_action_items` page with the index's
//!   cursors (`page_size`, `cursor`); a semantic or hybrid page keeps the
//!   exact query vector its cursor is bound to, so the next page is ranked
//!   against the same bits. A continuation naming another mode than the
//!   one its cursor was issued for is `cursor_mismatch`; one naming none
//!   continues in that mode.
//! - Writes go through core `ops` inside one [`Library::write_checked`],
//!   with a writable index opened inside that transaction: a request
//!   cancelled while it waited for the library lock is refused once the
//!   lock is held, before anything changes. A forget purges cached photos on
//!   its own blocking thread after the library is released and before that
//!   work returns, so neither a cancelled request nor an ended connection
//!   (whose pending handlers the runtime drops) skips it. The other
//!   follow-ups of a committed write (search vectors, retention, the speaker
//!   recheck) run even when the request is cancelled meanwhile; what they
//!   catch up on stays pending durably if the connection ends first.
//! - Requests run concurrently. Only the work needing the borrowed models,
//!   decoders and process probe of [`Deps`] (confirmation samples and
//!   retention) runs on the thread that called [`serve`], fed by a bounded
//!   queue; searches and vector refreshes use the shared secrets and model
//!   [`ExecutionContext`] on blocking threads, so they never wait behind it.
//! - A client's `notifications/cancelled` cancels only that request: queued
//!   work it owned is never started, its later phases never run and its
//!   answer is dropped. A request carrying `_meta.progressToken` gets
//!   `notifications/progress` as each real phase starts (never a completion:
//!   the answer is that).
//! - Failures are tool errors whose text starts with the stable error code;
//!   their structured content is the CLI's `{"error":{"code","message"}}`.
//! - `[mcp] enabled = false` in `config.toml` refuses startup and, re-read
//!   on every tool call and resource read, a running server's requests
//!   (`mcp_disabled`), before the library is touched.
//! - stdout carries the protocol only; tracing goes to stderr.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use broscribe_core::config::Config;
use broscribe_core::export::ExportFormat;
use broscribe_core::index::{
    CURSOR_MISMATCH, CursorRequest, Index, MAX_PAGE_SIZE, MeetingHit, PageCursor, QueryVector,
    SearchHit, SearchMode, SearchQuery,
};
use broscribe_core::ops::{self, ConfirmationSample};
use broscribe_core::render::{self, PeopleLookup};
use broscribe_core::schema::{ActionStatus, Meeting};
use broscribe_core::transcript::{self, TranscriptFilter};
use broscribe_core::{Error, Library, MeetingId, Result, SpeakerId};
use broscribe_integrations::PhotoPurgeStatus;
use broscribe_models::{ExecutionContext, SpeakerEmbedder};
use broscribe_pipeline::retention::{RetentionOptions, RetentionReport};
use broscribe_pipeline::{retention, vectors};
use broscribe_platform::{SecretStore, resolve_paths};
use chrono::{DateTime, Utc};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ListResourceTemplatesResult,
    PaginatedRequestParams, ProgressNotificationParam, ProgressToken, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, ResourceContents, ResourceTemplate,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData, Peer, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use crate::Deps;
use crate::cmd::diagnostics::{self, DiagnosticsReport};
use crate::cmd::{meeting_id, speaker_id};
use crate::query::{self, ActionFilters, DecisionFilters};

mod commands;

/// Results returned when a search names no limit.
const DEFAULT_LIMIT: usize = 20;
/// Jobs waiting for the [`Deps`] thread; a full queue makes the next
/// request wait (cancellably), never grow the queue.
const WORKER_QUEUE: usize = 4;
/// Query vectors kept for continuing semantic and hybrid pages.
const QUERY_VECTORS: usize = 32;
/// Error code of a request the client cancelled.
const CANCELLED: &str = "cancelled";
const URI_PREFIX: &str = "meeting://";
const URI_SUFFIX: &str = "/transcript.md";
const URI_TEMPLATE: &str = "meeting://{id}/transcript.md";
const MARKDOWN: &str = "text/markdown";

pub(crate) fn serve_admitted(
    deps: &Deps,
    library: PathBuf,
    admission: &broscribe_core::maintenance::ModeLease,
) -> Result<()> {
    init_tracing();
    serve_on_admitted(
        deps,
        library,
        admission,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
}

/// [`serve`] over any byte stream pair (tests use an in-memory duplex).
/// Blocks until the client disconnects.
pub fn serve_on<R, W>(
    deps: &Deps,
    library: PathBuf,
    data_dir: PathBuf,
    reader: R,
    writer: W,
) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let admission = broscribe_core::maintenance::ModeLease::acquire(
        &data_dir,
        broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
    )?;
    serve_on_admitted(deps, library, &admission, reader, writer)
}

fn serve_on_admitted<R, W>(
    deps: &Deps,
    library: PathBuf,
    admission: &broscribe_core::maintenance::ModeLease,
    reader: R,
    writer: W,
) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let data_dir = admission.data_dir().to_path_buf();
    // Refused before the library is opened.
    enabled_config(&data_dir)?;
    // Never creates a library: serving a folder that is not one is an error.
    let existing = Library::open_existing(library)?;
    let canonical = existing
        .root()
        .canonicalize()
        .map_err(|error| Error::io("resolving the BroScribe library", error))?;
    let lib = Library::open_existing(canonical)?;
    let (mut server, requests) = Server::new(deps, lib, data_dir);
    server.mode = admission.mode();
    // run joins the protocol thread, whose runtime waits for blocking work.
    // The admission therefore also covers disconnected and queued jobs.
    run(deps, server, requests, reader, writer)
}

/// Serves `server` over `reader`/`writer` on its own protocol thread while
/// this thread answers its [`Deps`] jobs; returns once the client is gone
/// and every blocking task the server started has ended.
fn run<R, W>(
    deps: &Deps,
    server: Server,
    mut requests: mpsc::Receiver<WorkerJob>,
    reader: R,
    writer: W,
) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let lib = server.lib.clone();
    let data_dir = server.data_dir.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::io("starting the MCP runtime", e))?;
    let protocol = std::thread::Builder::new()
        .name("broscribe-mcp".into())
        .spawn(move || {
            runtime.block_on(async move {
                let service = server
                    .serve((reader, writer))
                    .await
                    .map_err(|e| Error::Internal(format!("MCP initialisation failed: {e}")))?;
                service
                    .waiting()
                    .await
                    .map_err(|e| Error::Internal(format!("MCP server stopped: {e}")))?;
                Ok(())
            })
        })
        .map_err(|e| Error::io("starting the MCP thread", e))?;

    // Every sender lives in the server, so this loop ends when it stops.
    // Independent command jobs have bounded admission and borrow the same
    // platform adapters. Keep their lifetime within the server's mode lease.
    std::thread::scope(|scope| {
        let mut embedder = None;
        while let Some(job) = requests.blocking_recv() {
            match job {
                WorkerJob::Command(job) => {
                    let _ = std::thread::Builder::new()
                        .name("broscribe-mcp-command".into())
                        .spawn_scoped(scope, || job.run(deps));
                }
                WorkerJob::Samples(job) => {
                    // Cancelled (or abandoned) while queued: never computed.
                    if job.cancel.is_cancelled() || job.reply.is_closed() {
                        let _ = job.reply.send(Err(cancelled()));
                        continue;
                    }
                    let samples = confirmation_samples(
                        deps,
                        &lib,
                        &mut embedder,
                        &job.meeting,
                        job.speaker,
                        &job.model,
                    );
                    let _ = job.reply.send(samples);
                }
                WorkerJob::Retention { meeting, reply } => {
                    let result = resolve_paths(deps.paths.as_ref()).and_then(|paths| {
                        retention::apply_current(
                            &lib,
                            &data_dir,
                            deps.process.as_ref(),
                            &RetentionOptions {
                                meetings: Some(vec![meeting]),
                                cache_dir: Some(paths.cache_dir),
                                ..RetentionOptions::default()
                            },
                        )
                    });
                    let _ = reply.send(result);
                }
            }
        }
    });
    protocol
        .join()
        .map_err(|_| Error::Internal("the MCP thread panicked".into()))?
}

/// Tracing to stderr only (stdout carries the protocol); `RUST_LOG` filters,
/// default `warn`. Keeps a subscriber that is already installed.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();
}

/// Whether the request that owns some work was cancelled by the client (or
/// the client went away).
#[derive(Clone)]
struct Cancel(Arc<dyn Fn() -> bool + Send + Sync>);

impl Cancel {
    fn of(ctx: &RequestContext<RoleServer>) -> Self {
        let token = ctx.ct.clone();
        Self(Arc::new(move || token.is_cancelled()))
    }

    fn is_cancelled(&self) -> bool {
        (self.0)()
    }

    /// `cancelled` once the request was cancelled.
    fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(cancelled())
        } else {
            Ok(())
        }
    }
}

fn cancelled() -> Error {
    Error::Service {
        code: CANCELLED,
        message: "the client cancelled the request".into(),
    }
}

/// `work`, or `cancelled` as soon as the request is cancelled (the work's
/// answer is then dropped and its later phases never start).
async fn until_cancelled<T>(
    ctx: &RequestContext<RoleServer>,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        () = ctx.ct.cancelled() => Err(cancelled()),
        result = work => result,
    }
}

/// `notifications/progress` for a request that asked for them
/// (`_meta.progressToken`): one as each real phase starts, `progress`
/// counting the phases finished before it, out of `total`.
struct Progress {
    peer: Peer<RoleServer>,
    token: Option<ProgressToken>,
    done: u32,
    total: u32,
}

impl Progress {
    fn of(ctx: &RequestContext<RoleServer>, total: u32) -> Self {
        Self {
            peer: ctx.peer.clone(),
            token: ctx.meta.get_progress_token(),
            done: 0,
            total,
        }
    }

    /// Reports that `phase` starts. The text is fixed: never a name, a
    /// query or transcript text.
    async fn phase(&mut self, phase: &'static str) {
        self.notify(f64::from(self.done), f64::from(self.total), phase)
            .await;
        self.done += 1;
    }

    async fn notify(&self, done: f64, total: f64, message: &'static str) {
        let Some(token) = self.token.clone() else {
            return;
        };
        let param = ProgressNotificationParam::new(token, done)
            .with_total(total)
            .with_message(message);
        if let Err(e) = self.peer.notify_progress(param).await {
            tracing::debug!("progress not delivered: {e}");
        }
    }
}

/// A request for confirmation samples, answered by the thread owning [`Deps`].
struct SampleJob {
    meeting: MeetingId,
    speaker: SpeakerId,
    /// Speaker-embedding model of the active profile.
    model: String,
    /// The requesting call: a cancelled one is skipped, not computed.
    cancel: Cancel,
    reply: oneshot::Sender<Result<Vec<ConfirmationSample>>>,
}

/// Work requiring the borrowed platform dependencies stays on their owning
/// thread.
enum WorkerJob {
    Command(Box<commands::CommandJob>),
    Samples(SampleJob),
    /// Retention after a committed naming (its follow-up, whatever happens
    /// to the request).
    Retention {
        meeting: MeetingId,
        reply: oneshot::Sender<Result<RetentionReport>>,
    },
}

/// Clean-turn samples of the speaker (the pipeline helper the CLI uses);
/// empty when the recording is not available, so `ops::name_speaker` falls
/// back to the centroid. The embedder is loaded once and kept while the
/// profile's model stays the same.
fn confirmation_samples(
    deps: &Deps,
    lib: &Library,
    cache: &mut Option<Box<dyn SpeakerEmbedder + Send>>,
    meeting_id: &MeetingId,
    speaker: SpeakerId,
    model: &str,
) -> Result<Vec<ConfirmationSample>> {
    let meeting = lib.load_meeting(meeting_id)?;
    if lib.meeting_audio_path(&meeting)?.is_none() {
        return Ok(Vec::new());
    }
    let embedder = match cache.take() {
        Some(e) if e.model_id() == model => e,
        _ => match deps.models.load_embedder(model) {
            Ok(e) => e,
            // Like the CLI: without the model, naming uses the centroid.
            Err(Error::ModelMissing(m)) => {
                tracing::warn!("{m}; naming with the speaker's stored centroid");
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        },
    };
    let embedder: &dyn SpeakerEmbedder = &**cache.insert(embedder);
    broscribe_pipeline::confirmation_samples(lib, &deps.decoder, embedder, meeting_id, speaker)
}

/// The query vector a semantic or hybrid page was ranked with, kept under
/// the cursor of the page after it.
#[derive(Clone)]
struct KeptVector {
    mode: SearchMode,
    vector: QueryVector,
    note: Option<String>,
}

/// The last [`QUERY_VECTORS`] kept vectors, oldest dropped first. Without
/// one (evicted, or a cursor of another server) the query is embedded again
/// and the index refuses a vector that differs (`cursor_mismatch`).
#[derive(Default)]
struct QueryVectors(Mutex<VecDeque<(PageCursor, KeptVector)>>);

impl QueryVectors {
    fn get(&self, cursor: &PageCursor, model: &str) -> Option<KeptVector> {
        let kept = self.0.lock().unwrap_or_else(|e| e.into_inner());
        kept.iter()
            .find(|(c, k)| c == cursor && k.vector.model == model)
            .map(|(_, k)| k.clone())
    }

    fn keep(&self, cursor: PageCursor, vector: KeptVector) {
        let mut kept = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if kept.iter().any(|(c, _)| *c == cursor) {
            return;
        }
        if kept.len() == QUERY_VECTORS {
            kept.pop_front();
        }
        kept.push_back((cursor, vector));
    }
}

// ---- parameters ----

/// Filters shared by both searches.
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct SearchFilters {
    /// Date order for unranked meeting results (default newest).
    order: Option<crate::cli::MeetingOrderArg>,
    /// Words to find; every word must match (case and accents ignored).
    /// Omit to list everything the other filters allow.
    #[serde(default)]
    query: Option<String>,
    /// A person's ID, name, alias or e-mail.
    #[serde(default)]
    person: Option<String>,
    /// A tag.
    #[serde(default)]
    tag: Option<String>,
    /// An enrichment topic.
    #[serde(default)]
    topic: Option<String>,
    /// A detector app ID, such as zoom.
    #[serde(default)]
    app: Option<String>,
    /// Recorded or imported meetings.
    #[serde(default)]
    source: Option<crate::cli::MeetingSourceArg>,
    /// Meetings that started at or after this: `30d`, `12h`, `2w`,
    /// `YYYY-MM-DD` or RFC 3339.
    #[serde(default)]
    since: Option<String>,
    /// Meetings that started before this (a date includes that day).
    #[serde(default)]
    until: Option<String>,
    /// Maximum number of results, 1 to 500 (default 20). Not with `page_size`.
    #[serde(default)]
    limit: Option<u32>,
    /// Read one page of this many results (1 to 500) instead: the answer
    /// adds `revision` and, unless it is the last page, `next_cursor`.
    #[serde(default)]
    page_size: Option<u32>,
    /// The `next_cursor` of the previous page, with the same filters.
    /// `cursor_stale` once the index changed: start again without it.
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchUtterancesParams {
    #[serde(flatten)]
    filters: SearchFilters,
    /// Only utterances of this meeting (`m-xxxxxx`).
    #[serde(default)]
    meeting_id: Option<String>,
    /// keyword, semantic or hybrid; omitted: hybrid when vectors are available.
    #[serde(default)]
    mode: Option<SearchModeParam>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SearchModeParam {
    Keyword,
    Semantic,
    Hybrid,
}

impl From<SearchModeParam> for SearchMode {
    fn from(value: SearchModeParam) -> Self {
        match value {
            SearchModeParam::Keyword => Self::Keyword,
            SearchModeParam::Semantic => Self::Semantic,
            SearchModeParam::Hybrid => Self::Hybrid,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ActionStatusParam {
    Open,
    Done,
    Dropped,
}

impl From<ActionStatusParam> for ActionStatus {
    fn from(value: ActionStatusParam) -> Self {
        match value {
            ActionStatusParam::Open => Self::Open,
            ActionStatusParam::Done => Self::Done,
            ActionStatusParam::Dropped => Self::Dropped,
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ActionItemsParams {
    /// Owner: me, person ID, name, alias or e-mail.
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    status: Option<ActionStatusParam>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    meeting_id: Option<String>,
    #[serde(default)]
    query: Option<String>,
    /// Maximum number of actions, 1 to 500 (default 20). Not with `page_size`.
    #[serde(default)]
    limit: Option<u32>,
    /// Read one page of this many actions (1 to 500) instead: the answer
    /// adds `revision` and, unless it is the last page, `next_cursor`.
    #[serde(default)]
    page_size: Option<u32>,
    /// The `next_cursor` of the previous page, with the same filters.
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct DecisionsParams {
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    meeting_id: Option<String>,
    #[serde(default)]
    query: Option<String>,
    /// Maximum number of decisions, 1 to 500 (default 20).
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ActionStatusParams {
    meeting_id: String,
    action_id: String,
    status: ActionStatusParam,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MeetingParams {
    /// Meeting ID (`m-xxxxxx`).
    meeting_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PersonParams {
    /// Local person's ID, name or e-mail.
    person: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ForgetParams {
    /// Local person's ID, name, alias or e-mail.
    person: String,
    /// Set true to perform the permanent deletion. Omit to preview the plan.
    #[serde(default)]
    confirm: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct IdentityChecksParams {
    /// Local person's ID, name or e-mail. Omit to check all profiles.
    person: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct KeepIdentificationParams {
    meeting_id: String,
    speaker_id: String,
    person_id: String,
    /// Voice evidence revision from recheck_identifications.
    revision: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EditPersonParams {
    /// Local person's stable ID.
    person_id: String,
    /// New display name. Omit to keep the current name.
    name: Option<String>,
    /// Addresses to add.
    #[serde(default)]
    add_emails: Vec<String>,
    /// Addresses to remove.
    #[serde(default)]
    remove_emails: Vec<String>,
    /// Replace the whole e-mail list. Omit to apply incremental changes.
    emails: Option<Vec<String>>,
    /// Replace the whole alias list. Omit to apply incremental changes.
    aliases: Option<Vec<String>>,
    /// Aliases to add.
    #[serde(default)]
    add_aliases: Vec<String>,
    /// Aliases to remove.
    #[serde(default)]
    remove_aliases: Vec<String>,
    /// Replace the notes. An empty string clears them.
    notes: Option<String>,
    /// Optional field hashes from get_person for a revision-fenced edit.
    #[schemars(with = "Option<BTreeMap<String, String>>")]
    expected: Option<ops::PersonRevision>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddPersonParams {
    name: String,
    #[serde(default)]
    emails: Vec<String>,
    #[serde(default)]
    notes: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CheckReviewNoteParams {
    meeting_id: String,
    /// One-based position from get_meeting.
    position: usize,
    /// The revision for this position from get_meeting.review_note_revisions.
    revision: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EditActionParams {
    meeting_id: String,
    action_id: String,
    /// New owner, as a local person ID.
    owner: Option<String>,
    #[serde(default)]
    clear_owner: bool,
    /// New due date (YYYY-MM-DD).
    due: Option<String>,
    #[serde(default)]
    clear_due: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CancelGenerationParams {
    job_id: String,
    /// Run token from the generation's queue receipt or queue status.
    run: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestoreGenerationParams {
    meeting_id: String,
    /// Omit to restore the summary.
    template_id: Option<String>,
    /// RFC 3339 creation time of the result shown in the meeting or template.
    expected: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TranscriptParams {
    /// Meeting ID (`m-xxxxxx`).
    meeting_id: String,
    /// Only these lines: a speaker ID (`S2`; a merged member's ID selects
    /// its own lines), a person (ID, name, alias or e-mail: the lines they
    /// said) or the name a line shows (`Speaker 2`).
    #[serde(default)]
    speaker: Option<String>,
    /// Utterances ending after this offset (`hh:mm:ss`, `mm:ss` or seconds).
    #[serde(default)]
    from: Option<String>,
    /// Utterances starting before this offset.
    #[serde(default)]
    to: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NameSpeakerParams {
    /// Meeting ID (`m-xxxxxx`).
    meeting_id: String,
    /// Speaker ID in that meeting (`S2`).
    speaker_id: String,
    /// The person: an ID, an e-mail, or a name (a new name creates the person).
    person: String,
    /// Refuse if the speaker revision from `get_meeting` has changed.
    revision: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RejectSpeakerParams {
    /// Meeting ID (`m-xxxxxx`).
    meeting_id: String,
    /// Speaker ID in that meeting (`S2`).
    speaker_id: String,
    /// The person to stop suggesting, as an ID, name or e-mail. If omitted,
    /// use the speaker's current suggestion or automatic assignment.
    #[serde(default)]
    person: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReviewListParams {
    /// List dismissed speakers instead of the waiting queue.
    #[serde(default)]
    dismissed: bool,
    /// Maximum number of primary rows. Voices without a transcript are returned separately.
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReviewDecisionParams {
    /// Meeting ID (`m-xxxxxx`).
    meeting_id: String,
    /// Speaker ID in that meeting (`S2`).
    speaker_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct VoiceMatchesParams {
    /// Meeting ID (`m-xxxxxx`).
    meeting_id: String,
    /// Speaker ID in that meeting (`S2`).
    speaker_id: String,
    /// Matches to return, 1 to 10 (default 3).
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DuplicatesParams {
    /// Meeting ID (`m-xxxxxx`).
    meeting_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TagParams {
    /// Meeting ID (`m-xxxxxx`).
    meeting_id: String,
    /// Tags to add.
    #[serde(default)]
    add: Vec<String>,
    /// Tags to remove.
    #[serde(default)]
    remove: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TitleParams {
    /// Meeting ID (`m-xxxxxx`).
    meeting_id: String,
    /// The new title: one line, not empty.
    title: String,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct LivePreviewParams {
    /// Recording session ID; the active recording when absent.
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UndoLiveNameParams {
    meeting_id: String,
    speaker_id: String,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ListTemplatesParams {
    /// Also list this meeting's stored results (`m-xxxxxx`).
    #[serde(default)]
    meeting_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TemplateResultParams {
    /// Meeting ID (`m-xxxxxx`).
    meeting_id: String,
    /// Template ID (as `list_templates` lists it).
    template_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchTemplatesParams {
    /// Words to find; every word must match (case and accents ignored).
    query: String,
    /// Maximum number of results, 1 to 500 (default 20).
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ExportFormatParam {
    /// Markdown.
    #[default]
    Md,
    /// Plain text.
    Txt,
    /// JSON.
    Json,
}

impl From<ExportFormatParam> for ExportFormat {
    fn from(value: ExportFormatParam) -> Self {
        match value {
            ExportFormatParam::Md => Self::Markdown,
            ExportFormatParam::Txt => Self::Text,
            ExportFormatParam::Json => Self::Json,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ExportParams {
    /// Meeting ID (`m-xxxxxx`).
    meeting_id: String,
    /// Sections: `transcript`, `summary`, `decisions`, `actions`, `notes`,
    /// `template:<id>`; omitted: every section the meeting has.
    #[serde(default)]
    include: Vec<String>,
    /// md (default), txt or json.
    #[serde(default)]
    format: ExportFormatParam,
    /// Timestamps on transcript lines and items (default true).
    #[serde(default)]
    timestamps: Option<bool>,
}

// ---- results ----

/// A search hit with its offset as `hh:mm:ss`.
#[derive(Serialize)]
struct UtteranceHit {
    #[serde(flatten)]
    hit: SearchHit,
    at: String,
    transcript_uri: String,
}

impl UtteranceHit {
    fn of(hit: SearchHit) -> Self {
        Self {
            at: render::format_hms(hit.start),
            transcript_uri: transcript_uri(&hit.meeting_id),
            hit,
        }
    }
}

// ---- server ----

#[derive(Clone)]
struct Server {
    lib: Library,
    data_dir: PathBuf,
    mode: broscribe_core::maintenance::Mode,
    /// API keys of enabled providers (remote text embedding).
    secrets: Arc<dyn SecretStore>,
    /// Containment and TLS trust of every model the server runs.
    execution: ExecutionContext,
    /// The bounded queue of the [`Deps`] thread.
    jobs: mpsc::Sender<WorkerJob>,
    query_vectors: Arc<QueryVectors>,
    /// The wall clock relative date filters (`since 30d`) resolve against.
    clock: crate::WallClock,
    tool_router: ToolRouter<Self>,
    command_slots: Arc<tokio::sync::Semaphore>,
    write_slots: Arc<tokio::sync::Semaphore>,
    edit_history: Arc<Mutex<crate::cmd::identity_review::EditHistory>>,
    /// This server's test barriers in its write path.
    #[cfg(test)]
    hooks: Hooks,
}

/// Barriers a test sets on one server's write path.
#[cfg(test)]
#[derive(Clone, Default)]
struct Hooks {
    /// Called once a write passed its first cancellation check, before the
    /// enabled check and the library lock.
    admitted: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Called when a write's blocking work ended: its error code, if any.
    ended: Option<Arc<dyn Fn(Option<&'static str>) + Send + Sync>>,
    /// Called when the handler waiting for a write's blocking work is gone:
    /// it got the answer, or the server's teardown dropped it first.
    left: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Calls its hook when dropped.
#[cfg(test)]
struct OnDrop(Option<Arc<dyn Fn() + Send + Sync>>);

#[cfg(test)]
impl Drop for OnDrop {
    fn drop(&mut self) {
        if let Some(hook) = &self.0 {
            hook();
        }
    }
}

impl Server {
    /// A server on `lib`, and the receiving end of its [`Deps`] job queue.
    fn new(deps: &Deps, lib: Library, data_dir: PathBuf) -> (Self, mpsc::Receiver<WorkerJob>) {
        let (jobs, requests) = mpsc::channel(WORKER_QUEUE);
        let server = Self {
            lib,
            data_dir,
            mode: broscribe_core::maintenance::Mode::Normal,
            secrets: Arc::clone(&deps.secrets),
            execution: deps.execution.clone(),
            jobs,
            query_vectors: Arc::default(),
            clock: deps.clock.clone(),
            tool_router: Self::read_tools() + Self::write_tools(),
            command_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            write_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            edit_history: Arc::default(),
            #[cfg(test)]
            hooks: Hooks::default(),
        };
        (server, requests)
    }

    /// When a read asks: the instant its first page resolves relative dates
    /// at.
    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }
}

impl Server {
    fn read_tools() -> ToolRouter<Self> {
        Self::library_reads() + Self::command_reads() + Self::event_reads()
    }

    fn write_tools() -> ToolRouter<Self> {
        Self::library_writes() + Self::command_writes() + Self::session_writes()
    }
}

impl Server {
    fn require_normal(&self) -> Result<()> {
        if self.mode == broscribe_core::maintenance::Mode::Maintenance {
            enabled_config(&self.data_dir)?;
            Err(crate::cmd::maintenance::denied())
        } else {
            Ok(())
        }
    }
}

#[tool_router(router = library_reads)]
impl Server {
    #[tool(
        description = "Find meetings whose title, tags or participants match. Each result has the meeting ID, start time, duration and tags. With page_size, one page: results, revision and next_cursor (absent on the last page).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search_meetings(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SearchFilters>,
    ) -> CallToolResult {
        let now = self.now();
        reply(
            self.gated(&ctx, move |lib, _| {
                let (page, at) = page_at(p.page_size, p.cursor.as_deref(), now)?;
                let mut q = search_query(&p, None, page.is_some(), at)?;
                q.meeting_order = p.order.map(Into::into).unwrap_or_default();
                let index = Index::open_readonly(lib.root())?;
                let row = |hit: MeetingHit| {
                    let mut v = crate::to_value(&hit)?;
                    v["transcript_uri"] = transcript_uri(&hit.meeting_id).into();
                    Ok(v)
                };
                match page {
                    None => {
                        let results = index
                            .search_meetings(&q)?
                            .into_iter()
                            .map(row)
                            .collect::<Result<Vec<_>>>()?;
                        Ok(json!({ "results": results }))
                    }
                    Some(page) => {
                        query::page_value("results", index.search_meetings_page(&q, &page)?, row)
                    }
                }
            })
            .await,
        )
    }

    #[tool(
        description = "Search what was said using keyword, semantic or hybrid ranking. Results have citable speaker/timestamp references. Omitted mode falls back to keyword when text vectors are unavailable. With page_size, one page: results, revision and next_cursor (absent on the last page).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search_utterances(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SearchUtterancesParams>,
    ) -> CallToolResult {
        reply(self.search(&ctx, p).await)
    }

    #[tool(
        description = "A meeting's record: title, start time, duration, tags, languages, participants, speakers (with names and naming state) and processing state.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_meeting(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<MeetingParams>,
    ) -> CallToolResult {
        reply(
            self.gated_local(&ctx, move |lib, _| {
                let meeting = lib.load_meeting(&meeting_id(&p.meeting_id)?)?;
                meeting_view(&meeting, &lib.people_lookup()?)
            })
            .await,
        )
    }

    #[tool(
        description = "Read the summary and template generation jobs of a meeting, their states and queue positions, and the job_id and run values required by cancel_generation.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn generation_status(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<MeetingParams>,
    ) -> CallToolResult {
        reply(
            self.gated(&ctx, move |lib, _| {
                generation_status(&lib, &meeting_id(&p.meeting_id)?)
            })
            .await,
        )
    }

    #[tool(
        description = "A meeting's transcript as utterances in order: speaker_id (the speaker shown; original_speaker_id when a merged member said it), speaker (its name), who said it (person_id, person_name; attribution is \"line\" when the user corrected that line, else \"speaker\"; assignment of the shown speaker; live_decision when the correction came from a live decision), offset (seconds and hh:mm:ss) and text; optionally one speaker, person or shown name, or a time window.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_transcript(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<TranscriptParams>,
    ) -> CallToolResult {
        reply(
            self.gated_local(&ctx, move |lib, _| transcript(&lib, &p))
                .await,
        )
    }

    #[tool(
        description = "Everyone known, with aliases, e-mails, the number of meetings they are named in, their talk time in seconds and their number of voice samples.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_people(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        reply(
            self.gated(&ctx, move |lib, _| {
                let people = Index::open_readonly(lib.root())?.list_people()?;
                let rows: Vec<Value> = crate::query::people_rows(&lib, &people)?
                    .into_iter()
                    .map(|(row, _)| row)
                    .collect();
                Ok(json!({ "people": rows }))
            })
            .await,
        )
    }

    #[tool(
        description = "One person as CLI `people show` shows them: fields, notes and the revision edits are made against, voiceprints (sample counts per model, kind and strength; never the vectors) and the meetings they appear in (speakers named as them with talk time, lines corrected to them, live decisions naming them that await review), newest first. Read only.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_person(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<PersonParams>,
    ) -> CallToolResult {
        reply(
            self.gated(&ctx, move |lib, _| {
                let person = ops::find_person(&lib, &p.person)?
                    .ok_or_else(|| Error::NotFound(format!("person {}", p.person.trim())))?;
                ops::person_view(&lib, &Index::open_readonly(lib.root())?, &person.id)
            })
            .await,
        )
    }

    #[tool(
        description = "List action items, optionally filtered by owner (me, ID, name, alias or e-mail), status, meeting or date. Names reflect current speaker assignments. With page_size, one page: actions, revision and next_cursor (absent on the last page).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_action_items(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ActionItemsParams>,
    ) -> CallToolResult {
        let now = self.now();
        reply(
            self.gated(&ctx, move |lib, _| {
                let (page, at) = page_at(p.page_size, p.cursor.as_deref(), now)?;
                let q = query::action_query(
                    &lib,
                    &ActionFilters {
                        owner: p.owner.as_deref(),
                        status: p.status.map(Into::into),
                        since: p.since.as_deref(),
                        until: p.until.as_deref(),
                        meeting: p.meeting_id.as_deref(),
                        query: p.query.as_deref(),
                        limit: read_limit(p.limit, page.is_some())?,
                    },
                    at,
                )?;
                let index = Index::open_readonly(lib.root())?;
                match page {
                    None => Ok(json!({ "actions": index.list_actions(&q)? })),
                    Some(page) => {
                        query::page_value("actions", index.list_actions_page(&q, &page)?, |a| {
                            crate::to_value(&a)
                        })
                    }
                }
            })
            .await,
        )
    }

    #[tool(
        description = "List decisions, optionally filtered by enrichment topic, meeting, date or text. Names reflect current speaker assignments.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_decisions(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<DecisionsParams>,
    ) -> CallToolResult {
        let now = self.now();
        reply(
            self.gated(&ctx, move |lib, _| {
                let q = query::decision_query(
                    &DecisionFilters {
                        topic: p.topic.as_deref(),
                        since: p.since.as_deref(),
                        until: p.until.as_deref(),
                        meeting: p.meeting_id.as_deref(),
                        query: p.query.as_deref(),
                        limit: Some(limit(p.limit)?),
                    },
                    now,
                )?;
                Ok(json!({ "decisions": Index::open_readonly(lib.root())?.list_decisions(&q)? }))
            })
            .await,
        )
    }

    #[tool(
        description = "List speaker reviews across meetings. The waiting queue returns speakers, without_transcript and the total dismissed count. Set dismissed to list skipped speakers instead. Same fields as CLI speakers review --json.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_speaker_reviews(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ReviewListParams>,
    ) -> CallToolResult {
        reply(
            self.gated_local(&ctx, move |lib, _| {
                if p.dismissed {
                    let mut rows = ops::dismissed_speakers(&lib)?;
                    let dismissed = rows.len();
                    rows.truncate(p.limit.unwrap_or(usize::MAX));
                    return Ok(json!({ "speakers": rows, "dismissed": dismissed }));
                }
                let (speakers, without_transcript): (Vec<_>, Vec<_>) =
                    ops::review_queue(&lib, p.limit)?
                        .into_iter()
                        .partition(|entry| !entry.without_transcript);
                Ok(json!({
                    "speakers": speakers,
                    "without_transcript": without_transcript,
                    "dismissed": ops::review_dismissed_count(&lib)?,
                }))
            })
            .await,
        )
    }

    #[tool(
        description = "Review recurring voices and possible duplicate profiles across the local library. Returns identification groups, duplicate profiles ranked by voice similarity, and a pending merge if any. Decisions are made in the app or with the people CLI commands.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn people_review(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        reply(
            self.gated_local(&ctx, move |lib, config| {
                crate::cmd::identity_review::snapshot(&lib, &config)
            })
            .await,
        )
    }

    #[tool(
        description = "The people whose stored voice is closest to one speaker's, best first (at most `limit`, 1 to 10, default 3): person_id, name, score (raw voice similarity, not a probability), likely (only the first match, when it would be named automatically) and current_suggestion. A confirmed speaker is compared without the samples taken from this meeting.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn speaker_voice_matches(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<VoiceMatchesParams>,
    ) -> CallToolResult {
        reply(
            self.gated(&ctx, move |lib, config| {
                crate::cmd::read::voice_matches(
                    &lib,
                    &config,
                    &p.meeting_id,
                    &p.speaker_id,
                    p.limit.map(|n| n as usize),
                )
            })
            .await,
        )
    }

    #[tool(
        description = "Check confirmed names against independent voice samples in the local library. Includes previous meetings, possible mismatches, and counts of comparable identifications and unreadable meetings.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn recheck_identifications(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<IdentityChecksParams>,
    ) -> CallToolResult {
        reply(
            self.gated(&ctx, move |lib, config| {
                crate::cmd::identity_checks::snapshot(&lib, &config, p.person.as_deref())
            })
            .await,
        )
    }

    #[tool(
        description = "Pairs of speakers in one meeting that may be the same voice split in two, best first: {meeting_id, model, merge, auto_merge, duplicates: [{a, a_name, b, b_name, score}]} (b sounds like a; a merge would keep a; score is the raw voice similarity, between the calibrated merge and auto_merge thresholds). Empty without a calibrated merge threshold. Merging is left to the user (CLI `speakers merge` or the app).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn speaker_duplicates(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<DuplicatesParams>,
    ) -> CallToolResult {
        reply(
            self.gated(&ctx, move |lib, config| {
                crate::cmd::read::speaker_duplicates(&lib, &config, &p.meeting_id)
            })
            .await,
        )
    }

    #[tool(
        description = "Meeting templates (built-in and the user's): id, name, description, body (the instructions), builtin, overridden, auto (runs after each summary). With meeting_id, also that meeting's results: [{template_id, template_name, created_at, stale, defined}] (a result outlives a deleted template). Running a template is left to the user (CLI `templates run` or the app): it sends the transcript to an LLM provider.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_templates(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ListTemplatesParams>,
    ) -> CallToolResult {
        let data_dir = self.data_dir.clone();
        reply(
            self.gated_local(&ctx, move |lib, config| {
                let meeting = p
                    .meeting_id
                    .as_deref()
                    .map(|m| lib.load_meeting(&meeting_id(m)?))
                    .transpose()?;
                query::templates_view(&data_dir, &config, meeting.as_ref())
            })
            .await,
        )
    }

    #[tool(
        description = "A meeting's result for one template: markdown (names reflect current speaker assignments), template_name, provider, model, requested_model, model_reported, created_at, instructions_sha256 and stale (true when the transcript or names changed after it was generated).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_template_result(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<TemplateResultParams>,
    ) -> CallToolResult {
        reply(
            self.gated_local(&ctx, move |lib, _| {
                query::template_result_view(&lib, &meeting_id(&p.meeting_id)?, &p.template_id)
            })
            .await,
        )
    }

    #[tool(
        description = "Keyword search in template results (every word must match): [{meeting_id, kind: \"template\", template_id, template_name, snippet}]. Read the whole result with get_template_result.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search_templates(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SearchTemplatesParams>,
    ) -> CallToolResult {
        reply(
            self.gated(&ctx, move |lib, _| {
                let hits = query::template_hits(&lib, Some(&p.query), limit(p.limit)?)?;
                Ok(json!({ "results": hits }))
            })
            .await,
        )
    }

    #[tool(
        description = "One meeting as a single document (the same text as CLI `export`): transcript, summary, decisions, action items (a table) and template results, names filled in, stale sections marked. Returns {content, format, sections (included), omitted (asked for but absent)}.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn export_meeting(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ExportParams>,
    ) -> CallToolResult {
        reply(
            self.gated_local(&ctx, move |lib, _| {
                let doc = query::export_doc(
                    &lib,
                    &meeting_id(&p.meeting_id)?,
                    &p.include,
                    p.format.into(),
                    p.timestamps.unwrap_or(true),
                )?;
                Ok(json!({
                    "content": doc.content,
                    "format": p.format,
                    "sections": doc.sections,
                    "omitted": doc.omitted,
                }))
            })
            .await,
        )
    }

    #[tool(
        description = "The live preview of a recording in progress (the active one when no session is given): provisional turns with track, time, text and status preview, unresolved, voice (a provisional \"Voice N\", not a person), suggested (a person suggestion, from the library or from lines the user named in this recording) or named (the user named exactly that line). Suggestions are not identities. Same result as CLI `record live --json`. Use attribute_live_turn with the displayed turn evidence to name or clear a line.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_live_preview(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<LivePreviewParams>,
    ) -> CallToolResult {
        let data_dir = self.data_dir.clone();
        reply(
            self.gated(&ctx, move |lib, _| {
                let (mut client, state) = agent_on_library(&data_dir, lib.root())?;
                let session_id = p
                    .session_id
                    .or_else(|| state.recording.map(|recording| recording.session_id))
                    .ok_or_else(|| {
                        Error::InvalidInput(
                            "no active recording; name a stopped one with --session".into(),
                        )
                    })?;
                client
                    .call(broscribe_ipc::Call::RecordLive { session_id })
                    .map_err(crate::cmd::daemon::call_error)
            })
            .await,
        )
    }

    #[tool(
        description = "Counts-only technical diagnostics, the same report as CLI `diagnostics --json`: software version, build and protocol; meeting counts by status, source, tracks, models per role (counts, never IDs) and echo handling; index, queue and unsaved recording journal counts (capture gaps, dropped frames); BroScribe Agent's health (capture, preview, reader, jobs, index); and platform capabilities with availability, permission and runtime kept apart. Never names, titles, text, tags, times, identifiers, paths or error text: unreadable parts give their error code, unknown values are marked unknown. Read only; loads no model and opens no device.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_diagnostics(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        reply(self.diagnostics(&ctx).await)
    }
}

#[tool_router(router = library_writes)]
impl Server {
    #[tool(
        description = "Edit a saved person's name, aliases, e-mail addresses or notes. Pass the revision from get_person to refuse changes to a field changed since that read. Returns an undo receipt.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn edit_person(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<EditPersonParams>,
    ) -> CallToolResult {
        reply(
            self.gated_write(&ctx, move |lib, _, cancel| {
                let id = broscribe_core::PersonId::parse(&p.person_id)?;
                let spec = ops::PersonEditSpec {
                    expected: p.expected,
                    name: p.name,
                    add_emails: p.add_emails,
                    remove_emails: p.remove_emails,
                    emails: p.emails,
                    aliases: p.aliases,
                    add_aliases: p.add_aliases,
                    remove_aliases: p.remove_aliases,
                    notes: p.notes,
                };
                let outcome = ops::with_index_checked(
                    &lib,
                    || cancel.check(),
                    |tx, index| ops::edit_person(tx, index, &id, &spec),
                )?;
                Ok(json!(outcome))
            })
            .await,
        )
    }

    #[tool(
        description = "Keep a confirmed name after reviewing a possible mismatch. Pass the decision fields returned by recheck_identifications. New voice evidence can bring it back for review.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn keep_identification(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<KeepIdentificationParams>,
    ) -> CallToolResult {
        reply(
            self.gated_write(&ctx, move |lib, config, cancel| {
                let decision = ops::IdentityCheckDecision {
                    meeting_id: MeetingId::parse(&p.meeting_id)?,
                    speaker_id: p.speaker_id.parse()?,
                    person_id: broscribe_core::PersonId::parse(&p.person_id)?,
                    revision: p.revision,
                };
                let model = &config.profile.speaker_embedding;
                lib.write(|tx| {
                    cancel.check()?;
                    ops::keep_identification(tx, &decision, model, config.thresholds(model))
                })?;
                Ok(json!({"kept": decision}))
            })
            .await,
        )
    }

    #[tool(
        description = "Create a name-only local person, with optional email addresses and notes.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn add_person(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<AddPersonParams>,
    ) -> CallToolResult {
        reply(
            self.gated_write(&ctx, move |lib, _, cancel| {
                let spec = ops::NewPersonSpec {
                    name: p.name,
                    emails: p.emails,
                    notes: p.notes,
                };
                let person = ops::with_index_checked(
                    &lib,
                    || cancel.check(),
                    |tx, index| ops::add_person(tx, index, &spec),
                )?;
                Ok(json!({ "person": person }))
            })
            .await,
        )
    }

    #[tool(
        description = "Mark the saved meeting review note at a displayed position and revision as checked.",
        annotations(read_only_hint = false, open_world_hint = false)
    )]
    async fn check_review_note(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<CheckReviewNoteParams>,
    ) -> CallToolResult {
        reply(
            self.gated_write(&ctx, move |lib, _, cancel| {
                let id = meeting_id(&p.meeting_id)?;
                ops::with_index_checked(
                    &lib,
                    || cancel.check(),
                    |tx, index| ops::dismiss_review_note(tx, index, &id, p.position, &p.revision),
                )?;
                Ok(json!({ "meeting_id": id, "checked": p.position }))
            })
            .await,
        )
    }

    #[tool(
        description = "Correct or clear an action item's owner and due date. The edits carry forward on regeneration.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn edit_action(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<EditActionParams>,
    ) -> CallToolResult {
        reply(
            self.gated_write(&ctx, move |lib, _, cancel| {
                let id = meeting_id(&p.meeting_id)?;
                let edit = ops::ActionEdit {
                    owner: p
                        .owner
                        .as_deref()
                        .map(broscribe_core::PersonId::parse)
                        .transpose()?,
                    clear_owner: p.clear_owner,
                    due: p.due.as_deref().map(str::parse).transpose().map_err(|_| {
                        Error::InvalidInput("expected due date in YYYY-MM-DD form".into())
                    })?,
                    clear_due: p.clear_due,
                };
                let action = ops::with_index_checked(
                    &lib,
                    || cancel.check(),
                    |tx, index| ops::edit_action(tx, index, &id, &p.action_id, edit),
                )?;
                Ok(json!({ "meeting_id": id, "action": action }))
            })
            .await,
        )
    }

    #[tool(
        description = "Cancel a queued or running summary or template generation by its job ID and run token.",
        annotations(read_only_hint = false, open_world_hint = false)
    )]
    async fn cancel_generation(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<CancelGenerationParams>,
    ) -> CallToolResult {
        reply(
            self.gated_write(&ctx, move |lib, _, cancel| {
                use broscribe_core::queue::{JobId, Queue};
                let id = JobId::parse(&p.job_id)?;
                lib.write_checked(
                    || cancel.check(),
                    |tx| Queue::open(tx.root())?.cancel_generation(tx, &id, &p.run),
                )?;
                Ok(json!({ "job_id": id, "run": p.run, "cancelled": true }))
            })
            .await,
        )
    }

    #[tool(
        description = "Restore the immediate predecessor of the displayed summary or template result.",
        annotations(read_only_hint = false, open_world_hint = false)
    )]
    async fn restore_generation(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<RestoreGenerationParams>,
    ) -> CallToolResult {
        reply(self.gated_write(&ctx, move |lib, _, cancel| {
            let id = meeting_id(&p.meeting_id)?;
            let expected = chrono::DateTime::parse_from_rfc3339(&p.expected)
                .map_err(|_| Error::InvalidInput("expected result creation time in RFC 3339 form".into()))?;
            let created_at = broscribe_pipeline::enrich::restore_previous(
                &lib, &id, p.template_id.as_deref(), expected, || cancel.check(),
            )?;
            Ok(json!({ "meeting_id": id, "template_id": p.template_id, "created_at": created_at, "restored": true }))
        }).await)
    }

    #[tool(
        description = "Preview what goes, changes and stays when forgetting a person. Set confirm to true to permanently forget them and purge their voiceprints, names and cached directory photos. Confirmed calls report the background recheck, photo purge and vector refresh.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn forget_person(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ForgetParams>,
    ) -> CallToolResult {
        reply(self.forget(&ctx, p, false).await)
    }

    #[tool(
        description = "Preview what goes, changes and stays when forgetting a voice. Set confirm to true to permanently delete the local voiceprints, kept enrollment audio and learned voice evidence. Confirmed names and transcripts stay.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn forget_voice(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ForgetParams>,
    ) -> CallToolResult {
        reply(self.forget(&ctx, p, true).await)
    }

    #[tool(
        description = "Reject a local voice suggestion or automatic assignment for a meeting speaker and save the rejection so that person is no longer offered for that speaker. A confirmed speaker can be changed with name_speaker.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn reject_speaker(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<RejectSpeakerParams>,
    ) -> CallToolResult {
        reply(self.reject(&ctx, p).await)
    }

    #[tool(
        description = "Skip a speaker in the review queue without naming them or changing their voice evidence. Same effect as Can't tell in the app or CLI speakers dismiss.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn dismiss_speaker_review(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ReviewDecisionParams>,
    ) -> CallToolResult {
        reply(self.set_speaker_review_dismissed(&ctx, p, true).await)
    }

    #[tool(
        description = "Clear a speaker's dismissal and return them to the review queue if they still need a decision. Same effect as Restore in the app or CLI speakers restore.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn restore_speaker_review(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ReviewDecisionParams>,
    ) -> CallToolResult {
        reply(self.set_speaker_review_dismissed(&ctx, p, false).await)
    }

    #[tool(
        description = "Confirm who a speaker is. Adds the voice to the person (created when the name is new) and saves the name at once. Other unconfirmed speakers are rechecked in the background, this meeting first, then the rest of the library (`learning`: the queued recheck; `agent_notified` says whether the running agent took it). Nothing else is named by this call. A speaker merged into another names that group's kept speaker (`speaker_id`, with `redirected_from`).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn name_speaker(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<NameSpeakerParams>,
    ) -> CallToolResult {
        reply(self.name(&ctx, p).await)
    }

    #[tool(
        description = "Undo a speaker name carried over from a recording and remove the voice samples it taught. The recording-time line names remain.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn undo_live_speaker_name(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<UndoLiveNameParams>,
    ) -> CallToolResult {
        let mut result = self
            .gated_write(&ctx, move |lib, _, cancel| {
                let meeting = MeetingId::parse(&p.meeting_id)?;
                let speaker = SpeakerId::parse(&p.speaker_id)?;
                let undone = ops::with_index_checked(
                    &lib,
                    || cancel.check(),
                    |tx, index| crate::cmd::undo_recording_name(tx, index, &meeting, speaker),
                )?;
                Ok(json!({"meeting_id": meeting, "speaker_id": speaker, "undone": undone}))
            })
            .await;
        if let Ok(value) = &mut result {
            value["agent_notified"] = self.kick_agent().await.into();
        }
        reply(result)
    }

    #[tool(
        description = "Add and remove tags on a meeting; returns the meeting's tags.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tag_meeting(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<TagParams>,
    ) -> CallToolResult {
        reply(
            self.gated_write(&ctx, move |lib, _, cancel| {
                let id = meeting_id(&p.meeting_id)?;
                let tags = ops::with_index_checked(
                    &lib,
                    || cancel.check(),
                    |tx, index| ops::tag(tx, index, &id, &p.add, &p.remove),
                )?;
                Ok(json!({ "meeting_id": id, "tags": tags }))
            })
            .await,
        )
    }

    #[tool(
        description = "Rename a meeting. The title is the user's: calendar and window titles never replace it. A meeting still being recorded also updates the recording's title (`session_id`; `agent_notified` says whether the running agent took it). Same result as CLI `title --json`.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_meeting_title(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<TitleParams>,
    ) -> CallToolResult {
        let data_dir = self.data_dir.clone();
        reply(
            self.gated_write(&ctx, move |lib, _, cancel| {
                let id = meeting_id(&p.meeting_id)?;
                let title = crate::query::checked_title(&p.title)?;
                crate::cmd::lifecycle::set_title_checked(&lib, &data_dir, &id, &title, || {
                    cancel.check()
                })
            })
            .await,
        )
    }

    #[tool(
        description = "Set an action item's status to open, done or dropped. Records this as a user decision preserved by re-enrichment.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn update_action_status(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ActionStatusParams>,
    ) -> CallToolResult {
        reply(
            self.gated_write(&ctx, move |lib, _, cancel| {
                let id = meeting_id(&p.meeting_id)?;
                let action = ops::with_index_checked(
                    &lib,
                    || cancel.check(),
                    |tx, index| {
                        ops::set_action_status(tx, index, &id, &p.action_id, p.status.into())
                    },
                )?;
                Ok(json!({ "meeting_id": id, "action": action }))
            })
            .await,
        )
    }
}

impl Server {
    /// Runs `f` on the library off the async threads with the current
    /// configuration, unless the request was cancelled before it started or
    /// `[mcp] enabled` (re-read now) refuses it.
    async fn gated<T: Send + 'static>(
        &self,
        ctx: &RequestContext<RoleServer>,
        f: impl FnOnce(Library, Config) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.require_normal()?;
        self.gated_local(ctx, f).await
    }

    /// Conservative allowlist: files/config only, no index or provider work.
    async fn gated_local<T: Send + 'static>(
        &self,
        ctx: &RequestContext<RoleServer>,
        f: impl FnOnce(Library, Config) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let lib = self.lib.clone();
        let data_dir = self.data_dir.clone();
        let cancel = Cancel::of(ctx);
        blocking(move || {
            cancel.check()?;
            let config = enabled_config(&data_dir)?;
            f(lib, config)
        })
        .await
    }

    /// [`Server::gated`] for a mutation: `f` also gets the request's
    /// [`Cancel`] and checks it again once the library lock is held, before
    /// its first durable change (`ops::with_index_checked`, the checked
    /// title), so a request cancelled while it waited for the lock changes
    /// nothing. Once admitted, `f` runs to its end even when the request is
    /// cancelled or its connection ends: the runtime then drops the handler
    /// waiting for it, never blocking work already started. What must follow
    /// a commit whatever happens to the connection belongs in `f`.
    async fn set_speaker_review_dismissed(
        &self,
        ctx: &RequestContext<RoleServer>,
        p: ReviewDecisionParams,
        dismissed: bool,
    ) -> Result<Value> {
        self.gated_write(ctx, move |lib, _, cancel| {
            let meeting = meeting_id(&p.meeting_id)?;
            let speaker = speaker_id(&p.speaker_id)?;
            let outcome = ops::with_index_checked(
                &lib,
                || cancel.check(),
                |tx, index| {
                    let fence = ops::SpeakerFence::of(&tx.library().load_meeting(&meeting)?);
                    let decide = if dismissed {
                        ops::dismiss_review
                    } else {
                        ops::restore_review
                    };
                    decide(tx, index, &meeting, speaker, &fence)
                },
            )?;
            crate::to_value(&outcome)
        })
        .await
    }

    async fn gated_write<T: Send + 'static>(
        &self,
        ctx: &RequestContext<RoleServer>,
        f: impl FnOnce(Library, Config, Cancel) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.require_normal()?;
        let lib = self.lib.clone();
        let data_dir = self.data_dir.clone();
        let cancel = Cancel::of(ctx);
        #[cfg(test)]
        let hooks = self.hooks.clone();
        #[cfg(test)]
        let _waiting = OnDrop(hooks.left.clone());
        blocking(move || {
            cancel.check()?;
            #[cfg(test)]
            if let Some(admitted) = &hooks.admitted {
                admitted();
            }
            let result = enabled_config(&data_dir).and_then(|config| f(lib, config, cancel));
            #[cfg(test)]
            if let Some(ended) = &hooks.ended {
                ended(result.as_ref().err().map(Error::code));
            }
            result
        })
        .await
    }

    /// `get_diagnostics` in two phases, each off the async threads behind
    /// the enabled check: the library, journal and model metadata, then the
    /// agent's answer and the capability probes. A cancelled request never
    /// starts the next phase.
    async fn diagnostics(&self, ctx: &RequestContext<RoleServer>) -> Result<DiagnosticsReport> {
        let mut progress = Progress::of(ctx, 2);
        progress.phase("reading library metadata").await;
        let data_dir = self.data_dir.clone();
        let local = until_cancelled(
            ctx,
            self.gated(ctx, move |lib, config| {
                Ok(diagnostics::collect_local(
                    Some(lib.root()),
                    Ok(&config),
                    &data_dir,
                ))
            }),
        )
        .await?;
        progress.phase("asking BroScribe Agent").await;
        let data_dir = self.data_dir.clone();
        until_cancelled(
            ctx,
            self.gated(ctx, move |_, _| {
                let agent = diagnostics::ask_agent(&data_dir);
                if agent
                    .error()
                    .is_some_and(|error| error.code() == broscribe_core::maintenance::ERROR_CODE)
                {
                    return Err(crate::cmd::maintenance::denied());
                }
                Ok(diagnostics::finish(local, &agent))
            }),
        )
        .await
    }

    /// `search_utterances` in two phases: preparing the query (the text
    /// model may embed it) and reading the index, which never starts once
    /// the request is cancelled.
    async fn search(
        &self,
        ctx: &RequestContext<RoleServer>,
        p: SearchUtterancesParams,
    ) -> Result<Value> {
        let mut progress = Progress::of(ctx, 2);
        progress.phase("preparing the query").await;
        let secrets = Arc::clone(&self.secrets);
        let execution = self.execution.clone();
        let kept = Arc::clone(&self.query_vectors);
        let data_dir = self.data_dir.clone();
        let now = self.now();
        let prepared = until_cancelled(
            ctx,
            self.gated(ctx, move |lib, config| {
                prepare_search(
                    &lib,
                    &config,
                    &data_dir,
                    secrets.as_ref(),
                    &execution,
                    &kept,
                    p,
                    now,
                )
            }),
        )
        .await?;
        progress.phase("reading the index").await;
        let kept = Arc::clone(&self.query_vectors);
        until_cancelled(
            ctx,
            self.gated(ctx, move |lib, _| read_search(&lib, &kept, prepared)),
        )
        .await
    }

    async fn reject(
        &self,
        ctx: &RequestContext<RoleServer>,
        p: RejectSpeakerParams,
    ) -> Result<Value> {
        let id = meeting_id(&p.meeting_id)?;
        let speaker = speaker_id(&p.speaker_id)?;
        let meeting = id.clone();
        let outcome = self
            .gated_write(ctx, move |lib, config, cancel| {
                let model = &config.profile.speaker_embedding;
                ops::with_index_checked(
                    &lib,
                    || cancel.check(),
                    |tx, index| {
                        ops::reject_suggestion(
                            tx,
                            index,
                            &meeting,
                            speaker,
                            p.person.as_deref(),
                            model,
                            config.thresholds(model),
                        )
                    },
                )
            })
            .await?;
        let mut value = crate::to_value(&outcome)?;
        value["meeting_id"] = json!(id);
        if let Some(code) = self.refresh_vectors(vec![id.clone()]).await {
            value["vectors_error"] = code.into();
        }
        self.report_retention(id, &mut value).await?;
        Ok(value)
    }

    /// Forgets a person and purges their cached photos in one blocking work:
    /// the purge starts once the forget committed and the library lock is
    /// released (it takes the integrations' own locks), and ends before that
    /// work returns, so a cancelled request or an ended connection cannot
    /// skip it. A partial or refused purge is reported beside the forget,
    /// never as its failure. Then the windows the forget changed are embedded
    /// (`vectors_error`) and the speaker recheck it requested is kicked; if
    /// the connection ends first, those windows stay pending in the index and
    /// the recheck stays queued.
    async fn forget(
        &self,
        ctx: &RequestContext<RoleServer>,
        p: ForgetParams,
        voice_only: bool,
    ) -> Result<Value> {
        self.require_normal()?;
        if !p.confirm {
            return self
                .gated(ctx, move |lib, _| {
                    crate::cmd::forget_preview(&lib, &p.person, voice_only)
                })
                .await;
        }
        let mut progress = Progress::of(ctx, 2);
        progress
            .phase(if voice_only {
                "forgetting the voice"
            } else {
                "forgetting the person and purging cached photos"
            })
            .await;
        let data_dir = self.data_dir.clone();
        let (outcome, purge) = self
            .gated_write(ctx, move |lib, _, cancel| {
                let outcome = ops::with_index_checked(
                    &lib,
                    || cancel.check(),
                    |tx, index| {
                        if voice_only {
                            crate::cmd::forget_voice(tx, index, &p.person)
                        } else {
                            ops::forget_person(tx, index, &p.person)
                        }
                    },
                )?;
                // Committed and the library released: no integration lock is
                // taken under it.
                let purge = (!voice_only).then(|| {
                    broscribe_integrations::forget_person_photos(
                        &data_dir,
                        outcome.person_id.as_str(),
                    )
                });
                Ok((outcome, purge))
            })
            .await?;
        // Committed: what follows runs even if the request is cancelled.
        let mut value = crate::to_value(&outcome)?;
        match purge {
            Some(Ok(purge)) => value["photos"] = crate::to_value(&purge)?,
            // Refused before touching any store: what remains is unknown, so
            // it reads as partial; the error is sanitized (never a path).
            Some(Err(e)) => {
                value["photos"] = json!({ "status": PhotoPurgeStatus::Partial });
                value["photos_error"] = crate::to_value(&e.public())?;
            }
            None => {}
        }
        progress.phase("updating search vectors").await;
        if !outcome.meetings_changed.is_empty()
            && let Some(code) = self.refresh_vectors(outcome.meetings_changed.clone()).await
        {
            value["vectors_error"] = code.into();
        }
        if !outcome.learning.is_empty() {
            value["agent_notified"] = self.kick_agent().await.into();
        }
        Ok(value)
    }

    /// Asks the running agent to run the speaker recheck core requested;
    /// `false` without an agent (the request stays queued for the next
    /// worker) or when it could not be reached.
    async fn kick_agent(&self) -> bool {
        let data_dir = self.data_dir.clone();
        let library = self.lib.root().to_owned();
        blocking(move || kick_library_agent(&data_dir, &library))
            .await
            .unwrap_or(false)
    }

    /// Embeds the new windows of `meetings` with the configured text model
    /// (a follow-up of a committed write); the error code when that failed.
    async fn refresh_vectors(&self, meetings: Vec<MeetingId>) -> Option<&'static str> {
        let lib = self.lib.clone();
        let data_dir = self.data_dir.clone();
        let secrets = Arc::clone(&self.secrets);
        let execution = self.execution.clone();
        let result = blocking(move || {
            let config = Config::load(&data_dir)?;
            crate::cmd::refresh_vectors(
                &lib,
                &config,
                &data_dir,
                secrets.as_ref(),
                &execution,
                Some(&meetings),
            )
        })
        .await;
        result.err().map(|e| e.code())
    }

    /// Confirmation samples from the [`Deps`] thread: waiting for queue
    /// space and for the answer both end when the request is cancelled, and
    /// a job cancelled while queued is never computed.
    async fn samples(
        &self,
        ctx: &RequestContext<RoleServer>,
        meeting: MeetingId,
        speaker: SpeakerId,
        model: String,
    ) -> Result<Vec<ConfirmationSample>> {
        let (reply_to, answer) = oneshot::channel();
        let job = WorkerJob::Samples(SampleJob {
            meeting,
            speaker,
            model,
            cancel: Cancel::of(ctx),
            reply: reply_to,
        });
        until_cancelled(ctx, async {
            self.jobs
                .send(job)
                .await
                .map_err(|_| Error::Internal("the sample worker has stopped".into()))?;
            answer
                .await
                .map_err(|_| Error::Internal("the sample worker dropped the request".into()))?
        })
        .await
    }

    /// Retention after a committed naming or rejection, on the [`Deps`] thread.
    async fn retention(&self, meeting: MeetingId) -> Result<RetentionReport> {
        let (send, answer) = oneshot::channel();
        self.jobs
            .send(WorkerJob::Retention {
                meeting,
                reply: send,
            })
            .await
            .map_err(|_| Error::Internal("the retention worker has stopped".into()))?;
        answer
            .await
            .map_err(|_| Error::Internal("the retention worker dropped the request".into()))?
    }

    async fn report_retention(&self, meeting: MeetingId, value: &mut Value) -> Result<()> {
        match self.retention(meeting).await {
            Ok(report) => value["retention"] = crate::to_value(&report)?,
            Err(error) => value["retention_error"] = error.code().into(),
        }
        Ok(())
    }

    async fn name(&self, ctx: &RequestContext<RoleServer>, p: NameSpeakerParams) -> Result<Value> {
        self.require_normal()?;
        let mut progress = Progress::of(ctx, 4);
        progress.phase("reading the speaker").await;
        let meeting = meeting_id(&p.meeting_id)?;
        let speaker = speaker_id(&p.speaker_id)?;

        // A merged member names its group's kept speaker: its samples are
        // computed first, outside the lock, and the fence refuses the naming
        // if the speakers were regrouped meanwhile.
        let read = meeting.clone();
        let revision = p.revision.clone();
        let (model, fence, kept) = self
            .gated(ctx, move |lib, config| {
                let m = lib.load_meeting(&read)?;
                if let Some(expected) = revision {
                    ops::check_speaker_revision(&m, speaker, &expected)?;
                }
                if m.speaker(speaker).is_none() {
                    return Err(Error::NotFound(format!(
                        "speaker {speaker} in meeting {read}"
                    )));
                }
                Ok((
                    config.profile.speaker_embedding.clone(),
                    ops::SpeakerFence::of(&m),
                    ops::effective_speaker(&m, speaker),
                ))
            })
            .await?;
        progress.phase("computing voice samples").await;
        let samples = self
            .samples(ctx, meeting.clone(), kept, model.clone())
            .await?;

        progress.phase("saving the name").await;
        let saved = meeting.clone();
        let mut value = self
            .gated_write(ctx, move |lib, _, cancel| {
                let outcome = ops::with_index_checked(
                    &lib,
                    || cancel.check(),
                    |tx, index| {
                        let current = tx.load_meeting(&saved)?;
                        ops::check_speaker_fence(&current, &fence)?;
                        if let Some(expected) = &p.revision {
                            ops::check_speaker_revision(&current, speaker, expected)?;
                        }
                        ops::name_speaker(tx, index, &saved, speaker, &p.person, samples, &model)
                    },
                )?;
                let person_name = lib.load_person(&outcome.person_id)?.name;
                let mut v = crate::to_value(&outcome)?;
                v["meeting_id"] = saved.as_str().into();
                v["person_name"] = person_name.into();
                Ok(v)
            })
            .await?;

        // Committed: what follows runs whatever happens to the request.
        progress.phase("updating search and retention").await;
        // Members separated from their group change what the transcript
        // shows: embed the new windows.
        if value["separated"].as_array().is_some_and(|s| !s.is_empty())
            && let Some(code) = self.refresh_vectors(vec![meeting.clone()]).await
        {
            value["vectors_error"] = code.into();
        }
        // Retention follows the completed naming transaction, never inference.
        self.report_retention(meeting, &mut value).await?;
        value["agent_notified"] = self.kick_agent().await.into();
        Ok(value)
    }
}

/// MCP maintenance permits only the same proven reads as the handlers.
fn maintenance_tool(request: &rmcp::model::CallToolRequestParams) -> bool {
    match request.name.as_ref() {
        "get_meeting"
        | "get_transcript"
        | "list_speaker_reviews"
        | "people_review"
        | "list_templates"
        | "get_template_result"
        | "export_meeting"
        | "get_about"
        | "get_version"
        | "get_maintenance_status"
        | "get_daemon_status"
        | "get_queue_status"
        | "get_identity_status"
        | "show_meeting_record"
        | "find_transcript"
        | "get_template_definition" => true,
        "get_app_setting" => matches!(
            request
                .arguments
                .as_ref()
                .and_then(|args| args.get("setting"))
                .and_then(Value::as_str),
            Some("open_at_login" | "live_identity" | "uncalibrated_me_suggestions")
        ),
        _ => false,
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<rmcp::model::CallToolResponse, ErrorData> {
        let access = self.gated_local(&context, |_, _| Ok(())).await;
        if let Err(error) = access {
            return Ok(reply::<Value>(Err(error)).into());
        }
        if !maintenance_tool(&request)
            && let Err(error) = self.require_normal()
        {
            return Ok(reply::<Value>(Err(error)).into());
        }
        let call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(call).await
    }

    fn get_info(&self) -> ServerConfig {
        let names = |router: ToolRouter<Self>| {
            let mut names: Vec<_> = router
                .list_all()
                .into_iter()
                .map(|tool| tool.name.into_owned())
                .collect();
            names.sort_unstable();
            names.join(", ")
        };
        let read = names(Self::read_tools());
        let write = names(Self::write_tools());
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("broscribe", env!("CARGO_PKG_VERSION")))
        .with_instructions(format!(
            "BroScribe meeting memory. Read tools include {read}. Write tools include {write}. \
             search_meetings, search_utterances and list_action_items page with page_size and \
             the next_cursor of the previous page. If cursor_stale is returned, start again. \
             Live suggestions are provisional. Technical diagnostics contain only counts and \
             capability information. Delete meetings, merge speakers and start generation \
             through the CLI or app. \
             Long calls report progress when given a progress token and stop when cancelled. \
             Cite results by meeting ID, offset and speaker. Read the transcript resource at \
             meeting://<id>/transcript.md."
        ))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new(URI_TEMPLATE, "meeting-transcript")
                .with_title("Meeting transcript")
                .with_description("The meeting's rendered meeting.md: frontmatter and transcript.")
                .with_mime_type(MARKDOWN),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri;
        let read_uri = uri.clone();
        match self
            .gated_local(&context, move |lib, _| {
                read_transcript_resource(&lib, &read_uri)
            })
            .await
        {
            Ok(md) => Ok(ReadResourceResult::new(vec![
                ResourceContents::text(md, uri).with_mime_type(MARKDOWN),
            ])
            .into()),
            Err(e) => Err(protocol_error(&e)),
        }
    }
}

// ---- helpers ----

/// Fence commands and observations to the authenticated peer on this connection.
fn agent_on_library(
    data_dir: &Path,
    library: &Path,
) -> Result<(
    broscribe_platform_native::ipc::Client,
    broscribe_ipc::DaemonState,
)> {
    let mut client = crate::cmd::daemon::connect(data_dir, std::time::Duration::from_secs(5))?
        .ok_or_else(|| {
            Error::DaemonUnavailable(
                "BroScribe Agent isn't running; run `broscribe daemon start`".into(),
            )
        })?;
    let state = client
        .call(broscribe_ipc::Call::Status)
        .map_err(crate::cmd::daemon::call_error)?;
    let state = serde_json::from_value::<broscribe_ipc::DaemonState>(state)
        .map_err(|_| Error::Internal("BroScribe Agent returned an invalid state".into()))?;
    check_agent_library(library, &state)?;
    Ok((client, state))
}

pub(crate) fn check_agent_library(
    library: &Path,
    state: &broscribe_ipc::DaemonState,
) -> Result<()> {
    let reported = Path::new(&state.library);
    if !reported.is_absolute() || state.library.contains('\u{fffd}') {
        return Err(library_changed());
    }
    check_library_path(library, reported)
}

pub(crate) fn check_library_path(library: &Path, reported: &Path) -> Result<()> {
    let expected = library.canonicalize().ok();
    let advertised = reported.canonicalize().ok();
    if expected.is_none() || expected != advertised {
        return Err(library_changed());
    }
    Ok(())
}

fn library_changed() -> Error {
    Error::Service {
        code: "library_changed",
        message:
            "BroScribe Agent is using a different library. Reconnect to use the agent’s library."
                .into(),
    }
}

pub(crate) fn kick_library_agent(data_dir: &Path, library: &Path) -> Result<bool> {
    use broscribe_platform_native::ipc::{CancelToken, Endpoint};
    let client = match agent_on_library(data_dir, library) {
        Ok((client, _)) => client,
        Err(Error::DaemonUnavailable(_)) => return Ok(false),
        Err(error) => return Err(error),
    };
    client
        .call_connected_mutating(
            &Endpoint::new(data_dir),
            broscribe_ipc::Call::KickQueue,
            &CancelToken::new(),
        )
        .map_err(crate::cmd::daemon::call_error)?;
    Ok(true)
}

pub(crate) fn recording_call(ctx: &crate::Ctx<'_>, call: broscribe_ipc::Call) -> Result<Value> {
    use broscribe_platform_native::ipc::{CallError, CancelToken, Endpoint};

    let Some(library) = &ctx.request_library else {
        return crate::cmd::daemon::required_call(ctx, call);
    };
    ctx.check_request_cancelled()?;
    let (mut client, state) = agent_on_library(ctx.data_dir(), library)?;
    ctx.check_request_cancelled()?;
    if matches!(call, broscribe_ipc::Call::Status) {
        return crate::to_value(&state);
    }
    let result = if call.is_mutating() {
        let endpoint = Endpoint::new(ctx.data_dir());
        let cancel = ctx
            .request_cancel
            .clone()
            .map(CancelToken::from_flag)
            .unwrap_or_default();
        client.call_connected_mutating(&endpoint, call, &cancel)
    } else {
        client.call(call)
    };
    result.map_err(|error| match error {
        CallError::NotSent(error) if error.kind() == std::io::ErrorKind::Interrupted => {
            Error::Service {
                code: "cancelled",
                message: "cancelled before the agent received the request".into(),
            }
        }
        other => crate::cmd::daemon::call_error(other),
    })
}

/// Runs blocking work (files, SQLite, the library lock) off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Internal(format!("MCP worker failed: {e}")))?
}

/// A tool result: the value as structured content, or a tool error.
fn reply<T: Serialize>(result: Result<T>) -> CallToolResult {
    match result.and_then(|v| crate::to_value(&v)) {
        Ok(v) => CallToolResult::structured(v),
        Err(e) => tool_error(&e),
    }
}

/// `<code>: <message>` as text, the CLI error envelope as structured content.
fn tool_error(e: &Error) -> CallToolResult {
    tracing::debug!(code = e.code(), "tool failed: {e}");
    let mut result = CallToolResult::structured_error(crate::error_envelope(e));
    result.content = vec![ContentBlock::text(format!("{}: {e}", e.code()))];
    result
}

/// A protocol error for resource reads, the code leading the message.
fn protocol_error(e: &Error) -> ErrorData {
    let message = format!("{}: {e}", e.code());
    let data = Some(crate::error_envelope(e));
    match e {
        Error::NotFound(_) => ErrorData::resource_not_found(message, data),
        Error::InvalidInput(_) => ErrorData::invalid_params(message, data),
        _ => ErrorData::internal_error(message, data),
    }
}

fn transcript_uri(id: &MeetingId) -> String {
    format!("{URI_PREFIX}{id}{URI_SUFFIX}")
}

/// The current config, or `mcp_disabled` unless `[mcp] enabled = true`. Read
/// for every request, so turning MCP off stops a running server's access.
pub(crate) fn enabled_config(data_dir: &Path) -> Result<Config> {
    let config = Config::load(data_dir)?;
    if config.mcp_enabled() {
        Ok(config)
    } else {
        Err(Error::McpDisabled(
            "`broscribe mcp enable` turns it on".into(),
        ))
    }
}

/// A result count: 1 to [`MAX_PAGE_SIZE`], [`DEFAULT_LIMIT`] when omitted.
fn limit(n: Option<u32>) -> Result<usize> {
    let Some(n) = n else {
        return Ok(DEFAULT_LIMIT);
    };
    usize::try_from(n)
        .ok()
        .filter(|n| (1..=MAX_PAGE_SIZE).contains(n))
        .ok_or_else(|| Error::InvalidInput(format!("limit must be 1 to {MAX_PAGE_SIZE}")))
}

/// The query limit of a read that may be paged: a page takes its size from
/// the request, so it has none (and `limit` is refused beside it).
fn read_limit(n: Option<u32>, paged: bool) -> Result<Option<usize>> {
    match (paged, n) {
        (false, n) => limit(n).map(Some),
        (true, None) => Ok(None),
        (true, Some(_)) => Err(Error::InvalidInput(
            "give limit or page_size, not both".into(),
        )),
    }
}

/// The page a read asks for at `now` (none without `page_size`) and the
/// instant its relative dates resolve at: `now` for a first page or an
/// unpaged read, the first page's anchor for a continuation, so every page
/// of one read has the same date window, whichever front end asks.
fn page_at(
    page_size: Option<u32>,
    cursor: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(Option<CursorRequest>, DateTime<Utc>)> {
    // Lossless: `usize` has at least 32 bits on every target.
    let page = query::page_request(page_size.map(|n| n as usize), cursor, now)?;
    let at = query::resolve_at(page.as_ref(), now);
    Ok((page, at))
}

/// A search ready for the index: the query (with its mode and vector), the
/// page asked for and the note to report.
struct PreparedSearch {
    q: SearchQuery,
    page: Option<CursorRequest>,
    note: Option<String>,
}

/// Parses the search, its relative dates resolved at `now` (a continuation:
/// at its first page's anchor), and settles its mode. A continued semantic
/// or hybrid page reuses the vector and mode its cursor was issued with
/// (when this server still keeps it and the configured text model is still
/// that one) when the request names that mode or none; naming another mode
/// is `cursor_mismatch`, as the index answers for a cursor it no longer
/// matches. Otherwise the configured text model embeds the text, with the
/// requesting call's [`ExecutionContext`].
#[allow(clippy::too_many_arguments)]
fn prepare_search(
    lib: &Library,
    config: &Config,
    data_dir: &Path,
    secrets: &dyn SecretStore,
    execution: &ExecutionContext,
    kept: &QueryVectors,
    p: SearchUtterancesParams,
    now: DateTime<Utc>,
) -> Result<PreparedSearch> {
    let (page, at) = page_at(p.filters.page_size, p.filters.cursor.as_deref(), now)?;
    let mut q = search_query(&p.filters, p.meeting_id.as_deref(), page.is_some(), at)?;
    let requested = p.mode.map(Into::into);
    let note = if q.text.is_some() && requested != Some(SearchMode::Keyword) {
        let model = vectors::text_model(config, data_dir, secrets, execution)?;
        let reused = page
            .as_ref()
            .and_then(|page| page.after.as_ref())
            .zip(model.embedder())
            .and_then(|(cursor, embedder)| kept.get(cursor, embedder.model_key()));
        match reused {
            Some(k) if requested.is_some_and(|mode| mode != k.mode) => {
                return Err(Error::Service {
                    code: CURSOR_MISMATCH,
                    message: "this page cursor belongs to a search in another mode".into(),
                });
            }
            Some(k) => {
                q.mode = k.mode;
                q.query_vector = Some(k.vector);
                k.note
            }
            None => vectors::prepare_search(lib, &model, &mut q, requested)?,
        }
    } else {
        q.mode = SearchMode::Keyword;
        None
    };
    Ok(PreparedSearch { q, page, note })
}

fn read_search(lib: &Library, kept: &QueryVectors, s: PreparedSearch) -> Result<Value> {
    let index = Index::open_readonly(lib.root())?;
    let mut value = match &s.page {
        None => {
            let results: Vec<UtteranceHit> = index
                .search(&s.q)?
                .into_iter()
                .map(UtteranceHit::of)
                .collect();
            json!({ "results": results })
        }
        Some(page) => {
            let page = index.search_page(&s.q, page)?;
            if let (Some(cursor), Some(vector)) = (&page.next_cursor, &s.q.query_vector) {
                kept.keep(
                    cursor.clone(),
                    KeptVector {
                        mode: s.q.mode,
                        vector: vector.clone(),
                        note: s.note.clone(),
                    },
                );
            }
            query::page_value("results", page, |hit| {
                crate::to_value(&UtteranceHit::of(hit))
            })?
        }
    };
    value["mode"] = crate::to_value(&s.q.mode)?;
    value["note"] = json!(s.note);
    Ok(value)
}

/// The [`SearchQuery`] of `search_meetings` and `search_utterances`, its
/// relative dates resolved at `at`.
fn search_query(
    f: &SearchFilters,
    meeting: Option<&str>,
    paged: bool,
    at: DateTime<Utc>,
) -> Result<SearchQuery> {
    query::search_query(
        &query::SearchFilters {
            query: f.query.as_deref(),
            person: f.person.as_deref(),
            tag: f.tag.as_deref(),
            topic: f.topic.as_deref(),
            app: f.app.as_deref(),
            source: f.source.map(Into::into),
            since: f.since.as_deref(),
            until: f.until.as_deref(),
            meeting,
            limit: read_limit(f.limit, paged)?,
        },
        at,
    )
}

fn generation_status(lib: &Library, id: &MeetingId) -> Result<Value> {
    use broscribe_core::queue::{ENRICH_KIND, Queue, TEMPLATE_KIND, job_refers_to};

    let meeting = lib.load_meeting(id)?;
    let mut jobs = Vec::new();
    let template_prefix = format!("{id}/");
    if let Some(queue) = Queue::open_existing(lib.root())? {
        let snapshot = queue.snapshot()?;
        let ranks = snapshot.ranks();
        for job in &snapshot.jobs {
            let template = match job.kind.as_str() {
                TEMPLATE_KIND => job.key.strip_prefix(&template_prefix),
                ENRICH_KIND => None,
                _ => continue,
            };
            if template.is_none() && !job_refers_to(job, &meeting) {
                continue;
            }
            jobs.push(json!({
                "job_id": job.id,
                "run": job.generation_run,
                "kind": job.kind,
                "template_id": job.input.get("template_id").and_then(Value::as_str).or(template),
                "state": job.state.as_str(),
                "attempts": job.attempts,
                "error": job.error,
                "jobs_ahead": ranks.get(&job.id).copied().unwrap_or(0),
                "created_at": job.created_at,
                "updated_at": job.updated_at,
            }));
        }
    }
    Ok(json!({ "meeting_id": id, "jobs": jobs }))
}

fn meeting_view(m: &Meeting, people: &PeopleLookup) -> Result<Value> {
    let speakers: Vec<Value> = m
        .speakers
        .iter()
        .map(|s| {
            let suggestion = s.suggestion.as_ref();
            json!({
                "speaker_id": s.id.to_string(),
                "name": render::speaker_label(m, Some(s.id), people),
                "person_id": s.named_person(),
                "assignment": s.assignment,
                "revision": ops::speaker_revision(m, s),
                "score": s.score,
                "talk_s": s.talk_s,
                "merged_into": ops::is_grouped_member(m, s.id)
                    .then(|| ops::effective_speaker(m, s.id).to_string()),
                "review_dismissed": ops::review_dismissed(m, s.id),
                "suggested_person_id": suggestion.map(|g| &g.person_id),
                "suggested_name": suggestion.and_then(|g| people.display_name(&g.person_id)),
                "suggested_score": suggestion.map(|g| g.score),
                "live_promotion": s.live_promotion,
                "live_suggestion_lines": s.live_suggestion_lines,
                "identity_conflict": s.identity_conflict,
            })
        })
        .collect();
    Ok(json!({
        "meeting_id": m.id,
        "title": m.title,
        "started_at": m.started_at,
        "duration_s": m.duration_s,
        "source": m.source,
        "app": m.app,
        "tags": m.effective_tags(),
        "languages": m.languages,
        "language_effective": m.language_effective,
        "participants": m.participants,
        "speakers": speakers,
        "speaker_groups": m.speaker_groups,
        "not_same": m.not_same,
        "recording": m.recording.as_ref().map(|r| json!({ "state": r.state, "handling": r.handling })),
        "status": m.processing.status,
        "models": m.models,
        "review_notes": m.review_notes,
        "live_attributions": m.live_attributions,
        "review_note_revisions": m.review_notes.iter().map(ops::review_note_revision).collect::<Vec<_>>(),
        "enrichment": query::enrichment_view(m),
        "transcript_uri": transcript_uri(&m.id),
    }))
}

/// `get_transcript`: core's one transcript projection (order, shown
/// speaker, label, attribution and filters, as the CLI `transcript` reads
/// it), in MCP field names.
fn transcript(lib: &Library, p: &TranscriptParams) -> Result<Value> {
    let meeting = lib.load_meeting(&meeting_id(&p.meeting_id)?)?;
    let people = lib.people_lookup()?;
    let filter = TranscriptFilter {
        who: p
            .speaker
            .as_deref()
            .map(|s| transcript::resolve_who(lib, &meeting, &people, s))
            .transpose()?,
        from_s: p
            .from
            .as_deref()
            .map(crate::args::parse_clock)
            .transpose()?,
        to_s: p.to.as_deref().map(crate::args::parse_clock).transpose()?,
    };
    let utterances: Vec<Value> = transcript::transcript(&meeting, &people, &filter)
        .iter()
        .map(|s| {
            let mut u = json!({
                "meeting_id": meeting.id,
                "segment_id": s.segment_id,
                "speaker_id": s.speaker.map(|id| id.to_string()),
                "speaker": s.speaker_label,
                "start": s.start,
                "end": s.end,
                "at": render::format_hms(s.start),
                "text": s.text,
            });
            if let Some(original) = s.original_speaker {
                u["original_speaker_id"] = original.to_string().into();
            }
            query::provenance(s, &mut u);
            u
        })
        .collect();
    Ok(json!({
        "meeting_id": meeting.id,
        "title": meeting.title,
        "started_at": meeting.started_at,
        "utterances": utterances,
    }))
}

/// The rendered `meeting.md` of a `meeting://<id>/transcript.md` URI.
fn read_transcript_resource(lib: &Library, uri: &str) -> Result<String> {
    let id = uri
        .strip_prefix(URI_PREFIX)
        .and_then(|rest| rest.strip_suffix(URI_SUFFIX))
        .ok_or_else(|| {
            Error::InvalidInput(format!(
                "unknown resource {uri:?} (expected {URI_TEMPLATE})"
            ))
        })?;
    let meeting = lib.load_meeting(&meeting_id(id)?)?;
    render::render_meeting_md(&meeting, &lib.people_lookup()?)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    use broscribe_core::schema::{
        Centroid, ProcessingStatus, Segment, Source, Speaker, Track, TrackRole,
    };
    use tokio::io::{
        AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
    };

    use super::*;

    /// How long a test waits for a barrier before failing (never a claim
    /// about ordering).
    const BARRIER: Duration = Duration::from_secs(60);

    #[test]
    fn documented_tools_match_the_runtime_routers() {
        for (prefix, router, read_only) in [
            ("The read tools are ", Server::read_tools(), true),
            ("The write tools are ", Server::write_tools(), false),
        ] {
            let mut registered: Vec<_> = router
                .list_all()
                .into_iter()
                .map(|tool| {
                    assert_eq!(
                        tool.annotations
                            .and_then(|annotations| annotations.read_only_hint),
                        Some(read_only),
                        "{}",
                        tool.name
                    );
                    tool.name.into_owned()
                })
                .collect();
            registered.sort_unstable();
            for (name, doc) in [
                ("mcp.rs", include_str!("mcp.rs")),
                ("design.md", include_str!("../../../docs/design.md")),
            ] {
                let listed = doc
                    .lines()
                    .find_map(|line| {
                        line.trim_start_matches("//! ")
                            .trim_start_matches("- ")
                            .strip_prefix(prefix)
                    })
                    .unwrap_or_else(|| panic!("Missing tool list in {name}"));
                let mut listed: Vec<_> = listed.split('`').skip(1).step_by(2).collect();
                listed.sort_unstable();
                assert_eq!(registered, listed, "{name}: {prefix}");
            }
        }
    }

    #[test]
    fn mcp_excluded_capabilities_are_absent_from_the_router() {
        let routes = Server::read_tools() + Server::write_tools();
        for excluded in [
            "delete_meeting",
            "discard_recording",
            "resolve_recording_journal",
            "apply_retention",
            "preview_retention",
            "run_retention",
            "set_retention",
            "merge_people",
            "resume_people_merge",
            "enable_provider",
            "set_provider_key",
            "delete_provider_key",
            "set_provider_secret",
            "uninstall",
            "uninstall_broscribe",
            "uninstall_agent_tools",
            "uninstall_daemon",
            "remove_daemon",
            "check_for_updates",
            "check_update",
            "install_update",
            "apply_update",
            "update_status",
            "set_library_folder",
            "switch_library",
            "set_consent_reminder",
            "disable_consent_reminder",
            "set_detection_policy",
            "accept_unverified_model_license",
            "move_recording",
            "check_voice_reading",
            "delete_template",
            "reset_template",
            "delete_enrollment_reading",
            "group_speakers",
            "ungroup_speakers",
            "apply_identity_reresolution",
            "set_mcp_enabled",
            "set_maintenance_mode",
            "sign_in_calendar",
            "remove_calendar_account",
            "set_calendar_client_secret",
            "run_queue_worker",
            "prune_queue",
        ] {
            assert!(routes.get(excluded).is_none(), "excluded tool {excluded}");
        }
        fn check_schema(schema: &Value, tool: &str) {
            match schema {
                Value::Object(fields) => {
                    if let Some(properties) = fields.get("properties").and_then(Value::as_object) {
                        for excluded in [
                            "allow_unverified_license",
                            "library_path",
                            "library_folder",
                            "library_root",
                        ] {
                            assert!(
                                !properties.contains_key(excluded),
                                "{tool} accepts {excluded}"
                            );
                        }
                    }
                    assert!(
                        fields.get("const") != Some(&json!("move"))
                            && !fields
                                .get("enum")
                                .and_then(Value::as_array)
                                .is_some_and(|values| values.iter().any(|value| value == "move")),
                        "{tool} accepts move handling"
                    );
                    for value in fields.values() {
                        check_schema(value, tool);
                    }
                }
                Value::Array(entries) => {
                    for entry in entries {
                        check_schema(entry, tool);
                    }
                }
                _ => {}
            }
        }

        for tool in routes.list_all() {
            let schema = json!(tool.input_schema);
            check_schema(&schema, &tool.name);
            if tool.annotations.and_then(|a| a.destructive_hint) == Some(true)
                && tool.name != "undo_live_speaker_name"
            {
                assert_eq!(
                    schema["properties"]["confirm"]["type"], "boolean",
                    "{}",
                    tool.name
                );
                assert_eq!(
                    schema["properties"]["confirm"]["default"], false,
                    "{}",
                    tool.name
                );
            }
            if tool.name == "enable_consent_reminder" {
                assert!(schema["properties"].get("enabled").is_none());
            }
        }
    }

    fn deps(root: &Path) -> Deps {
        use broscribe_platform::fakes::{
            FakeAutostart, FakeBrowserLauncher, FakeMicCapture, FakePaths, FakeProcessProbe,
            FakeProcessTreeControl, FakeSecretStore, FakeTrustStore,
        };
        Deps {
            paths: Box::new(FakePaths::new(root)),
            updates: crate::UpdateDeps::with_transport(
                Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                    root.join("install"),
                    "synthetic-test-target",
                )),
                Arc::new(broscribe_updates::FakeTransport::default()),
            ),
            models: Box::new(broscribe_models::fakes::FakeModelProvider::default()),
            decoder: broscribe_platform::DecoderChain::default(),
            process: Box::new(FakeProcessProbe::new(1)),
            mic: Box::new(FakeMicCapture::new(16_000)),
            secrets: Arc::new(FakeSecretStore::default()),
            browser: Arc::new(FakeBrowserLauncher::default()),
            agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
            autostart: Box::new(FakeAutostart::default()),
            agents: crate::AgentEnv::isolated(root),
            execution: ExecutionContext::new(
                Arc::new(FakeProcessTreeControl::default()),
                Arc::new(FakeTrustStore::default()),
            ),
            clock: crate::system_clock(),
        }
    }

    /// One `done` meeting with two speakers (centroids of the default
    /// embedding model, no recording), written through core and indexed.
    fn seed(lib_dir: &Path) -> MeetingId {
        let lib = Library::open(lib_dir).unwrap();
        let id = lib.new_meeting_id().unwrap();
        let started = chrono::DateTime::parse_from_rfc3339("2026-10-05T14:00:00-03:00").unwrap();
        let mut m = Meeting::new(id.clone(), Source::Import, started);
        m.title = Some("Pricing sync".into());
        m.duration_s = 20.0;
        m.tracks = vec![Track {
            id: "mixed".into(),
            role: TrackRole::Mixed,
        }];
        let model = Config::default().profile.speaker_embedding;
        for n in 1..=2u8 {
            let mut s = Speaker::new(SpeakerId::new(n.into()).unwrap(), "mixed");
            s.talk_s = 4.0;
            let vector: Vec<f32> = (0..16)
                .map(|i| if i == usize::from(n) { 1.0 } else { 0.0 })
                .collect();
            s.centroid = Some(Centroid::new(model.clone(), &vector, 3));
            m.speakers.push(s);
        }
        for (i, (spk, start, end, text)) in [
            (1u8, 0.0, 4.0, "We should ship the pricing memo"),
            (2, 5.0, 9.0, "Agreed the roadmap looks good"),
        ]
        .into_iter()
        .enumerate()
        {
            m.segments.push(Segment {
                speech_overlap: Vec::new(),
                id: u32::try_from(i).unwrap(),
                track: "mixed".into(),
                speaker: Some(SpeakerId::new(spk.into()).unwrap()),
                start,
                end,
                text: text.into(),
                words: Vec::new(),
            });
        }
        m.processing.status = ProcessingStatus::Done;
        lib.write(|tx| {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)?;
            Index::open(tx.root())?.upsert_meeting(tx, &m)
        })
        .unwrap();
        id
    }

    /// Every file of the library but its lock files and the rebuildable
    /// index: meetings, people, voiceprints and the queue.
    fn library_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        let mut dirs = vec![root.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.ends_with(broscribe_core::paths::INDEX_DIR)
                    || path.ends_with(broscribe_core::paths::LOCK_FILE)
                {
                    continue;
                }
                if path.is_dir() {
                    dirs.push(path);
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    files.insert(path, bytes);
                }
            }
        }
        files
    }

    /// A client speaking raw JSON-RPC (newline-delimited), so the test
    /// controls request IDs and cancellations exactly.
    struct RawClient {
        writer: WriteHalf<DuplexStream>,
        lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
        /// Every message the server sent, in order.
        seen: Vec<Value>,
    }

    impl RawClient {
        async fn connect(io: DuplexStream) -> Self {
            let (reader, writer) = tokio::io::split(io);
            let mut client = Self {
                writer,
                lines: BufReader::new(reader).lines(),
                seen: Vec::new(),
            };
            client
                .send(json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": { "name": "synthetic-client", "version": "0.0.0" },
                    },
                }))
                .await;
            assert!(client.answer(0).await.get("result").is_some());
            client
                .send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .await;
            client
        }

        async fn send(&mut self, message: Value) {
            self.writer
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
            self.writer.flush().await.unwrap();
        }

        async fn call(&mut self, id: i64, tool: &str, arguments: Value) {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": tool, "arguments": arguments },
            }))
            .await;
        }

        async fn cancel(&mut self, id: i64) {
            self.send(json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": id, "reason": "synthetic cancellation" },
            }))
            .await;
        }

        /// Reads messages until the answer to request `id`.
        async fn answer(&mut self, id: i64) -> Value {
            loop {
                let line = tokio::time::timeout(BARRIER, self.lines.next_line())
                    .await
                    .expect("the server answers in time")
                    .unwrap()
                    .expect("the server is still connected");
                let message: Value = serde_json::from_str(&line).unwrap();
                self.seen.push(message.clone());
                if message["id"] == id && message.get("method").is_none() {
                    return message;
                }
            }
        }

        /// The structured result of a successful call `id`.
        async fn ok(&mut self, id: i64) -> Value {
            let answer = self.answer(id).await;
            assert_ne!(answer["result"]["isError"], json!(true), "{answer}");
            answer["result"]["structuredContent"].clone()
        }
    }

    /// Runs `script` as a client of `server` on its own thread while this
    /// thread serves; returns once both ended.
    fn serve_script<F>(
        deps: &Deps,
        server: Server,
        requests: mpsc::Receiver<WorkerJob>,
        script: impl FnOnce(RawClient) -> F + Send + 'static,
    ) where
        F: Future<Output = ()>,
    {
        Config::update(&server.data_dir, |config| {
            config.mcp.enabled = Some(true);
            Ok(())
        })
        .unwrap();
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let client = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move { script(RawClient::connect(client_io).await).await });
        });
        let (reader, writer) = tokio::io::split(server_io);
        run(deps, server, requests, reader, writer).unwrap();
        client.join().unwrap();
    }

    #[test]
    fn people_metadata_edit_matches_cli_and_honours_revision() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("library")).unwrap();
        let person =
            broscribe_core::schema::Person::new(broscribe_core::PersonId::random(), "Nora Sample");
        lib.write(|tx| {
            tx.save_person(&person)?;
            Index::open(tx.root())?.upsert_person(tx, &person)
        })
        .unwrap();
        let deps = deps(dir.path());
        let (server, requests) = Server::new(&deps, lib.clone(), dir.path().join("data"));
        serve_script(&deps, server, requests, move |mut client| async move {
            client
                .call(1, "get_person", json!({"person": person.id}))
                .await;
            let original = client.ok(1).await;
            let args = json!({
                "person_id": person.id,
                "expected": original["revision"],
                "name": "Nora Fiction",
                "add_emails": ["nora@example.invalid"]
            });
            client.call(2, "edit_person", args.clone()).await;
            let edited = client.ok(2).await;
            assert!(edited["undo_id"].is_string());
            let saved = lib.load_person(&person.id).unwrap();
            assert_eq!(saved.name, "Nora Fiction");
            assert_eq!(saved.emails, ["nora@example.invalid"]);
            client.call(3, "edit_person", args).await;
            let error = client.answer(3).await;
            assert_eq!(
                error["result"]["structuredContent"]["error"]["code"],
                "stale_person"
            );
            client.call(4, "list_people", json!({})).await;
            assert_eq!(client.ok(4).await["people"][0]["name"], "Nora Fiction");
        });
    }

    #[test]
    fn speaker_review_tools_skip_and_restore_without_naming() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        let meeting = seed(&library);
        let deps = deps(dir.path());
        let lib = Library::open_existing(&library).unwrap();
        let original = lib.load_meeting(&meeting).unwrap();
        let (server, requests) = Server::new(&deps, lib.clone(), dir.path().join("data"));
        serve_script(&deps, server, requests, move |mut client| async move {
            let row = json!({ "meeting_id": meeting, "speaker_id": "S2" });
            client.call(1, "dismiss_speaker_review", row.clone()).await;
            let dismissed = client.ok(1).await;
            assert_eq!(dismissed["dismissed"], true);
            assert_eq!(dismissed["changed"], true);
            client
                .call(2, "list_speaker_reviews", json!({ "dismissed": true }))
                .await;
            let listed = client.ok(2).await;
            assert_eq!(listed["dismissed"], 1);
            assert_eq!(listed["speakers"][0]["speaker_id"], "S2");
            client.call(3, "restore_speaker_review", row).await;
            assert_eq!(client.ok(3).await["dismissed"], false);
            client.call(4, "list_speaker_reviews", json!({})).await;
            assert_eq!(client.ok(4).await["dismissed"], 0);
            let restored = lib.load_meeting(&meeting).unwrap();
            let id = speaker_id("S2").unwrap();
            assert_eq!(restored.speaker(id), original.speaker(id));
            assert_eq!(restored.segments, original.segments);
        });
    }

    #[test]
    fn speaker_rejection_and_voice_forget_keep_their_distinct_effects() {
        use broscribe_core::schema::{Assignment, Suggestion};

        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        let data = dir.path().join("data");
        let meeting = seed(&library);
        let deps = deps(dir.path());
        let lib = Library::open_existing(&library).unwrap();
        let (server, requests) = Server::new(&deps, lib.clone(), data);
        serve_script(&deps, server, requests, move |mut client| async move {
            client
                .call(
                    1,
                    "name_speaker",
                    json!({
                        "meeting_id": meeting, "speaker_id": "S1", "person": "Mira Fiction"
                    }),
                )
                .await;
            let named = client.ok(1).await;
            let id = broscribe_core::PersonId::parse(named["person_id"].as_str().unwrap()).unwrap();
            let s1 = speaker_id("S1").unwrap();
            let s2 = speaker_id("S2").unwrap();
            lib.write(|tx| {
                let mut saved = tx.library().load_meeting(&meeting)?;
                let speaker = saved.speaker_mut(s2).unwrap();
                speaker.assignment = Assignment::Suggested;
                speaker.suggestion = Some(Suggestion {
                    person_id: id.clone(),
                    score: 0.78,
                });
                tx.save_meeting(&saved)
            })
            .unwrap();

            client
                .call(
                    2,
                    "reject_speaker",
                    json!({
                        "meeting_id": meeting, "speaker_id": "S2", "person": id
                    }),
                )
                .await;
            let rejected = client.ok(2).await;
            assert_eq!(rejected["rejected"], json!(id), "{rejected}");
            let saved = lib.load_meeting(&meeting).unwrap();
            assert_eq!(saved.speaker(s2).unwrap().assignment, Assignment::None);
            assert_eq!(
                saved.speaker(s2).unwrap().rejected_person_ids,
                std::slice::from_ref(&id)
            );
            assert_eq!(saved.speaker(s1).unwrap().person_id.as_ref(), Some(&id));

            let enrollment = lib.person_dir(&id).join("enrollment").join("synthetic.wav");
            std::fs::create_dir_all(enrollment.parent().unwrap()).unwrap();
            std::fs::write(&enrollment, b"synthetic").unwrap();
            assert!(!lib.load_voiceprints(&id).unwrap().embeddings.is_empty());
            client
                .call(3, "forget_voice", json!({ "person": id, "confirm": true }))
                .await;
            let forgotten = client.ok(3).await;
            assert_eq!(forgotten["person_id"], json!(id));
            assert!(forgotten.get("photos").is_none());
            assert!(!enrollment.exists());
            assert!(lib.load_voiceprints(&id).unwrap().embeddings.is_empty());
            assert_eq!(lib.load_person(&id).unwrap().name, "Mira Fiction");
            let after = lib.load_meeting(&meeting).unwrap();
            assert_eq!(after.speaker(s1).unwrap().assignment, Assignment::Confirmed);
            assert_eq!(after.speaker(s1).unwrap().person_id.as_ref(), Some(&id));
            assert_eq!(after.segments, saved.segments);
        });
    }

    /// A write cancelled after it passed its first cancellation check, while
    /// another writer holds the library, changes nothing once the lock is
    /// free. The server's own barrier proves the forget passed that check
    /// before the client cancels; a later request on the same connection is
    /// answered (a connection's messages are handled in order) before the
    /// other writer lets go. The forget then ends `cancelled`: its lock wait
    /// is far longer than the test, so this is the check under the lock,
    /// never `library_busy`. The person, their voiceprints, the meeting and
    /// the queue are unchanged, and the cancelled request is never answered.
    #[test]
    fn a_write_cancelled_while_it_waits_for_the_library_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let lib_dir = tmp.path().join("library");
        let data_dir = tmp.path().join("data");
        let id = seed(&lib_dir);
        let deps = deps(tmp.path());

        // A person with a voiceprint and a queued recheck.
        let lib = Library::open_existing(&lib_dir).unwrap();
        let (server, requests) = Server::new(&deps, lib, data_dir.clone());
        let meeting = id.clone();
        serve_script(&deps, server, requests, move |mut c| async move {
            c.call(
                1,
                "name_speaker",
                json!({ "meeting_id": meeting.as_str(), "speaker_id": "S1", "person": "Ana Example" }),
            )
            .await;
            let named = c.ok(1).await;
            assert_eq!(named["voiceprint_added"], true, "{named}");
            assert!(named["learning"]["job_id"].is_string(), "{named}");
        });
        let before = library_files(&lib_dir);

        let (held_tx, held) = std_mpsc::channel();
        let (release, release_rx) = std_mpsc::channel::<()>();
        let holder_lib = lib_dir.clone();
        let holder = std::thread::spawn(move || {
            Library::open_existing(&holder_lib)
                .unwrap()
                .write(|_| {
                    held_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .unwrap();
        });
        held.recv_timeout(BARRIER)
            .expect("the other writer holds the library");

        let waiting = Library::open_existing(&lib_dir)
            .unwrap()
            .with_lock_wait(Duration::from_secs(3600));
        let (mut server, requests) = Server::new(&deps, waiting, data_dir);
        let (admitted_tx, admitted) = std_mpsc::channel();
        let (ended_tx, ended) = std_mpsc::channel();
        server.hooks = Hooks {
            admitted: Some(Arc::new(move || admitted_tx.send(()).unwrap())),
            ended: Some(Arc::new(move |code| ended_tx.send(code).unwrap())),
            left: None,
        };
        let meeting = id.clone();
        serve_script(&deps, server, requests, move |mut c| async move {
            c.call(
                1,
                "forget_person",
                json!({ "person": "Ana Example", "confirm": true }),
            )
            .await;
            admitted
                .recv_timeout(BARRIER)
                .expect("the forget passed its first cancellation check");
            c.cancel(1).await;
            let get = json!({ "meeting_id": meeting.as_str() });
            c.call(2, "get_meeting", get.clone()).await;
            c.ok(2).await;
            release.send(()).unwrap();
            assert_eq!(
                ended.recv_timeout(BARRIER).expect("the forget ended"),
                Some(CANCELLED),
                "refused under the lock"
            );
            c.call(3, "get_meeting", get).await;
            c.ok(3).await;
            assert!(
                !c.seen
                    .iter()
                    .any(|m| m["id"] == 1 && m.get("method").is_none()),
                "a cancelled request is never answered: {:?}",
                c.seen
            );
        });
        holder.join().unwrap();

        assert!(
            library_files(&lib_dir) == before,
            "the cancelled forget changed the library"
        );
    }

    /// A forget still running when its connection ends purges every cached
    /// photo of the person. The client goes away once the forget's blocking
    /// work ended, and that work returns only after the server's teardown
    /// (EOF, then the drain rmcp bounds) dropped the handler waiting for it,
    /// so nothing that request's async side would run afterwards can run.
    /// The person is gone and so is every cached image version: each
    /// integration's provider and account stores, flat and per generation.
    #[test]
    fn a_forget_whose_connection_ends_still_purges_every_cached_photo() {
        let tmp = tempfile::tempdir().unwrap();
        let lib_dir = tmp.path().join("library");
        let data_dir = tmp.path().join("data");
        let id = seed(&lib_dir);
        let deps = deps(tmp.path());

        let lib = Library::open_existing(&lib_dir).unwrap();
        let (server, requests) = Server::new(&deps, lib, data_dir.clone());
        let (person_tx, person) = std_mpsc::channel();
        serve_script(&deps, server, requests, move |mut c| async move {
            c.call(
                1,
                "name_speaker",
                json!({ "meeting_id": id.as_str(), "speaker_id": "S1", "person": "Ana Example" }),
            )
            .await;
            let named = c.ok(1).await;
            let person = named["person_id"].as_str().unwrap().to_owned();
            person_tx.send(person).unwrap();
        });
        let person = person.recv().unwrap();
        let name = format!("{person}.img");
        let mut photos = Vec::new();
        for integration in broscribe_integrations::IntegrationId::ALL {
            let dir = broscribe_integrations::account_dir(&data_dir, integration);
            let account = dir.join("accounts").join("a-synthetic");
            for store in [dir.join("photos"), account.join("photos")] {
                for photo in [store.join(&name), store.join("7").join(&name)] {
                    std::fs::create_dir_all(photo.parent().unwrap()).unwrap();
                    std::fs::write(&photo, b"synthetic-photo").unwrap();
                    photos.push(photo);
                }
            }
        }

        let lib = Library::open_existing(&lib_dir).unwrap();
        let (mut server, requests) = Server::new(&deps, lib, data_dir);
        let (ended_tx, ended) = std_mpsc::channel();
        let (left_tx, left) = std_mpsc::channel();
        let left = Mutex::new(left);
        server.hooks = Hooks {
            admitted: None,
            ended: Some(Arc::new(move |code| {
                ended_tx.send(code).unwrap();
                left.lock()
                    .unwrap()
                    .recv_timeout(BARRIER)
                    .expect("the server's teardown dropped the forget's handler");
            })),
            left: Some(Arc::new(move || {
                let _ = left_tx.send(());
            })),
        };
        serve_script(&deps, server, requests, move |mut c| async move {
            c.call(
                1,
                "forget_person",
                json!({ "person": "Ana Example", "confirm": true }),
            )
            .await;
            assert_eq!(
                ended.recv_timeout(BARRIER).expect("the forget ended"),
                None,
                "the forget committed"
            );
            // Gone with the answer outstanding: the server reads EOF.
            drop(c);
        });

        let lib = Library::open_existing(&lib_dir).unwrap();
        assert!(lib.list_people().unwrap().is_empty());
        assert_eq!(
            ops::find_person(&lib, &person).unwrap_err().code(),
            "not_found"
        );
        let remaining: Vec<_> = photos.iter().filter(|p| p.exists()).collect();
        assert!(remaining.is_empty(), "cached photos remain: {remaining:?}");
    }
}
