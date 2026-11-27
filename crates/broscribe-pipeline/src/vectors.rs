//! Text vectors for semantic search (plan M2 §9.1–§9.3).
//!
//! Vectors are derived data cached in `.index/vectors.sqlite` by
//! (text-model key, SHA-256 of a window's text). This module finds the window
//! texts without a vector, embeds them **outside** the library lock (the model
//! loads lazily, only when something is missing), then stores each batch in a
//! short [`Library::write`] after re-checking that the meeting's window
//! revision has not changed meanwhile. Naming a speaker or renaming a person
//! changes no window text, so it never costs an embedding.
//!
//! The text model is chosen by `profile.text_embedding`: a registry model
//! (local, no consent needed) or a text-embedding provider, which receives
//! nothing unless the user enabled it and accepted its consent text — for
//! document vectors, query vectors and rebuilds alike.

use std::collections::HashMap;
use std::path::Path;

use broscribe_core::config::Config;
use broscribe_core::index::{
    Index, PendingWindow, QueryVector, RebuildReport, SearchMode, SearchQuery, VectorCache,
};
use broscribe_core::{Error, Library, MeetingId, Result};
use broscribe_models::llm::http::api_key_secret;
use broscribe_models::llm::registry::{ProviderPurpose, ProviderRegistry};
use broscribe_models::text_embedding::{HttpEmbedder, MODEL2VEC_RUNTIME, Model2VecEmbedder};
use broscribe_models::{
    ExecutionContext, ModelStore, Registry, Role, TRUST_STORE_UNAVAILABLE, TextEmbedder,
};
use broscribe_platform::SecretStore;
use serde::Serialize;

/// Window texts per embedding call and per short write transaction.
pub const EMBED_BATCH: usize = 256;

/// The configured text model, or why there is none.
pub enum TextModel {
    Ready(Box<dyn TextEmbedder>),
    /// Semantic search is off: not installed, not configured, or the
    /// provider is not enabled with consent. Keyword search still works.
    Unavailable {
        id: String,
        reason: String,
    },
}

impl TextModel {
    pub fn embedder(&self) -> Option<&dyn TextEmbedder> {
        match self {
            TextModel::Ready(e) => Some(e.as_ref()),
            TextModel::Unavailable { .. } => None,
        }
    }
}

/// Resolves `profile.text_embedding` to an embedder without loading model
/// weights or sending anything. A registry model must be installed (its
/// files are read on first use, never over the network); a provider must be
/// a text-embedding provider, enabled with recorded consent (otherwise it is
/// [`TextModel::Unavailable`] and receives no request). Its API key, if any,
/// is read from `secrets`; a key that cannot be read makes it unavailable
/// too (keyword search still works). A provider's requests run under
/// `execution`: an `https` server without the OS trust roots is unavailable
/// as well, refused before connecting, while local models are unaffected.
pub fn text_model(
    config: &Config,
    data_dir: &Path,
    secrets: &dyn SecretStore,
    execution: &ExecutionContext,
) -> Result<TextModel> {
    let id = config.profile.text_embedding.trim();
    let unavailable = |reason: String| {
        Ok(TextModel::Unavailable {
            id: id.to_owned(),
            reason,
        })
    };
    if id.is_empty() {
        return unavailable("no text-embedding model is configured".into());
    }
    let registry = Registry::load(data_dir)?;
    if let Some(entry) = registry.get(id) {
        if !entry.has_role(Role::TextEmbedding) {
            return Err(Error::InvalidInput(format!(
                "model {id} is not a text-embedding model"
            )));
        }
        if entry.runtime != MODEL2VEC_RUNTIME {
            return Err(Error::Unsupported(format!(
                "text-embedding runtime {} (model {id})",
                entry.runtime
            )));
        }
        return match Model2VecEmbedder::new(entry, &ModelStore::new(data_dir)) {
            Ok(e) => Ok(TextModel::Ready(Box::new(e))),
            Err(e) if e.code() == "model_missing" => unavailable(e.to_string()),
            Err(e) => Err(e),
        };
    }
    let providers = ProviderRegistry::load(data_dir)?;
    let Some(entry) = providers.get(id) else {
        return Err(Error::InvalidInput(format!(
            "text_embedding {id} is neither a registry model nor a provider"
        )));
    };
    if entry.purpose != ProviderPurpose::TextEmbedding {
        return Err(Error::InvalidInput(format!(
            "provider {id} is not a text-embedding provider"
        )));
    }
    let Some(pc) = config.provider(id).filter(|p| p.is_consented()) else {
        return unavailable(format!(
            "provider {id} is not enabled; enable it and accept its consent text first"
        ));
    };
    let key = match secrets.get(&api_key_secret(id)) {
        Ok(key) => key,
        Err(e) => return unavailable(format!("reading the API key of provider {id} failed: {e}")),
    };
    match HttpEmbedder::new(entry, pc, key, execution) {
        Ok(e) => Ok(TextModel::Ready(Box::new(e))),
        Err(e) if e.code() == TRUST_STORE_UNAVAILABLE => unavailable(e.to_string()),
        Err(e) => Err(e),
    }
}

