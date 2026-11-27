//! Paged library search (design §4.1): the meetings the library lists, read
//! a page at a time through the index's keyset pages, never as one
//! unbounded read.
//!
//! - Without text the list is [`Index::search_meetings_page`] in its order
//!   (newest first), filters included.
//! - With text, meetings whose title, tags or participants match come first
//!   (their own pages, in rank order), then meetings found only through their
//!   transcript, from `broscribe search --query-stdin --page-size N
//!   [--cursor C]` pages (keyword or hybrid, as the CLI chooses; text and
//!   textual filters go on stdin only). A meeting is listed once; one whose transcript
//!   matched shows its best utterance snippet. Transcript pages hold
//!   utterances, so one load reads at most [`TRANSCRIPT_PAGES_PER_LOAD`] of
//!   them.
//! - Each read can be cancelled: a newer search, a refresh or quitting sets
//!   its flag, which stops it between pages and kills its CLI child; its
//!   result is never applied.
//! - A cursor the index reports stale (it changed since the page before),
//!   mismatched or invalid restarts the read from the first page, reading
//!   at least as many meetings as were listed; the list is then replaced
//!   rather than extended, so nothing is listed twice or skipped.
//! - Without an index (a new library) the list is empty. Any other index
//!   error (one that needs a rebuild included) is shown; there is no
//!   fallback scan of the meeting folders.
//! - A failed transcript page (the `broscribe search` child, not the index)
//!   doesn't fail the read. A first read or a refresh then lists only the
//!   title, people and tag matches, beside a [`TranscriptError`]; a
//!   "Load more" keeps what it read before the failed page and makes that
//!   page the next one, so "Load more" reads it again.
//! - Listed meetings always belong to the query being read: a read that
//!   fails while the list still shows an earlier query's meetings clears
//!   them (LIB-61).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use broscribe_core::index::{
    CURSOR_MISMATCH, CURSOR_STALE, CursorRequest, INVALID_CURSOR, Index, IndexDatabase, MeetingHit,
    PageCursor, SearchMode, SearchQuery,
};
use broscribe_core::{Error, MeetingId};

use crate::jobs::{ChildKind, CliError, CliRunner, core_user_text};

/// Meetings per index page (and per "Load more").
pub const MEETING_PAGE_SIZE: usize = 100;
/// Utterances per transcript search page.
pub const TRANSCRIPT_PAGE_SIZE: usize = 200;
/// Most transcript pages one load reads; more stay behind "Load more".
pub const TRANSCRIPT_PAGES_PER_LOAD: usize = 5;
/// Restarts from the first page after stale cursors before giving up.
const STALE_RESTARTS: usize = 2;

/// Page sizes of a [`MeetingSearch`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageSizes {
    /// Meetings per index page and the meetings one "Load more" adds.
    pub meetings: usize,
    /// Utterances per transcript search page.
    pub transcript: usize,
}

impl Default for PageSizes {
    fn default() -> Self {
        Self {
            meetings: MEETING_PAGE_SIZE,
            transcript: TRANSCRIPT_PAGE_SIZE,
        }
    }
}

/// What a read in flight does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Load {
    /// The first pages of a new query.
    Search,
    /// The listed pages again, after the library changed.
    Refresh,
    /// The next page after the listed ones.
    More,
}

/// Where the next page starts.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Next {
    /// Meeting (title, tags, participants) matches: the first page, or the
    /// one after this index cursor.
    Meetings(Option<PageCursor>),
    /// Transcript matches (text only): the first page, or the one after
    /// this CLI cursor (opaque here; it holds no query text).
    Transcript(Option<String>),
}

/// A transcript search failure beside the listed meetings (LIB-61): the
/// index pages were read, a `broscribe search` page was not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptError {
    /// The CLI's error code, for tests and logs; never shown.
    pub code: String,
    /// The cause shown to the user: [`CliError::user_text`], words without
    /// the code; empty when nothing is known.
    pub message: String,
    /// It failed during "Load more": the meetings read before the failed
    /// page stay listed and "Load more" reads that page again. Otherwise
    /// only title, people and tag matches are listed and the search is run
    /// again.
    pub retry_more: bool,
}

/// The meeting search the library view shows; held by
/// [`crate::state::AppState`] (one search state, no parallel copy), so it
/// survives the main window.
#[derive(Debug, Default)]
pub struct MeetingSearch {
    /// Bumped by every read started; only that read's result applies.
    generation: u64,
    /// Bumped whenever `results` changes.
    revision: u64,
    query: SearchQuery,
    /// Bumped by every [`Self::start`]: names the query being read.
    query_id: u64,
    /// The `query_id` the listed meetings were read for; `None` while none
    /// are (nothing applied yet, or the list was cleared).
    shown_query: Option<u64>,
    results: Vec<MeetingHit>,
    loading: Option<Load>,
    error: Option<SearchError>,
    page_failed: bool,
    index_missing: bool,
    metadata_count: usize,
    transcript_matches: BTreeMap<MeetingId, TranscriptMatch>,
    recording_sessions: BTreeMap<MeetingId, String>,
    /// Why the last read listed no transcript matches (or not all of
    /// them); it describes the listed meetings and goes with them.
    transcript_error: Option<TranscriptError>,
    /// `None` once everything matching is listed.
    next: Option<Next>,
    /// Meetings listed.
    listed: BTreeSet<MeetingId>,
    /// Listed meetings that show a transcript snippet.
    matched: BTreeSet<MeetingId>,
    /// The flag of the read in flight.
    cancel: Option<Arc<AtomicBool>>,
    sizes: PageSizes,
}