/// What [`embed_missing`] did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct VectorReport {
    /// Vector-cache key of the text model.
    pub model: String,
    /// Distinct window texts that had no vector.
    pub pending: usize,
    pub embedded: usize,
    /// Vectors dropped because their meeting's windows changed during
    /// embedding (the next pass embeds the new text).
    pub stale: usize,
}

/// Embeds every window text (of `meeting`, or of the whole library) that
/// has no vector for `embedder` yet. Inference runs outside the lock; each
/// batch is stored in a short write transaction, only for meetings whose
/// window revision is still the one read before embedding. Needs the index.
pub fn embed_missing(
    lib: &Library,
    embedder: &dyn TextEmbedder,
    meeting: Option<&MeetingId>,
) -> Result<VectorReport> {
    let model = embedder.model_key().to_owned();
    let (pending, revisions) = {
        let index = Index::open_readonly(lib.root())?;
        let pending = index.pending_windows(&model, meeting)?;
        let mut revisions: HashMap<MeetingId, Option<String>> = HashMap::new();
        for p in &pending {
            if !revisions.contains_key(&p.meeting_id) {
                let rev = index.window_revision(&p.meeting_id)?;
                revisions.insert(p.meeting_id.clone(), rev);
            }
        }
        (pending, revisions)
    };
    let mut cache = VectorCache::open(lib.root())?;
    let present = cache.present(&model, pending.iter().map(|p| p.text_sha256.as_str()))?;
    let missing: Vec<PendingWindow> = pending
        .into_iter()
        .filter(|p| !present.contains(&p.text_sha256))
        .collect();

    let mut report = VectorReport {
        model: model.clone(),
        pending: missing.len(),
        ..VectorReport::default()
    };
    for batch in missing.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|p| p.text.clone()).collect();
        let vectors = embedder.embed(&texts)?;
        if vectors.len() != batch.len() {
            return Err(Error::Inference(format!(
                "text model {model} returned {} vectors for {} texts",
                vectors.len(),
                batch.len()
            )));
        }
        let (stored, stale) = lib.write(|tx| {
            let index = Index::open_readonly(tx.root())?;
            let mut current: HashMap<&MeetingId, bool> = HashMap::new();
            let mut accepted = Vec::with_capacity(batch.len());
            let mut stale = 0;
            for (p, v) in batch.iter().zip(&vectors) {
                let unchanged = match current.get(&p.meeting_id) {
                    Some(ok) => *ok,
                    None => {
                        let ok = index.window_revision(&p.meeting_id)?
                            == revisions.get(&p.meeting_id).cloned().flatten();
                        current.insert(&p.meeting_id, ok);
                        ok
                    }
                };
                if unchanged {
                    accepted.push((p.text_sha256.clone(), v.clone()));
                } else {
                    stale += 1;
                }
            }
            let stored = cache.insert(tx, &model, &accepted)?;
            Ok((stored, stale))
        })?;
        report.embedded += stored;
        report.stale += stale;
    }
    Ok(report)
}

/// Search vectors for what queue work completed, outside the lock: every
/// meeting after a resolved manifest batch, else the imported, recorded and
/// reprocessed meetings. Nothing when the work completed none.
pub fn embed_completed(
    lib: &Library,
    embedder: &dyn TextEmbedder,
    summary: &crate::worker::WorkerSummary,
) -> Result<()> {
    if summary.batches_resolved {
        return embed_missing(lib, embedder, None).map(|_| ());
    }
    summary
        .imported
        .iter()
        .try_for_each(|id| embed_missing(lib, embedder, Some(id)).map(|_| ()))
}

/// What [`rebuild_index`] did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RebuildOutcome {
    pub index: RebuildReport,
    /// The vector pass, when a text model is available.
    pub vectors: Option<VectorReport>,
    /// Why the vector pass failed; the index itself was rebuilt.
    pub vectors_error: Option<String>,
}

/// `index rebuild`: rebuilds the relational and FTS tables from the files in
/// one transaction (no text model needed; unused cache entries are pruned),
/// then fills missing vectors outside the lock when `embedder` is given.
pub fn rebuild_index(lib: &Library, embedder: Option<&dyn TextEmbedder>) -> Result<RebuildOutcome> {
    let index = lib.write(|tx| Index::rebuild(tx))?;
    let mut out = RebuildOutcome {
        index,
        ..RebuildOutcome::default()
    };
    if let Some(e) = embedder {
        match embed_missing(lib, e, None) {
            Ok(r) => out.vectors = Some(r),
            Err(err) => out.vectors_error = Some(err.to_string()),
        }
    }
    Ok(out)
}

/// Prepares a search: picks the mode and embeds the query text.
///
/// - `requested = None`: hybrid when the text model is available, the
///   index has vectors for it and the query embeds, else keyword with a
///   note saying why.
/// - `Some(Keyword)`: no embedding, no note.
/// - `Some(Semantic | Hybrid)`: fails when the text model is unavailable or
///   cannot embed the query; notes when no vectors exist yet (`index
///   rebuild` adds them).
///
/// Returns the note, if any; `q.mode` and `q.query_vector` are set.
pub fn prepare_search(
    lib: &Library,
    model: &TextModel,
    q: &mut SearchQuery,
    requested: Option<SearchMode>,
) -> Result<Option<String>> {
    q.query_vector = None;
    if requested == Some(SearchMode::Keyword) {
        q.mode = SearchMode::Keyword;
        return Ok(None);
    }
    let embedder = match (model, requested) {
        (TextModel::Ready(e), _) => e.as_ref(),
        (TextModel::Unavailable { reason, .. }, None) => {
            q.mode = SearchMode::Keyword;
            return Ok(Some(format!(
                "keyword search only: semantic search is unavailable ({reason})"
            )));
        }
        (TextModel::Unavailable { id, reason }, Some(mode)) => {
            return Err(Error::InvalidInput(format!(
                "{} search needs the text model {id}: {reason}",
                mode_name(mode)
            )));
        }
    };
    let has_vectors = Index::open_readonly(lib.root())?.has_vectors(embedder.model_key())?;
    let mut note = None;
    match requested {
        None if !has_vectors => {
            q.mode = SearchMode::Keyword;
            return Ok(Some(
                "keyword search only: no text vectors yet (run `broscribe index rebuild`)".into(),
            ));
        }
        None => q.mode = SearchMode::Hybrid,
        Some(mode) => {
            q.mode = mode;
            if !has_vectors {
                note = Some(
                    "no text vectors yet (run `broscribe index rebuild`); semantic results are empty"
                        .into(),
                );
            }
        }
    }
    if let Some(text) = q.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let embedded = embedder.embed(&[text.to_owned()]).and_then(|mut v| {
            v.pop()
                .ok_or_else(|| Error::Inference("the text model returned no vector".into()))
        });
        let vector = match (embedded, requested) {
            (Ok(v), _) => v,
            // Auto mode never fails on the model: keyword results still come.
            (Err(e), None) => {
                q.mode = SearchMode::Keyword;
                return Ok(Some(format!(
                    "keyword search only: the text model could not embed the query ({e})"
                )));
            }
            (Err(e), Some(_)) => return Err(e),
        };
        q.query_vector = Some(QueryVector {
            model: embedder.model_key().to_owned(),
            vector,
        });
    }
    Ok(note)
}