impl MeetingSearch {
    pub fn with_page_sizes(sizes: PageSizes) -> Self {
        Self {
            sizes,
            ..Self::default()
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Moves whenever [`Self::results`] changes.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn query(&self) -> &SearchQuery {
        &self.query
    }

    /// The listed meetings, in list order.
    pub fn results(&self) -> &[MeetingHit] {
        &self.results
    }

    /// The read in flight, if any.
    pub fn loading(&self) -> Option<Load> {
        self.loading
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_ref().map(SearchError::user_text)
    }

    pub fn failure(&self) -> Option<&SearchError> {
        self.error.as_ref()
    }

    pub fn page_failed(&self) -> bool {
        self.page_failed
    }

    pub fn index_missing(&self) -> bool {
        self.index_missing
    }

    pub fn metadata_count(&self) -> usize {
        self.metadata_count
    }

    pub fn transcript_match(&self, id: &MeetingId) -> Option<&TranscriptMatch> {
        self.transcript_matches.get(id)
    }

    pub fn recording_session(&self, id: &MeetingId) -> Option<&str> {
        self.recording_sessions.get(id).map(String::as_str)
    }

    pub fn searching_transcripts(&self) -> bool {
        matches!(self.next, Some(Next::Transcript(_))) && self.transcript_error.is_none()
    }

    /// Why the transcript matches of the listed meetings are missing or
    /// incomplete, if they are. The index's matches stay listed.
    pub fn transcript_error(&self) -> Option<&TranscriptError> {
        self.transcript_error.as_ref()
    }

    /// Whether more meetings match than are listed.
    pub fn has_more(&self) -> bool {
        self.next.is_some()
    }

    /// Whether the listed meetings were read for the current query (an
    /// earlier query's stay listed while a new one is first read).
    fn lists_current_query(&self) -> bool {
        self.shown_query == Some(self.query_id)
    }

    /// A read of the first page of `query`, superseding any read in flight.
    /// The listed meetings stay until its result applies; the transcript
    /// failure beside them goes at once, since it was about another query.
    pub fn start(&mut self, query: SearchQuery) -> PageJob {
        self.query = query;
        self.query_id += 1;
        self.transcript_error = None;
        self.error = None;
        self.page_failed = false;
        self.index_missing = false;
        let want = self.sizes.meetings;
        self.job(
            Load::Search,
            Next::Meetings(None),
            BTreeSet::new(),
            BTreeSet::new(),
            want,
            true,
        )
    }

    /// A read of the current query from the first page again, at least as
    /// far as it is listed now, superseding any read in flight. While the
    /// listed meetings belong to an earlier query (a superseded first read
    /// of this one), it is a first read ([`Load::Search`]) itself.
    pub fn refresh(&mut self) -> PageJob {
        let (load, want) = if self.lists_current_query() {
            (Load::Refresh, self.results.len().max(self.sizes.meetings))
        } else {
            (Load::Search, self.sizes.meetings)
        };
        self.job(
            load,
            Next::Meetings(None),
            BTreeSet::new(),
            BTreeSet::new(),
            want,
            true,
        )
    }

    /// A read of the next page; `None` while another read is in flight,
    /// when everything is listed, or when the listed meetings belong to an
    /// earlier query.
    pub fn more(&mut self) -> Option<PageJob> {
        if self.loading.is_some() || !self.lists_current_query() {
            return None;
        }
        let from = self.next.clone()?;
        let (listed, matched) = (self.listed.clone(), self.matched.clone());
        let want = self.sizes.meetings;
        Some(self.job(Load::More, from, listed, matched, want, false))
    }

    fn job(
        &mut self,
        load: Load,
        from: Next,
        listed: BTreeSet<MeetingId>,
        matched: BTreeSet<MeetingId>,
        want: usize,
        replace: bool,
    ) -> PageJob {
        self.cancel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.loading = Some(load);
        PageJob {
            generation: self.generation,
            load,
            query: SearchQuery {
                limit: None,
                ..self.query.clone()
            },
            from,
            listed,
            matched,
            want,
            replace,
            sizes: self.sizes,
            cancel,
        }
    }

    /// Stops the read in flight (killing its CLI child); its result is
    /// never applied.
    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::SeqCst);
        }
        self.generation += 1;
        self.loading = None;
    }

    /// Applies the outcome of the read `generation`; `false` (changing
    /// nothing) when a newer read was started since.
    ///
    /// - A read that fails while the listed meetings belong to an earlier
    ///   query (a first read of a new query, or a refresh that superseded
    ///   one) clears them, so no meeting from another query stays listed.
    /// - A failed refresh of the listed query keeps its meetings with the
    ///   error and offers no next page.
    /// - A failed `Load::More` keeps its listed meetings and `has_more` so
    ///   the next page can be tried again.
    /// - A read whose transcript step failed still applies, with
    ///   [`Self::transcript_error`] set apart from [`Self::error`].
    pub fn apply(&mut self, generation: u64, outcome: PageOutcome) -> bool {
        if generation != self.generation {
            return false;
        }
        self.cancel = None;
        let load = self.loading.take();
        match outcome {
            Ok(batch) => {
                let before = (load == Some(Load::Refresh) && batch.replace).then(|| {
                    (
                        std::mem::take(&mut self.results),
                        self.metadata_count,
                        std::mem::take(&mut self.transcript_matches),
                        std::mem::take(&mut self.recording_sessions),
                        self.index_missing,
                    )
                });
                if batch.replace {
                    self.results = batch.hits;
                    self.metadata_count = batch.metadata;
                    self.transcript_matches.clear();
                    self.recording_sessions.clear();
                } else {
                    self.results.extend(batch.hits);
                    self.metadata_count += batch.metadata;
                }
                self.recording_sessions.extend(batch.recording_sessions);
                for (id, matched) in batch.snippets {
                    if let Some(hit) = self.results.iter_mut().find(|h| h.meeting_id == id) {
                        hit.snippet = Some(matched.snippet.clone());
                    }
                    self.transcript_matches.insert(id, matched);
                }
                self.listed = batch.listed;
                self.matched = batch.matched;
                self.next = batch.next;
                self.error = None;
                self.page_failed = false;
                self.index_missing = batch.index_missing;
                self.transcript_error = batch.transcript_error;
                self.shown_query = Some(self.query_id);
                if !before.is_some_and(|(hits, metadata, transcripts, sessions, missing)| {
                    hits == self.results
                        && metadata == self.metadata_count
                        && transcripts == self.transcript_matches
                        && sessions == self.recording_sessions
                        && missing == self.index_missing
                }) {
                    self.revision += 1;
                }
            }
            Err(ReadError::Cancelled) => {}
            Err(ReadError::Failed(error)) => {
                if !self.lists_current_query() {
                    self.clear_results();
                } else if load != Some(Load::More) {
                    self.next = None;
                }
                self.error = Some(error);
                self.page_failed = load == Some(Load::More);
            }
        }
        true
    }

    /// Empties the list and everything read with it, so no query's
    /// meetings are listed.
    fn clear_results(&mut self) {
        if !self.results.is_empty() {
            self.revision += 1;
        }
        self.results.clear();
        self.metadata_count = 0;
        self.transcript_matches.clear();
        self.recording_sessions.clear();
        self.index_missing = false;
        self.listed.clear();
        self.matched.clear();
        self.next = None;
        self.transcript_error = None;
        self.shown_query = None;
    }
}