fn mode_name(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Keyword => "keyword",
        SearchMode::Semantic => "semantic",
        SearchMode::Hybrid => "hybrid",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use broscribe_core::index::SearchHit;
    use broscribe_core::ops;
    use broscribe_core::schema::{Meeting, Segment, Source, Speaker};
    use broscribe_core::{SpeakerId, WriteTx};
    use broscribe_models::FakeTextEmbedder;
    use broscribe_platform::fakes::{FakeSecretStore, FakeTrustStore};
    use chrono::DateTime;
    use tempfile::TempDir;

    use super::*;

    const MA: &str = "m-0000a1";
    const MB: &str = "m-0000b2";

    fn mid(s: &str) -> MeetingId {
        MeetingId::parse(s).unwrap()
    }

    fn seg(id: u32, speaker: u32, start: f64, text: &str) -> Segment {
        Segment {
            speech_overlap: Vec::new(),
            id,
            track: "t0".into(),
            speaker: Some(SpeakerId::new(speaker).unwrap()),
            start,
            end: start + 5.0,
            text: text.into(),
            words: Vec::new(),
        }
    }

    fn meeting(id: &str, started: &str, tags: &[&str], segments: Vec<Segment>) -> Meeting {
        let mut m = Meeting::new(
            mid(id),
            Source::Import,
            DateTime::parse_from_rfc3339(started).unwrap(),
        );
        m.title = Some(format!("Meeting {id}"));
        m.tags = tags.iter().map(|t| (*t).to_owned()).collect();
        m.speakers = vec![
            Speaker::new(SpeakerId::new(1).unwrap(), "t0"),
            Speaker::new(SpeakerId::new(2).unwrap(), "t0"),
        ];
        m.segments = segments;
        m
    }

    /// Two meetings, indexed; no vectors yet.
    fn setup() -> (TempDir, Library) {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let ma = meeting(
            MA,
            "2026-11-10T10:00:00Z",
            &["finance"],
            vec![
                seg(0, 1, 0.0, "We approved the marketing budget."),
                seg(1, 1, 5.0, "It covers the spring campaign."),
                seg(2, 2, 10.0, "Who organises the holiday party?"),
                seg(3, 1, 15.0, "I can book the venue."),
            ],
        );
        let mb = meeting(
            MB,
            "2026-11-12T10:00:00Z",
            &["sync"],
            vec![
                seg(0, 1, 0.0, "The marketing budget needs another review."),
                seg(1, 2, 5.0, "Deploy the release on Friday."),
            ],
        );
        lib.write(|tx| {
            for m in [&ma, &mb] {
                tx.create_meeting_dir(m)?;
                tx.save_meeting(m)?;
            }
            Index::rebuild(tx)
        })
        .unwrap();
        (tmp, lib)
    }

    fn query(text: &str, mode: SearchMode, e: &FakeTextEmbedder) -> SearchQuery {
        SearchQuery {
            text: Some(text.into()),
            mode,
            query_vector: Some(QueryVector {
                model: e.model_key().into(),
                vector: e.vector(text),
            }),
            ..SearchQuery::default()
        }
    }

    fn search(lib: &Library, q: &SearchQuery) -> Vec<SearchHit> {
        Index::open_readonly(lib.root()).unwrap().search(q).unwrap()
    }

    fn keys(hits: &[SearchHit]) -> Vec<(String, u32)> {
        hits.iter()
            .map(|h| (h.meeting_id.to_string(), h.segment_id))
            .collect()
    }

    #[test]
    fn embeds_each_window_text_once_and_naming_costs_nothing() {
        let (_tmp, lib) = setup();
        let fake = FakeTextEmbedder::new("fake-text", 256);
        let report = embed_missing(&lib, &fake, None).unwrap();
        // Windows: A = [0,1], [2], [3]; B = [0], [1].
        assert_eq!((report.pending, report.embedded, report.stale), (5, 5, 0));
        assert_eq!(fake.calls(), 1);

        // Naming a speaker and renaming its person: zero embedding calls.
        ops::with_index(&lib, |tx, idx| {
            ops::name_speaker(
                tx,
                idx,
                &mid(MA),
                SpeakerId::new(1).unwrap(),
                "Ana Example",
                Vec::new(),
                "nemo-titanet-large",
            )
        })
        .unwrap();
        let report = embed_missing(&lib, &fake, None).unwrap();
        assert_eq!((report.pending, report.embedded), (0, 0));
        assert_eq!(fake.calls(), 1);

        // The named person now filters the semantic leg too.
        let mut q = query("marketing budget plans", SearchMode::Semantic, &fake);
        q.person = Some("ana example".into());
        let hits = search(&lib, &q);
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.meeting_id == mid(MA)));
        assert_eq!(hits[0].speaker_name, "Ana Example");

        // Changing text embeds only the changed window.
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&mid(MB))?;
            m.segments[1].text = "Deploy the release on Monday.".into();
            tx.save_meeting(&m)?;
            Index::open(tx.root())?.upsert_meeting(tx, &m)
        })
        .unwrap();
        let report = embed_missing(&lib, &fake, Some(&mid(MB))).unwrap();
        assert_eq!((report.pending, report.embedded), (1, 1));
        assert_eq!(fake.texts(), 6);
    }

    #[test]
    fn hybrid_fusion_returns_a_citable_segment() {
        let (_tmp, lib) = setup();
        let fake = FakeTextEmbedder::new("fake-text", 256);
        embed_missing(&lib, &fake, None).unwrap();

        // "plans" appears nowhere: the keyword leg finds nothing, the
        // semantic leg still finds the budget windows.
        let keyword = SearchQuery {
            text: Some("marketing budget plans".into()),
            ..SearchQuery::default()
        };
        assert!(search(&lib, &keyword).is_empty());
        let hits = search(
            &lib,
            &query("marketing budget plans", SearchMode::Hybrid, &fake),
        );
        assert!(hits.len() >= 2);
        let top: Vec<(String, u32)> = keys(&hits[..2]);
        assert!(top.contains(&(MA.into(), 0)), "{top:?}");
        assert!(top.contains(&(MB.into(), 0)), "{top:?}");
        // A semantic-only window is cited by its longest segment, whole.
        let a = hits.iter().find(|h| h.meeting_id == mid(MA)).unwrap();
        assert_eq!((a.start, a.end), (0.0, 5.0));
        assert_eq!(a.snippet, "We approved the marketing budget.");
        assert!(a.score.unwrap() > 0.0);

        // A keyword hit inside a window is the cited segment, with a snippet;
        // windows rank once each.
        let hits = search(&lib, &query("spring campaign", SearchMode::Hybrid, &fake));
        assert_eq!(keys(&hits[..1]), [(MA.to_owned(), 1)]);
        assert!(hits[0].snippet.contains("**spring**"));
        let windows: Vec<(String, u32)> = keys(&hits);
        assert_eq!(
            windows.iter().filter(|k| k.0 == MA && k.1 <= 1).count(),
            1,
            "{windows:?}"
        );
    }

    #[test]
    fn filters_apply_to_both_legs() {
        let (_tmp, lib) = setup();
        let fake = FakeTextEmbedder::new("fake-text", 256);
        embed_missing(&lib, &fake, None).unwrap();
        for mode in [SearchMode::Semantic, SearchMode::Hybrid] {
            let mut q = query("marketing budget", mode, &fake);
            q.tag = Some("SYNC".into());
            let hits = search(&lib, &q);
            assert!(!hits.is_empty(), "{mode:?}");
            assert!(hits.iter().all(|h| h.meeting_id == mid(MB)), "{mode:?}");

            let mut q = query("marketing budget", mode, &fake);
            q.since = Some(DateTime::parse_from_rfc3339("2026-11-11T00:00:00Z").unwrap());
            assert!(search(&lib, &q).iter().all(|h| h.meeting_id == mid(MB)));

            let mut q = query("marketing budget", mode, &fake);
            q.meeting = Some(mid(MA));
            assert!(search(&lib, &q).iter().all(|h| h.meeting_id == mid(MA)));

            let mut q = query("marketing budget", mode, &fake);
            q.source = Some(Source::Recording);
            assert!(search(&lib, &q).is_empty(), "{mode:?}");
        }
    }

    #[test]
    fn rebuild_prunes_unused_vectors_and_refills() {
        let (_tmp, lib) = setup();
        let fake = FakeTextEmbedder::new("fake-text", 256);
        embed_missing(&lib, &fake, None).unwrap();
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&mid(MB))?;
            m.segments[1].text = "Ship it next week.".into();
            tx.save_meeting(&m)
        })
        .unwrap();

        let out = rebuild_index(&lib, Some(&fake)).unwrap();
        assert_eq!(out.index.meetings, 2);
        assert_eq!(out.index.windows, 5);
        assert_eq!(out.index.vectors_pruned, 1);
        let v = out.vectors.unwrap();
        assert_eq!((v.pending, v.embedded), (1, 1));
        assert_eq!(fake.calls(), 2);
        assert_eq!(
            VectorCache::open(lib.root()).unwrap().count(None).unwrap(),
            5
        );

        // Deleting `.index/` only costs recomputing: keyword search works
        // after a rebuild without any text model.
        std::fs::remove_dir_all(lib.root().join(broscribe_core::paths::INDEX_DIR)).unwrap();
        let out = rebuild_index(&lib, None).unwrap();
        assert!(out.vectors.is_none());
        let idx = Index::open_readonly(lib.root()).unwrap();
        assert!(!idx.vectors_attached());
        let hits = idx
            .search(&SearchQuery {
                text: Some("venue".into()),
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(keys(&hits), [(MA.to_owned(), 3)]);
    }

    /// Changes meeting B's text from inside `embed` (which therefore runs
    /// outside the lock); the batch's vectors for B are then stale.
    struct EditingEmbedder<'a> {
        lib: &'a Library,
        inner: FakeTextEmbedder,
        edited: AtomicBool,
    }

    impl TextEmbedder for EditingEmbedder<'_> {
        fn model_key(&self) -> &str {
            self.inner.model_key()
        }

        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            if !self.edited.swap(true, Ordering::SeqCst) {
                self.lib.write(|tx: &mut WriteTx<'_>| {
                    let mut m = tx.library().load_meeting(&mid(MB))?;
                    m.segments[0].text = "Budget review moved.".into();
                    tx.save_meeting(&m)?;
                    Index::open(tx.root())?.upsert_meeting(tx, &m)
                })?;
            }
            self.inner.embed(texts)
        }
    }

    #[test]
    fn vectors_of_a_meeting_changed_during_embedding_are_dropped() {
        let (_tmp, lib) = setup();
        let e = EditingEmbedder {
            lib: &lib,
            inner: FakeTextEmbedder::new("fake-text", 256),
            edited: AtomicBool::new(false),
        };
        let report = embed_missing(&lib, &e, None).unwrap();
        assert_eq!((report.pending, report.embedded, report.stale), (5, 3, 2));
        // The next pass embeds meeting B's current windows.
        let report = embed_missing(&lib, &e, None).unwrap();
        assert_eq!((report.pending, report.embedded, report.stale), (2, 2, 0));
    }

    #[test]
    fn disabled_provider_gets_no_requests_and_search_falls_back() {
        let (tmp, lib) = setup();
        let data = tmp.path().join("data");
        let secrets = FakeSecretStore::default();
        let mut config = Config::default();
        config.profile.text_embedding = "openai-embeddings".into();
        config.providers.insert(
            "openai-embeddings".into(),
            serde_json::from_value(serde_json::json!({
                "enabled": true,
                "model": "text-embedding-3-small",
                "base_url": "https://api.example.com/v1",
            }))
            .unwrap(),
        );
        let trust = Arc::new(FakeTrustStore::default());
        let execution = crate::testutil::execution_trusting(trust.clone());
        let model = text_model(&config, &data, &secrets, &execution).unwrap();
        assert!(matches!(model, TextModel::Unavailable { .. }));

        let mut q = SearchQuery {
            text: Some("budget".into()),
            ..SearchQuery::default()
        };
        let note = prepare_search(&lib, &model, &mut q, None).unwrap();
        assert!(note.unwrap().contains("not enabled"));
        assert_eq!(q.mode, SearchMode::Keyword);
        assert!(q.query_vector.is_none());
        let err = prepare_search(&lib, &model, &mut q, Some(SearchMode::Semantic)).unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        assert_eq!(search(&lib, &q).len(), 2);

        // The default local model, not installed: keyword with a note.
        let model = text_model(&Config::default(), &data, &secrets, &execution).unwrap();
        let TextModel::Unavailable { id, .. } = &model else {
            panic!("model is not installed");
        };
        assert_eq!(id, "potion-multilingual-128m");
        assert_eq!(
            trust.reads.load(Ordering::SeqCst),
            0,
            "a disabled provider and a local model never reach HTTP"
        );
    }

    #[test]
    fn prepare_search_defaults_to_hybrid_once_vectors_exist() {
        let (_tmp, lib) = setup();
        let model = TextModel::Ready(Box::new(FakeTextEmbedder::new("fake-text", 256)));
        let mut q = SearchQuery {
            text: Some("budget".into()),
            ..SearchQuery::default()
        };
        let note = prepare_search(&lib, &model, &mut q, None).unwrap();
        assert!(note.unwrap().contains("no text vectors"));
        assert_eq!(q.mode, SearchMode::Keyword);

        embed_missing(&lib, model.embedder().unwrap(), None).unwrap();
        let note = prepare_search(&lib, &model, &mut q, None).unwrap();
        assert_eq!(note, None);
        assert_eq!(q.mode, SearchMode::Hybrid);
        assert_eq!(q.query_vector.as_ref().unwrap().model, "fake-text");
        assert!(!search(&lib, &q).is_empty());
    }

    #[test]
    fn auto_search_falls_back_to_keywords_when_the_query_cannot_be_embedded() {
        struct Down;
        impl TextEmbedder for Down {
            fn model_key(&self) -> &str {
                "fake-text"
            }
            fn embed(&self, _: &[String]) -> Result<Vec<Vec<f32>>> {
                Err(Error::Inference("the embedding server is down".into()))
            }
        }
        let (_tmp, lib) = setup();
        embed_missing(&lib, &FakeTextEmbedder::new("fake-text", 256), None).unwrap();
        let model = TextModel::Ready(Box::new(Down));
        let mut q = SearchQuery {
            text: Some("budget".into()),
            ..SearchQuery::default()
        };
        let note = prepare_search(&lib, &model, &mut q, None).unwrap();
        assert!(note.unwrap().contains("could not embed"));
        assert_eq!(q.mode, SearchMode::Keyword);
        assert!(q.query_vector.is_none());
        assert_eq!(search(&lib, &q).len(), 2);
        let err = prepare_search(&lib, &model, &mut q, Some(SearchMode::Hybrid)).unwrap_err();
        assert_eq!(err.code(), "inference");
    }

    #[test]
    fn an_unreadable_api_key_makes_the_provider_unavailable() {
        struct Locked;
        impl SecretStore for Locked {
            fn get(&self, _: &str) -> Result<Option<String>> {
                Err(Error::Internal("keychain locked".into()))
            }
            fn set(&self, _: &str, _: &str) -> Result<()> {
                Ok(())
            }
            fn delete(&self, _: &str) -> Result<()> {
                Ok(())
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.profile.text_embedding = "openai-embeddings".into();
        config.providers.insert(
            "openai-embeddings".into(),
            serde_json::from_value(serde_json::json!({
                "enabled": true,
                "consented_at": "2026-11-14T10:00:00Z",
                "model": "text-embedding-3-small",
                "base_url": "https://api.example.com/v1",
            }))
            .unwrap(),
        );
        let execution = crate::testutil::execution();
        let model = text_model(&config, tmp.path(), &Locked, &execution).unwrap();
        let TextModel::Unavailable { reason, .. } = &model else {
            panic!("the key cannot be read");
        };
        assert!(reason.contains("API key"), "{reason}");
    }

    #[test]
    fn remote_provider_without_trust_roots_is_unavailable_and_loopback_still_works() {
        let tmp = tempfile::tempdir().unwrap();
        let secrets = FakeSecretStore::default();
        let trust = Arc::new(FakeTrustStore::default());
        let execution = crate::testutil::execution_trusting(trust.clone());
        let provider = |base_url: &str| {
            let mut config = Config::default();
            config.profile.text_embedding = "openai-embeddings".into();
            config.providers.insert(
                "openai-embeddings".into(),
                serde_json::from_value(serde_json::json!({
                    "enabled": true,
                    "consented_at": "2026-11-14T10:00:00Z",
                    "model": "text-embedding-3-small",
                    "base_url": base_url,
                }))
                .unwrap(),
            );
            config
        };
        let config = provider("https://api.example.com/v1");
        let model = text_model(&config, tmp.path(), &secrets, &execution).unwrap();
        let TextModel::Unavailable { reason, .. } = &model else {
            panic!("HTTPS needs the OS trust roots");
        };
        assert!(reason.contains("trust store"), "{reason}");
        assert_eq!(trust.reads.load(Ordering::SeqCst), 1);

        // Never bundled roots instead; a loopback server needs none.
        let config = provider("http://127.0.0.1:9/v1");
        let model = text_model(&config, tmp.path(), &secrets, &execution).unwrap();
        let embedder = model.embedder().expect("loopback works without roots");
        assert!(embedder.model_key().ends_with("@http://127.0.0.1:9"));
        assert_eq!(trust.reads.load(Ordering::SeqCst), 1, "one trust choice");
    }

    #[test]
    fn completed_work_embeds_its_meetings_or_the_whole_library() {
        let fake = FakeTextEmbedder::new("fake-text", 256);
        let (_tmp, lib) = setup();
        let summary = crate::worker::WorkerSummary {
            imported: vec![mid(MA)],
            ..Default::default()
        };
        embed_completed(&lib, &fake, &summary).unwrap();
        // Meeting A's windows only: B's two are still missing.
        assert_eq!(embed_missing(&lib, &fake, None).unwrap().pending, 2);

        let (_tmp, lib) = setup();
        let summary = crate::worker::WorkerSummary {
            batches_resolved: true,
            ..Default::default()
        };
        embed_completed(&lib, &fake, &summary).unwrap();
        assert_eq!(embed_missing(&lib, &fake, None).unwrap().pending, 0);
        // Nothing completed: nothing embedded.
        let (_tmp, lib) = setup();
        embed_completed(&lib, &fake, &crate::worker::WorkerSummary::default()).unwrap();
        assert_eq!(embed_missing(&lib, &fake, None).unwrap().pending, 5);
    }
}