/// Why a read produced no page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// Superseded or stopped.
    Cancelled,
    /// Shown to the user.
    Failed(SearchError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchError {
    pub code: &'static str,
    pub message: String,
}

impl SearchError {
    fn index(e: &Error) -> Self {
        Self {
            code: e.code(),
            message: core_user_text(e),
        }
    }

    pub fn user_text(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptMatch {
    pub snippet: String,
    pub speaker: String,
    pub start: f64,
    pub segment: Option<u32>,
}

/// The result of [`PageJob::run`].
pub type PageOutcome = Result<PageBatch, ReadError>;

/// Meetings read by one [`PageJob`].
#[derive(Debug)]
pub struct PageBatch {
    /// The meetings replace the listed ones (a first read, a refresh, or a
    /// restart after a stale cursor) instead of following them.
    replace: bool,
    hits: Vec<MeetingHit>,
    /// Transcript snippets for meetings listed from their metadata.
    snippets: Vec<(MeetingId, TranscriptMatch)>,
    metadata: usize,
    index_missing: bool,
    recording_sessions: BTreeMap<MeetingId, String>,
    next: Option<Next>,
    listed: BTreeSet<MeetingId>,
    matched: BTreeSet<MeetingId>,
    /// Set when a transcript page failed; the read still counts.
    transcript_error: Option<TranscriptError>,
}

impl PageBatch {
    fn empty() -> Self {
        Self {
            replace: true,
            hits: Vec::new(),
            snippets: Vec::new(),
            metadata: 0,
            index_missing: false,
            recording_sessions: BTreeMap::new(),
            next: None,
            listed: BTreeSet::new(),
            matched: BTreeSet::new(),
            transcript_error: None,
        }
    }

    pub fn replaces(&self) -> bool {
        self.replace
    }

    pub fn transcript_error(&self) -> Option<&TranscriptError> {
        self.transcript_error.as_ref()
    }
}

/// One read, made by [`MeetingSearch`]; [`PageJob::run`] blocks, so run it
/// on the background executor.
#[derive(Debug)]
pub struct PageJob {
    generation: u64,
    load: Load,
    query: SearchQuery,
    from: Next,
    listed: BTreeSet<MeetingId>,
    matched: BTreeSet<MeetingId>,
    /// New meetings to read before stopping (unless everything is read).
    want: usize,
    replace: bool,
    sizes: PageSizes,
    cancel: Arc<AtomicBool>,
}

impl PageJob {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether a newer read (or [`MeetingSearch::cancel`]) superseded it.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Reads the pages of the library at `root`; transcript pages through
    /// `cli`.
    pub fn run(self, root: &Path, cli: &CliRunner) -> PageOutcome {
        let index = match Index::open_readonly(root) {
            Ok(index) => index,
            Err(Error::NotFound(_)) if !Index::path(root).exists() => {
                let health =
                    Index::health(root).map_err(|e| ReadError::Failed(SearchError::index(&e)))?;
                return Ok(PageBatch {
                    index_missing: health.database == IndexDatabase::Missing,
                    ..PageBatch::empty()
                });
            }
            Err(e) => {
                tracing::warn!(code = e.code(), "opening the library index");
                let health = Index::health(root).ok();
                let mut error = SearchError::index(&e);
                if health.is_some_and(|health| health.database.needs_rebuild()) {
                    error.code = "index_repair";
                }
                return Err(ReadError::Failed(error));
            }
        };
        let Self {
            load,
            query,
            mut from,
            mut listed,
            mut matched,
            mut want,
            mut replace,
            sizes,
            cancel,
            ..
        } = self;
        let known = listed.len();
        let reader = Reader {
            index: &index,
            cli,
            filters: SearchQuery {
                text: None,
                mode: SearchMode::default(),
                query_vector: None,
                ..query.clone()
            },
            query: &query,
            sizes,
            cancel: &cancel,
        };
        for _ in 0..=STALE_RESTARTS {
            match reader.collect(from.clone(), &mut listed, &mut matched, want) {
                Ok(mut collected) => {
                    let retry_more = load == Load::More;
                    if collected.transcript_error.is_some() && !retry_more {
                        collected.metadata_only(&mut listed, &mut matched);
                    }
                    let recording_sessions = recording_sessions(root, &collected.hits);
                    return Ok(PageBatch {
                        replace,
                        hits: collected.hits,
                        snippets: collected.snippets,
                        metadata: collected.metadata,
                        index_missing: false,
                        recording_sessions,
                        next: collected.next,
                        listed,
                        matched,
                        transcript_error: collected.transcript_error.map(|(code, message)| {
                            TranscriptError {
                                code,
                                message,
                                retry_more,
                            }
                        }),
                    });
                }
                Err(Step::Stale) => {
                    // From the first page again, as far as was listed plus
                    // what was asked for, replacing the list.
                    if !replace {
                        want += known;
                        replace = true;
                    }
                    from = Next::Meetings(None);
                    listed.clear();
                    matched.clear();
                }
                Err(Step::Cancelled) => return Err(ReadError::Cancelled),
                Err(Step::Failed(message)) => return Err(ReadError::Failed(message)),
            }
        }
        Err(ReadError::Failed(SearchError {
            code: "library_changed",
            message: "the library kept changing while it was read; it is read again after the next change".into(),
        }))
    }
}

fn recording_sessions(root: &Path, hits: &[MeetingHit]) -> BTreeMap<MeetingId, String> {
    use broscribe_core::schema::{ProcessingStatus, Source};
    let drafts = hits.iter().filter(|hit| {
        hit.source == Source::Recording && hit.processing_status == ProcessingStatus::Provisional
    });
    let mut drafts = drafts.peekable();
    if drafts.peek().is_none() {
        return BTreeMap::new();
    }
    let Ok(library) = broscribe_core::Library::open_existing(root) else {
        return BTreeMap::new();
    };
    let Ok(lookup) = library.meeting_lookup() else {
        return BTreeMap::new();
    };
    drafts
        .filter_map(|hit| {
            let meeting = lookup.load(&hit.meeting_id).ok()?;
            let session = meeting.source_id.as_deref()?.strip_prefix("recording:")?;
            Some((hit.meeting_id.clone(), session.to_owned()))
        })
        .collect()
}

/// Why reading pages stopped early.
enum Step {
    /// A cursor no longer fits the index: read again from the first page.
    Stale,
    Cancelled,
    Failed(SearchError),
}

/// Outcome of a single transcript search page.
enum TranscriptStep {
    /// A cursor no longer fits the index: read again from the first page.
    Stale,
    Cancelled,
    /// Fatal index error while looking up a meeting hit.
    Fatal(SearchError),
    /// Non-fatal CLI transcript search failure: return metadata hits read
    /// so far along with `(code, cause)`, the cause in words without the
    /// code ([`CliError::user_text`]).
    TranscriptFailed(String, String),
}

fn is_cursor_code(code: &str) -> bool {
    [CURSOR_STALE, CURSOR_MISMATCH, INVALID_CURSOR].contains(&code)
}

impl Step {
    fn from_index(e: Error) -> Self {
        if is_cursor_code(e.code()) {
            Step::Stale
        } else {
            tracing::warn!(code = e.code(), "reading the library index");
            Step::Failed(SearchError::index(&e))
        }
    }
}

impl TranscriptStep {
    fn from_index(e: Error) -> Self {
        if is_cursor_code(e.code()) {
            Self::Stale
        } else {
            tracing::warn!(
                code = e.code(),
                "reading a transcript match from the library index"
            );
            Self::Fatal(SearchError::index(&e))
        }
    }

    fn from_cli(e: CliError) -> Self {
        match e.code.as_str() {
            "cancelled" => Self::Cancelled,
            code if is_cursor_code(code) => Self::Stale,
            _ => {
                let cause = e.user_text();
                Self::TranscriptFailed(e.code, cause)
            }
        }
    }
}

struct Collected {
    hits: Vec<MeetingHit>,
    snippets: Vec<(MeetingId, TranscriptMatch)>,
    next: Option<Next>,
    /// `(code, cause)` of a failed transcript page; `next` is that page.
    transcript_error: Option<(String, String)>,
    /// `hits[..metadata]` came from meeting (title, tags, participants)
    /// pages, which are read before any transcript page.
    metadata: usize,
}

impl Collected {
    /// What a first read or a refresh lists when a transcript page failed:
    /// the meeting pages' matches only, so nothing found through a
    /// transcript page read before the failure is listed beside a line
    /// saying titles, people and tags were searched; no next page.
    fn metadata_only(
        &mut self,
        listed: &mut BTreeSet<MeetingId>,
        matched: &mut BTreeSet<MeetingId>,
    ) {
        for hit in self.hits.drain(self.metadata..) {
            listed.remove(&hit.meeting_id);
        }
        self.snippets.clear();
        matched.clear();
        self.next = None;
    }
}

struct Reader<'a> {
    index: &'a Index,
    cli: &'a CliRunner,
    query: &'a SearchQuery,
    /// The query without text: a transcript match's meeting must pass it.
    filters: SearchQuery,
    sizes: PageSizes,
    cancel: &'a Arc<AtomicBool>,
}

impl Reader<'_> {
    /// Pages from `from` until `want` new meetings are read, everything is
    /// read, or [`TRANSCRIPT_PAGES_PER_LOAD`] transcript pages were read.
    /// A transcript page the CLI fails to read (other than for a cursor or
    /// a cancel) ends the read there: what was read before it is returned
    /// with `transcript_error`, and the failed page as `next`.
    fn collect(
        &self,
        from: Next,
        listed: &mut BTreeSet<MeetingId>,
        matched: &mut BTreeSet<MeetingId>,
        want: usize,
    ) -> Result<Collected, Step> {
        let mut hits = Vec::new();
        let mut snippets = Vec::new();
        // Transcript matches whose meeting the filters exclude.
        let mut excluded = BTreeSet::new();
        let mut transcript_pages = 0;
        let mut metadata = 0;
        let mut next = Some(from);
        while let Some(step) = next.take() {
            let transcript_spent = matches!(step, Next::Transcript(_))
                && transcript_pages == TRANSCRIPT_PAGES_PER_LOAD;
            if hits.len() >= want || transcript_spent {
                next = Some(step);
                break;
            }
            if self.cancel.load(Ordering::SeqCst) {
                return Err(Step::Cancelled);
            }
            next = match step {
                Next::Meetings(after) => {
                    let next = self.meetings_page(after, listed, &mut hits)?;
                    metadata = hits.len();
                    next
                }
                Next::Transcript(after) => {
                    transcript_pages += 1;
                    match self.transcript_page(
                        after.as_deref(),
                        listed,
                        matched,
                        &mut excluded,
                        &mut hits,
                        &mut snippets,
                    ) {
                        Ok(next_step) => next_step,
                        Err(TranscriptStep::Stale) => return Err(Step::Stale),
                        Err(TranscriptStep::Cancelled) => return Err(Step::Cancelled),
                        Err(TranscriptStep::Fatal(message)) => return Err(Step::Failed(message)),
                        Err(TranscriptStep::TranscriptFailed(code, message)) => {
                            // A failed page adds nothing, so it can be read
                            // again from the same cursor.
                            return Ok(Collected {
                                hits,
                                snippets,
                                next: Some(Next::Transcript(after)),
                                transcript_error: Some((code, message)),
                                metadata,
                            });
                        }
                    }
                }
            };
        }
        Ok(Collected {
            hits,
            snippets,
            next,
            transcript_error: None,
            metadata,
        })
    }

    fn meetings_page(
        &self,
        after: Option<PageCursor>,
        listed: &mut BTreeSet<MeetingId>,
        hits: &mut Vec<MeetingHit>,
    ) -> Result<Option<Next>, Step> {
        let request = CursorRequest {
            page_size: self.sizes.meetings,
            after,
            anchor: None,
        };
        let page = self
            .index
            .search_meetings_page(self.query, &request)
            .map_err(Step::from_index)?;
        for hit in page.items {
            if listed.insert(hit.meeting_id.clone()) {
                hits.push(hit);
            }
        }
        Ok(match page.next_cursor {
            Some(cursor) => Some(Next::Meetings(Some(cursor))),
            None if has_text(self.query) => Some(Next::Transcript(None)),
            None => None,
        })
    }

    fn transcript_page(
        &self,
        after: Option<&str>,
        listed: &mut BTreeSet<MeetingId>,
        matched: &mut BTreeSet<MeetingId>,
        excluded: &mut BTreeSet<MeetingId>,
        hits: &mut Vec<MeetingHit>,
        snippets: &mut Vec<(MeetingId, TranscriptMatch)>,
    ) -> Result<Option<Next>, TranscriptStep> {
        let (args, stdin) = transcript_command(self.query, self.sizes.transcript, after);
        let value = self
            .cli
            .run_blocking_cancellable_stdin(&args, ChildKind::Command, stdin, self.cancel.clone())
            .map_err(TranscriptStep::from_cli)?;
        let utterances = value["results"].as_array().ok_or_else(|| {
            TranscriptStep::TranscriptFailed(
                "bad_output".into(),
                "The search tool's answer couldn't be read.".into(),
            )
        })?;
        // Utterances come in rank order: a meeting's first is its best.
        for utterance in utterances {
            let Some(id) = utterance["meeting_id"]
                .as_str()
                .and_then(|id| MeetingId::parse(id).ok())
            else {
                continue;
            };
            if matched.contains(&id) || excluded.contains(&id) {
                continue;
            }
            let snippet = utterance["snippet"].as_str().map(|text| TranscriptMatch {
                snippet: text.to_owned(),
                speaker: utterance["speaker_name"]
                    .as_str()
                    .unwrap_or("Unknown")
                    .to_owned(),
                start: utterance["start"].as_f64().unwrap_or_default(),
                segment: utterance["segment_id"]
                    .as_u64()
                    .and_then(|id| u32::try_from(id).ok()),
            });
            if listed.contains(&id) {
                if let Some(snippet) = snippet {
                    snippets.push((id.clone(), snippet));
                }
                matched.insert(id);
                continue;
            }
            match self.meeting(&id)? {
                Some(hit) => {
                    if let Some(snippet) = snippet {
                        snippets.push((id.clone(), snippet));
                    }
                    listed.insert(id.clone());
                    matched.insert(id);
                    hits.push(hit);
                }
                None => {
                    excluded.insert(id);
                }
            }
        }
        Ok(value["next_cursor"]
            .as_str()
            .map(|cursor| Next::Transcript(Some(cursor.to_owned()))))
    }

    /// The meeting `id` when it still passes the filters in the index.
    fn meeting(&self, id: &MeetingId) -> Result<Option<MeetingHit>, TranscriptStep> {
        let query = SearchQuery {
            meeting: Some(id.clone()),
            ..self.filters.clone()
        };
        let hits = self
            .index
            .search_meetings(&query)
            .map_err(TranscriptStep::from_index)?;
        Ok(hits.into_iter().next())
    }
}

fn has_text(query: &SearchQuery) -> bool {
    query.text.as_deref().is_some_and(|t| !t.trim().is_empty())
}

/// `broscribe search --query-stdin --page-size N [--cursor C]` for `query`:
/// dates, app, source and the opaque cursor in argv. The query's textual
/// filters, which can contain transcript words or names, go on stdin.
fn transcript_command(
    query: &SearchQuery,
    page_size: usize,
    after: Option<&str>,
) -> (Vec<OsString>, Vec<u8>) {
    let mut args: Vec<OsString> = vec![
        "search".into(),
        "--query-stdin".into(),
        "--mode".into(),
        "auto".into(),
        "--page-size".into(),
        page_size.to_string().into(),
    ];
    if let Some(cursor) = after {
        args.extend(["--cursor".into(), cursor.into()]);
    }
    for (flag, value) in [
        ("--app", query.app.clone()),
        (
            "--source",
            query.source.map(|source| match source {
                broscribe_core::schema::Source::Recording => "recorded".into(),
                broscribe_core::schema::Source::Import => "imported".into(),
            }),
        ),
        ("--since", query.since.map(|v| v.to_rfc3339())),
        ("--until", query.until.map(|v| v.to_rfc3339())),
    ] {
        if let Some(value) = value {
            args.extend([flag.into(), value.into()]);
        }
    }
    let mut stdin = serde_json::json!({
        "query": query.text.as_deref().unwrap_or(""),
        "person": query.person,
    });
    if let Some(tag) = &query.tag {
        stdin["tag"] = serde_json::json!(tag);
    }
    if let Some(topic) = &query.topic {
        stdin["topic"] = serde_json::json!(topic);
    }
    (args, stdin.to_string().into_bytes())
}

#[cfg(test)]
mod tests {
    use broscribe_core::Library;
    use broscribe_core::schema::{Meeting, Source};
    use chrono::TimeDelta;

    use super::*;

    /// Indexes a meeting with only a title, `minutes_ago` minutes old.
    fn index_meeting(lib: &Library, title: &str, minutes_ago: i64, source: Source) -> MeetingId {
        let started = broscribe_core::schema::now() - TimeDelta::minutes(minutes_ago);
        let mut meeting = Meeting::new(MeetingId::random(), source, started);
        meeting.title = Some(title.into());
        broscribe_core::ops::with_index(lib, |tx, index| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)?;
            index.upsert_meeting(tx, &meeting)?;
            Ok(())
        })
        .unwrap();
        meeting.id
    }

    fn library(dir: &Path) -> Library {
        Library::open(dir.join("lib")).unwrap()
    }

    /// A CLI that must never run (reads without text use the index only).
    fn no_cli(dir: &Path, lib: &Library) -> CliRunner {
        crate::test_support::runner(dir.join("no-such-cli"), lib.root().into())
    }

    fn sizes(meetings: usize, transcript: usize) -> PageSizes {
        PageSizes {
            meetings,
            transcript,
        }
    }

    fn ids(search: &MeetingSearch) -> Vec<MeetingId> {
        search
            .results()
            .iter()
            .map(|h| h.meeting_id.clone())
            .collect()
    }

    fn text_query(text: &str) -> SearchQuery {
        SearchQuery {
            text: Some(text.into()),
            ..SearchQuery::default()
        }
    }

    /// The transcript failure as `(code, message, retry_more)`.
    #[cfg(unix)]
    fn transcript(search: &MeetingSearch) -> Option<(&str, &str, bool)> {
        search
            .transcript_error()
            .map(|e| (e.code.as_str(), e.message.as_str(), e.retry_more))
    }

    /// Runs the next read to completion and applies it.
    fn run(search: &mut MeetingSearch, job: PageJob, lib: &Library, cli: &CliRunner) {
        let generation = job.generation();
        let outcome = job.run(lib.root(), cli);
        assert!(search.apply(generation, outcome));
    }

    #[test]
    fn meetings_load_one_page_at_a_time_in_index_order() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        let newest_first: Vec<MeetingId> = (0..5)
            .map(|i| index_meeting(&lib, &format!("Synthetic {i}"), i, Source::Import))
            .collect();
        let cli = no_cli(dir.path(), &lib);
        let mut search = MeetingSearch::with_page_sizes(sizes(2, 10));

        let job = search.start(SearchQuery::default());
        assert_eq!(search.loading(), Some(Load::Search));
        run(&mut search, job, &lib, &cli);
        assert_eq!(ids(&search), newest_first[..2]);
        assert!(search.has_more());
        assert_eq!(search.loading(), None);

        let job = search.more().unwrap();
        assert!(search.more().is_none(), "one read at a time");
        run(&mut search, job, &lib, &cli);
        let job = search.more().unwrap();
        run(&mut search, job, &lib, &cli);
        assert_eq!(ids(&search), newest_first);
        assert!(!search.has_more());
        assert!(search.more().is_none());
        assert_eq!(search.error(), None);

        // A filter is part of the paged read, not applied afterwards.
        let recorded = index_meeting(&lib, "Recorded", 10, Source::Recording);
        let job = search.start(SearchQuery {
            source: Some(Source::Recording),
            ..SearchQuery::default()
        });
        run(&mut search, job, &lib, &cli);
        assert_eq!(ids(&search), [recorded]);
        assert!(!search.has_more());
    }

    #[test]
    fn a_stale_cursor_restarts_from_the_first_page_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        for i in 0..4 {
            index_meeting(&lib, &format!("Synthetic {i}"), i + 1, Source::Import);
        }
        let cli = no_cli(dir.path(), &lib);
        let mut search = MeetingSearch::with_page_sizes(sizes(2, 10));
        let job = search.start(SearchQuery::default());
        run(&mut search, job, &lib, &cli);
        assert_eq!(search.results().len(), 2);

        // The index changes between pages: the cursor of page one is stale.
        let newest = index_meeting(&lib, "Newer", 0, Source::Import);
        let job = search.more().unwrap();
        let generation = job.generation();
        let batch = job.run(lib.root(), &cli).unwrap();
        assert!(batch.replaces(), "a restart replaces the list");
        assert!(search.apply(generation, Ok(batch)));
        let listed = ids(&search);
        assert_eq!(
            listed.len(),
            4,
            "as far as listed plus one page: {listed:?}"
        );
        assert_eq!(listed[0], newest);
        let unique: BTreeSet<_> = listed.iter().collect();
        assert_eq!(unique.len(), listed.len());
        assert!(search.has_more());
    }

    #[test]
    fn a_refresh_reads_as_far_as_listed() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        for i in 0..5 {
            index_meeting(&lib, &format!("Synthetic {i}"), i + 1, Source::Import);
        }
        let cli = no_cli(dir.path(), &lib);
        let mut search = MeetingSearch::with_page_sizes(sizes(2, 10));
        let job = search.start(SearchQuery::default());
        run(&mut search, job, &lib, &cli);
        let job = search.more().unwrap();
        run(&mut search, job, &lib, &cli);
        assert_eq!(search.results().len(), 4);
        let revision = search.revision();

        index_meeting(&lib, "Newer", 0, Source::Import);
        let job = search.refresh();
        assert_eq!(search.loading(), Some(Load::Refresh));
        run(&mut search, job, &lib, &cli);
        assert_eq!(search.results().len(), 4);
        assert_eq!(search.results()[0].title.as_deref(), Some("Newer"));
        assert!(search.revision() > revision);
        assert!(search.has_more());
    }

    #[test]
    fn superseded_reads_are_cancelled_and_never_applied() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        index_meeting(&lib, "Synthetic", 1, Source::Import);
        let cli = no_cli(dir.path(), &lib);
        let mut search = MeetingSearch::with_page_sizes(sizes(2, 10));

        let first = search.start(text_query("alpha"));
        let second = search.start(text_query("beta"));
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        assert_eq!(search.query().text.as_deref(), Some("beta"));
        // The superseded read stops before reading anything.
        let first_generation = first.generation();
        assert_eq!(
            first.run(lib.root(), &cli).unwrap_err(),
            ReadError::Cancelled
        );
        let stale = search.apply(
            first_generation,
            Err(ReadError::Failed(SearchError {
                code: "internal",
                message: "late".into(),
            })),
        );
        assert!(!stale);
        assert_eq!(search.error(), None);
        assert_eq!(search.loading(), Some(Load::Search));

        // Quitting cancels the current read as well.
        search.cancel();
        assert!(second.is_cancelled());
        assert_eq!(search.loading(), None);
        let generation = second.generation();
        let outcome = second.run(lib.root(), &cli);
        assert!(!search.apply(generation, outcome));
    }

    #[test]
    fn a_new_library_lists_nothing_and_an_unusable_index_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        let cli = no_cli(dir.path(), &lib);
        let mut search = MeetingSearch::default();
        let job = search.start(SearchQuery::default());
        run(&mut search, job, &lib, &cli);
        assert!(search.results().is_empty() && !search.has_more());
        assert_eq!(search.error(), None);

        // An index that exists but cannot be read is reported, never
        // replaced by a scan of the meeting folders.
        index_meeting(&lib, "Synthetic", 1, Source::Import);
        std::fs::write(Index::path(lib.root()), b"not a database").unwrap();
        let job = search.refresh();
        run(&mut search, job, &lib, &cli);
        assert!(search.error().is_some());
        assert!(!search.has_more());
    }

    #[cfg(unix)]
    mod transcript {
        use std::os::unix::fs::PermissionsExt as _;

        use super::*;

        /// A fake CLI whose `search` prints `first` without `--cursor` and
        /// `second` with `--cursor c2`; it appends its argv to `search-args`
        /// and writes its stdin to `search-stdin`.
        fn fake_cli(
            dir: &Path,
            lib: &Library,
            first: &serde_json::Value,
            second: &serde_json::Value,
        ) -> CliRunner {
            let executable = dir.join("search-cli");
            std::fs::write(
                &executable,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncat > '{}'\ncase \"$*\" in\n  *'--cursor c2'*) printf '%s' '{}' ;;\n  *) printf '%s' '{}' ;;\nesac\n",
                    dir.join("search-args").display(),
                    dir.join("search-stdin").display(),
                    second,
                    first,
                ),
            )
            .unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            crate::test_support::runner(executable, lib.root().into())
        }

        fn argv(dir: &Path) -> Vec<String> {
            std::fs::read_to_string(dir.join("search-args"))
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        #[test]
        fn metadata_matches_come_first_then_transcript_pages_each_meeting_once() {
            let dir = tempfile::tempdir().unwrap();
            let lib = library(dir.path());
            let title_only = index_meeting(&lib, "Zephyr roadmap", 1, Source::Import);
            let both = index_meeting(&lib, "Zephyr review", 2, Source::Import);
            let transcript_only = index_meeting(&lib, "Weekly sync", 3, Source::Import);
            let second_page = index_meeting(&lib, "Planning", 4, Source::Import);
            let recorded = index_meeting(&lib, "Recorded call", 5, Source::Recording);
            let first = serde_json::json!({"mode":"keyword","results":[
                {"meeting_id":transcript_only,"snippet":"the **zephyr** draft"},
                {"meeting_id":both,"snippet":"we discussed **zephyr**"},
                {"meeting_id":transcript_only,"snippet":"a later **zephyr** line"}
            ],"revision":{"instance":"x","revision":1},"next_cursor":"c2"});
            let second = serde_json::json!({"mode":"keyword","results":[
                {"meeting_id":recorded,"snippet":"**zephyr** on a call"},
                {"meeting_id":second_page,"snippet":"**zephyr** budget"}
            ],"revision":{"instance":"x","revision":1}});
            let cli = fake_cli(dir.path(), &lib, &first, &second);
            let mut search = MeetingSearch::with_page_sizes(sizes(1, 3));
            // Imported meetings only: the transcript match of a recording
            // is not listed.
            let job = search.start(SearchQuery {
                source: Some(Source::Import),
                ..text_query("zephyr")
            });
            run(&mut search, job, &lib, &cli);
            assert_eq!(search.results().len(), 1, "one page of metadata matches");
            while let Some(job) = search.more() {
                run(&mut search, job, &lib, &cli);
                assert!(search.error().is_none(), "{}", search.error().unwrap_or(""));
            }
            let listed = ids(&search);
            assert_eq!(listed.len(), 4, "each meeting once: {listed:?}");
            let metadata: BTreeSet<_> = listed[..2].iter().collect();
            assert_eq!(metadata, BTreeSet::from([&title_only, &both]));
            assert_eq!(listed[2..], [transcript_only.clone(), second_page]);
            let hit = |id: &MeetingId| {
                search
                    .results()
                    .iter()
                    .find(|h| h.meeting_id == *id)
                    .unwrap()
            };
            assert_eq!(
                hit(&both).snippet.as_deref(),
                Some("we discussed **zephyr**")
            );
            assert_eq!(
                hit(&transcript_only).snippet.as_deref(),
                Some("the **zephyr** draft")
            );
            assert!(
                hit(&title_only)
                    .snippet
                    .as_deref()
                    .unwrap()
                    .contains("Zephyr")
            );

            // Every transcript read is one bounded page, continued by its
            // cursor; no unbounded limit.
            let argv = argv(dir.path());
            assert_eq!(argv.len(), 2, "{argv:?}");
            assert!(argv.iter().all(|a| a.contains("--page-size 3")), "{argv:?}");
            assert!(argv.iter().all(|a| !a.contains("--limit")), "{argv:?}");
            assert!(!argv[0].contains("--cursor"));
            assert!(argv[1].contains("--cursor c2"));
        }

        #[test]
        fn search_text_person_and_tag_reach_the_child_on_stdin_only() {
            let dir = tempfile::tempdir().unwrap();
            let lib = library(dir.path());
            index_meeting(&lib, "Unrelated title", 1, Source::Import);
            let empty =
                serde_json::json!({"results": [], "revision": {"instance":"x","revision":1}});
            let cli = fake_cli(dir.path(), &lib, &empty, &empty);
            let mut search = MeetingSearch::default();
            let job = search.start(SearchQuery {
                person: Some("Synthetic Person".into()),
                tag: Some("planning".into()),
                ..text_query("quoted transcript words")
            });
            run(&mut search, job, &lib, &cli);
            let argv = argv(dir.path()).join("\n");
            assert!(argv.contains("search --query-stdin"), "{argv}");
            assert!(
                !argv.contains("planning") && !argv.contains("--tag"),
                "{argv}"
            );
            assert!(!argv.contains("quoted transcript"), "{argv}");
            assert!(
                !argv.contains("Synthetic") && !argv.contains("--person"),
                "{argv}"
            );
            let stdin: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.path().join("search-stdin")).unwrap())
                    .unwrap();
            assert_eq!(
                stdin,
                serde_json::json!({"query": "quoted transcript words", "person": "Synthetic Person", "tag": "planning"})
            );
        }

        #[test]
        fn a_stale_transcript_cursor_restarts_and_other_failures_keep_the_list() {
            let dir = tempfile::tempdir().unwrap();
            let lib = library(dir.path());
            let a = index_meeting(&lib, "Alpha", 1, Source::Import);
            index_meeting(&lib, "Beta", 2, Source::Import);
            let first = serde_json::json!({"results":[
                {"meeting_id":a,"snippet":"**word** one"}
            ],"revision":{"instance":"x","revision":1},"next_cursor":"c2"});
            let stale = serde_json::json!({"error":{"code":"cursor_stale","message":"changed"}});
            let cli = fake_cli(dir.path(), &lib, &first, &stale);
            let mut search = MeetingSearch::with_page_sizes(sizes(1, 1));
            let job = search.start(text_query("word"));
            run(&mut search, job, &lib, &cli);
            assert_eq!(ids(&search).as_slice(), std::slice::from_ref(&a));
            assert!(search.has_more());
            // Every continuation is stale: bounded restarts, then an error
            // with the listed meetings kept.
            let job = search.more().unwrap();
            run(&mut search, job, &lib, &cli);
            assert!(search.error().unwrap().contains("kept changing"));
            assert_eq!(ids(&search), [a]);
            assert!(search.has_more(), "a failed Load more can be tried again");
            let calls = argv(dir.path()).len();
            assert!(calls <= 1 + 2 * (STALE_RESTARTS + 1), "{calls}");

            let failing =
                serde_json::json!({"error":{"code":"internal","message":"synthetic failure"}});
            let cli = fake_cli(dir.path(), &lib, &failing, &failing);
            let job = search.refresh();
            run(&mut search, job, &lib, &cli);
            // A transcript CLI failure is non-fatal: results are replaced with
            // the metadata hits of the current query (none here) and the
            // transcript error is recorded apart from fatal `error`.
            assert_eq!(search.error(), None);
            assert_eq!(
                transcript(&search),
                Some(("internal", "Something went wrong inside BroScribe.", false))
            );
            assert!(ids(&search).is_empty());
            assert!(!search.has_more());
        }

        #[test]
        fn failed_transcript_search_replaces_previous_query_with_metadata_hits() {
            let dir = tempfile::tempdir().unwrap();
            let lib = library(dir.path());
            let alpha = index_meeting(&lib, "Alpha kickoff", 1, Source::Import);
            let budget = index_meeting(&lib, "Budget review", 2, Source::Import);
            let other = index_meeting(&lib, "Weekly sync", 3, Source::Import);

            let ok_dir = dir.path().join("ok");
            let fail_dir = dir.path().join("fail");
            std::fs::create_dir_all(&ok_dir).unwrap();
            std::fs::create_dir_all(&fail_dir).unwrap();
            let empty =
                serde_json::json!({"results": [], "revision": {"instance":"x","revision":1}});
            let ok_cli = fake_cli(&ok_dir, &lib, &empty, &empty);
            let failing = serde_json::json!({"error":{"code":"unavailable","message":"transcript index busy"}});
            let failing_cli = fake_cli(&fail_dir, &lib, &failing, &failing);

            let mut search = MeetingSearch::default();

            // Query A: lists Alpha kickoff, Budget review and Weekly sync.
            let job = search.start(SearchQuery::default());
            run(&mut search, job, &lib, &ok_cli);
            assert_eq!(ids(&search), [alpha.clone(), budget.clone(), other]);
            assert_eq!(search.transcript_error(), None);

            // Query B ("budget") with failing transcript step: replaces query A's
            // rows with B's metadata hits only and records transcript_error.
            let job = search.start(text_query("budget"));
            run(&mut search, job, &lib, &failing_cli);
            assert_eq!(ids(&search), [budget]);
            assert_eq!(search.error(), None);
            assert_eq!(
                transcript(&search),
                Some(("unavailable", "Transcript index busy.", false))
            );
            assert!(!search.has_more());

            // Query C ("nomatch") with failing transcript step and zero metadata
            // hits: clears B's rows so nothing from an earlier query stays listed.
            let job = search.start(text_query("nomatch"));
            run(&mut search, job, &lib, &failing_cli);
            assert!(ids(&search).is_empty());
            assert_eq!(search.error(), None);
            assert_eq!(
                transcript(&search),
                Some(("unavailable", "Transcript index busy.", false))
            );

            // A subsequent clean search clears transcript_error.
            let job = search.start(text_query("alpha"));
            run(&mut search, job, &lib, &ok_cli);
            assert_eq!(ids(&search), [alpha]);
            assert_eq!(search.transcript_error(), None);
            assert_eq!(search.error(), None);
        }

        /// Two fake CLIs in their own folders: `failing` answers the first
        /// transcript page with `first` and the `c2` page with an internal
        /// error; `ok` answers the `c2` page with `second`.
        fn failing_then_ok(
            dir: &Path,
            lib: &Library,
            first: &serde_json::Value,
            second: &serde_json::Value,
        ) -> (std::path::PathBuf, CliRunner, std::path::PathBuf, CliRunner) {
            let (failing_dir, ok_dir) = (dir.join("failing"), dir.join("ok"));
            std::fs::create_dir_all(&failing_dir).unwrap();
            std::fs::create_dir_all(&ok_dir).unwrap();
            let error = serde_json::json!({"error":{"code":"internal","message":"index is busy"}});
            let failing = fake_cli(&failing_dir, lib, first, &error);
            let ok = fake_cli(&ok_dir, lib, first, second);
            (failing_dir, failing, ok_dir, ok)
        }

        #[test]
        fn a_first_read_whose_transcript_page_fails_lists_only_title_people_and_tag_matches() {
            let dir = tempfile::tempdir().unwrap();
            let lib = library(dir.path());
            let roadmap = index_meeting(&lib, "Zephyr roadmap", 1, Source::Import);
            let weekly = index_meeting(&lib, "Weekly sync", 2, Source::Import);
            let planning = index_meeting(&lib, "Planning", 3, Source::Import);
            let first = serde_json::json!({"results":[
                {"meeting_id":weekly,"snippet":"the **zephyr** draft"},
                {"meeting_id":roadmap,"snippet":"we said **zephyr**"}
            ],"revision":{"instance":"x","revision":1},"next_cursor":"c2"});
            let second = serde_json::json!({"results":[
                {"meeting_id":planning,"snippet":"**zephyr** budget"}
            ],"revision":{"instance":"x","revision":1}});
            let (_, failing, _, ok) = failing_then_ok(dir.path(), &lib, &first, &second);
            let mut search = MeetingSearch::with_page_sizes(sizes(10, 2));

            // The first transcript page is read, the second fails: what the
            // first page found (a transcript-only meeting, a snippet) goes,
            // so the list is what the message promises.
            let job = search.start(text_query("zephyr"));
            run(&mut search, job, &lib, &failing);
            assert_eq!(ids(&search).as_slice(), std::slice::from_ref(&roadmap));
            let snippet = search.results()[0].snippet.as_deref().unwrap_or_default();
            assert!(!snippet.contains("we said"), "{snippet}");
            assert_eq!(
                transcript(&search),
                Some(("internal", "Something went wrong inside BroScribe.", false))
            );
            assert_eq!(search.error(), None);
            assert!(!search.has_more());

            // Try again runs the search from its first page.
            let job = search.start(text_query("zephyr"));
            run(&mut search, job, &lib, &ok);
            assert_eq!(ids(&search), [roadmap, weekly, planning]);
            assert_eq!(search.transcript_error(), None);
        }

        #[test]
        fn a_transcript_failure_during_load_more_keeps_the_failed_page_to_try_again() {
            let dir = tempfile::tempdir().unwrap();
            let lib = library(dir.path());
            let roadmap = index_meeting(&lib, "Zephyr roadmap", 1, Source::Import);
            let weekly = index_meeting(&lib, "Weekly sync", 2, Source::Import);
            let planning = index_meeting(&lib, "Planning", 3, Source::Import);
            let first = serde_json::json!({"results":[
                {"meeting_id":weekly,"snippet":"the **zephyr** draft"}
            ],"revision":{"instance":"x","revision":1},"next_cursor":"c2"});
            let second = serde_json::json!({"results":[
                {"meeting_id":planning,"snippet":"**zephyr** budget"}
            ],"revision":{"instance":"x","revision":1}});
            let (failing_dir, failing, ok_dir, ok) =
                failing_then_ok(dir.path(), &lib, &first, &second);
            let mut search = MeetingSearch::with_page_sizes(sizes(1, 1));

            let job = search.start(text_query("zephyr"));
            run(&mut search, job, &lib, &failing);
            assert_eq!(ids(&search).as_slice(), std::slice::from_ref(&roadmap));
            // A successful transcript page.
            let job = search.more().unwrap();
            run(&mut search, job, &lib, &failing);
            assert_eq!(ids(&search), [roadmap.clone(), weekly.clone()]);
            assert!(search.has_more());

            // The next page fails (not a cursor error): the listed meetings
            // stay, and so does Load more, at the failed page.
            let job = search.more().unwrap();
            run(&mut search, job, &lib, &failing);
            assert_eq!(ids(&search), [roadmap.clone(), weekly.clone()]);
            assert_eq!(
                transcript(&search),
                Some(("internal", "Something went wrong inside BroScribe.", true))
            );
            assert_eq!(search.error(), None);
            assert!(search.has_more(), "Load more reads the failed page again");
            let failing_calls = argv(&failing_dir);
            assert_eq!(failing_calls.len(), 2, "{failing_calls:?}");
            assert!(failing_calls[1].contains("--cursor c2"));

            // Trying again reads that page, not the first one.
            let job = search.more().unwrap();
            run(&mut search, job, &lib, &ok);
            assert_eq!(ids(&search), [roadmap, weekly, planning]);
            assert_eq!(search.transcript_error(), None);
            assert!(!search.has_more());
            let retried = argv(&ok_dir);
            assert_eq!(retried.len(), 1, "{retried:?}");
            assert!(retried[0].contains("--cursor c2"), "{retried:?}");
        }
    }

    #[test]
    fn a_failed_refresh_that_superseded_a_new_query_lists_none_of_the_old_rows() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        index_meeting(&lib, "Alpha one", 1, Source::Import);
        index_meeting(&lib, "Alpha two", 2, Source::Import);
        let cli = no_cli(dir.path(), &lib);
        let mut search = MeetingSearch::default();

        // Query A succeeds.
        let job = search.start(SearchQuery::default());
        run(&mut search, job, &lib, &cli);
        assert_eq!(search.results().len(), 2);

        // Query B starts; A's meetings stay listed while it is read.
        let b = search.start(SearchQuery {
            source: Some(Source::Recording),
            ..SearchQuery::default()
        });
        assert_eq!(search.results().len(), 2);
        // A library change or a window activation refreshes before B lands:
        // it supersedes B and is B's first read itself.
        let refresh = search.refresh();
        assert!(b.is_cancelled());
        assert_eq!(search.loading(), Some(Load::Search));

        // The refresh fails.
        std::fs::write(Index::path(lib.root()), b"not a database").unwrap();
        let revision = search.revision();
        run(&mut search, refresh, &lib, &cli);
        assert!(search.error().is_some());
        assert!(search.results().is_empty(), "none of A's rows remain");
        assert!(!search.has_more());
        assert!(search.revision() > revision);
        // B's own answer comes too late to apply.
        let generation = b.generation();
        assert!(!search.apply(generation, b.run(lib.root(), &cli)));
        assert!(search.results().is_empty());

        // Nothing listed for B, so a later refresh is still a first read.
        let job = search.refresh();
        assert_eq!(search.loading(), Some(Load::Search));
        run(&mut search, job, &lib, &cli);
        assert!(search.results().is_empty());
    }

    #[test]
    fn load_more_never_continues_an_earlier_querys_pages() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        for i in 0..3 {
            index_meeting(&lib, &format!("Synthetic {i}"), i + 1, Source::Import);
        }
        let cli = no_cli(dir.path(), &lib);
        let mut search = MeetingSearch::with_page_sizes(sizes(1, 10));
        let job = search.start(SearchQuery::default());
        run(&mut search, job, &lib, &cli);
        assert!(search.has_more());

        // A new query's read is stopped (quitting) before it lands: the
        // listed page and its next page are the old query's.
        let _ = search.start(text_query("other"));
        search.cancel();
        assert!(search.more().is_none());
    }

    #[test]
    fn failed_first_search_clears_previous_rows_while_refresh_and_more_keep_them() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        let alpha_1 = index_meeting(&lib, "Alpha one", 1, Source::Import);
        index_meeting(&lib, "Alpha two", 2, Source::Import);
        index_meeting(&lib, "Beta review", 3, Source::Import);
        let cli = no_cli(dir.path(), &lib);

        let mut search = MeetingSearch::with_page_sizes(sizes(1, 10));
        let job = search.start(SearchQuery::default());
        run(&mut search, job, &lib, &cli);
        assert_eq!(ids(&search).as_slice(), std::slice::from_ref(&alpha_1));
        assert!(search.has_more());

        // Corrupt the index so subsequent reads fail completely.
        std::fs::write(Index::path(lib.root()), b"not a database").unwrap();

        // Load::More failure preserves the existing rows AND has_more.
        let job = search.more().unwrap();
        run(&mut search, job, &lib, &cli);
        assert!(search.error().is_some());
        assert_eq!(ids(&search).as_slice(), std::slice::from_ref(&alpha_1));
        assert!(search.has_more());

        // Load::Refresh total failure preserves the existing rows and clears has_more.
        let job = search.refresh();
        run(&mut search, job, &lib, &cli);
        assert!(search.error().is_some());
        assert_eq!(ids(&search), [alpha_1]);
        assert!(!search.has_more());

        // Load::Search (new query B) total failure clears query A's rows and
        // bumps revision so views rebuild their row list.
        let revision_before = search.revision();
        let job = search.start(text_query("beta"));
        run(&mut search, job, &lib, &cli);
        assert!(search.error().is_some());
        assert_eq!(search.transcript_error(), None);
        assert!(
            ids(&search).is_empty(),
            "new query failure must clear old rows"
        );
        assert!(!search.has_more());
        assert!(search.revision() > revision_before);
    }

    #[test]
    fn stale_generation_with_transcript_error_or_failure_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        let alpha = index_meeting(&lib, "Alpha", 1, Source::Import);
        let cli = no_cli(dir.path(), &lib);

        let mut search = MeetingSearch::default();
        let first = search.start(text_query("alpha"));
        let first_gen = first.generation();
        let second = search.start(SearchQuery::default());
        run(&mut search, second, &lib, &cli);
        assert_eq!(ids(&search).as_slice(), std::slice::from_ref(&alpha));
        assert_eq!(search.transcript_error(), None);

        // Late partial batch with transcript_error from the superseded query
        // must not overwrite the newer query's results or errors.
        let stale_batch = PageBatch {
            replace: true,
            hits: Vec::new(),
            snippets: Vec::new(),
            next: None,
            listed: BTreeSet::new(),
            matched: BTreeSet::new(),
            transcript_error: Some(TranscriptError {
                code: "internal".into(),
                message: "late error".into(),
                retry_more: false,
            }),
            ..PageBatch::empty()
        };
        assert!(!search.apply(first_gen, Ok(stale_batch)));
        assert!(!search.apply(
            first_gen,
            Err(ReadError::Failed(SearchError {
                code: "internal",
                message: "late fatal".into(),
            }))
        ));
        assert_eq!(ids(&search), [alpha]);
        assert_eq!(search.transcript_error(), None);
        assert_eq!(search.error(), None);
    }
}
