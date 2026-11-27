//! Text embeddings (`text_embedding` role, plan M2 §3, §9.1–§9.2).
//!
//! - [`Model2VecEmbedder`]: local static embeddings with `model2vec-rs`, loaded
//!   only from the pinned files of an installed registry model (no hub or
//!   network access) and only on first use.
//! - [`HttpEmbedder`]: an `openai-compatible-http` provider's `/v1/embeddings`;
//!   refused unless the provider is enabled with recorded consent.
//! - [`FakeTextEmbedder`]: deterministic, counting fake for tests.
//!
//! Vectors are keyed in the vector cache by [`TextEmbedder::model_key`].
//! Texts are never logged.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use broscribe_core::config::{ProviderConfig, validate_model_name};
use broscribe_core::{Error, Result};
use model2vec_rs::model::StaticModel;
use serde::Deserialize;
use serde_json::json;

use crate::adapters::l2_normalize;
use crate::execution::ExecutionContext;
use crate::llm::http::{endpoint_url, normalized_origin};
use crate::llm::registry::{ProviderEntry, ProviderPurpose, ProviderRuntime};
use crate::registry::{ModelEntry, Role};
use crate::store::{Health, ModelStore};

/// Registry runtime of local static-embedding models.
pub const MODEL2VEC_RUNTIME: &str = "model2vec";
/// The files a model2vec model needs, by their URL's last segment.
pub const MODEL2VEC_FILES: [&str; 3] = ["model.safetensors", "tokenizer.json", "config.json"];
/// Token limit per text when the registry entry sets no `max_length`.
const DEFAULT_MAX_LENGTH: usize = 512;
/// Texts per model2vec batch and per HTTP request.
const BATCH: usize = 64;
/// Endpoint appended to an HTTP provider's base URL (which includes `/v1`).
pub const EMBEDDINGS_ENDPOINT: &str = "embeddings";
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);

/// A text-embedding model.
pub trait TextEmbedder: Send + Sync {
    /// Vector-cache key: the registry model ID, or `<provider>:<model>` for a
    /// remote provider. Vectors are only compared within one key.
    fn model_key(&self) -> &str;

    /// One L2-normalised vector per text, in order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Checks a model's output: one non-empty, finite vector per text, all of
/// one dimension; normalises them.
fn finish(key: &str, n: usize, mut vectors: Vec<Vec<f32>>) -> Result<Vec<Vec<f32>>> {
    if vectors.len() != n {
        return Err(Error::Inference(format!(
            "text model {key} returned {} vectors for {n} texts",
            vectors.len()
        )));
    }
    let dim = vectors.first().map_or(0, Vec::len);
    for v in &mut vectors {
        if v.is_empty() || v.len() != dim || v.iter().any(|x| !x.is_finite()) {
            return Err(Error::Inference(format!(
                "text model {key} returned an empty, mixed-size or non-finite vector"
            )));
        }
        l2_normalize(v);
    }
    Ok(vectors)
}

/// `cell`'s value, created by `load` at most once at a time: concurrent first
/// callers wait for one load instead of each running it. A failed load
/// leaves the cell empty, so a later call tries again.
fn load_once<'a, T>(
    cell: &'a OnceLock<T>,
    gate: &Mutex<()>,
    load: impl FnOnce() -> Result<T>,
) -> Result<&'a T> {
    if let Some(v) = cell.get() {
        return Ok(v);
    }
    let _loading = gate.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(v) = cell.get() {
        return Ok(v);
    }
    let v = load()?;
    Ok(cell.get_or_init(|| v))
}

/// Local model2vec static embeddings.
pub struct Model2VecEmbedder {
    id: String,
    dir: PathBuf,
    max_length: usize,
    model: OnceLock<StaticModel>,
    /// Serializes the first load (see [`load_once`]).
    loading: Mutex<()>,
}

impl Model2VecEmbedder {
    /// Checks that `entry` is an installed model2vec text-embedding model with
    /// the three pinned files; the files are read on the first [`embed`].
    ///
    /// [`embed`]: TextEmbedder::embed
    pub fn new(entry: &ModelEntry, store: &ModelStore) -> Result<Self> {
        if entry.runtime != MODEL2VEC_RUNTIME || !entry.has_role(Role::TextEmbedding) {
            return Err(Error::InvalidInput(format!(
                "model {} is not a {MODEL2VEC_RUNTIME} text-embedding model",
                entry.id
            )));
        }
        for name in MODEL2VEC_FILES {
            if !entry.files.iter().any(|f| f.file_name() == name) {
                return Err(Error::InvalidInput(format!(
                    "model {} does not list {name}",
                    entry.id
                )));
            }
        }
        match store.doctor(entry, false)? {
            Health::Installed => {}
            Health::Missing => {
                return Err(Error::ModelMissing(format!(
                    "{id} is not installed (run `broscribe models pull {id}`)",
                    id = entry.id
                )));
            }
            Health::Corrupt { reason } => {
                return Err(Error::ModelMissing(format!(
                    "{id} needs reinstalling ({reason}); run `broscribe models pull {id}`",
                    id = entry.id
                )));
            }
        }
        let max_length = match entry.param_i64("max_length")? {
            None => DEFAULT_MAX_LENGTH,
            Some(n) => usize::try_from(n).ok().filter(|n| *n > 0).ok_or_else(|| {
                Error::InvalidInput(format!(
                    "model {}: parameter `max_length` must be positive",
                    entry.id
                ))
            })?,
        };
        Ok(Self {
            id: entry.id.clone(),
            dir: store.content_dir(entry)?,
            max_length,
            model: OnceLock::new(),
            loading: Mutex::new(()),
        })
    }

    /// Loads the model from its pinned local files (once, even when several
    /// threads embed at the same time).
    fn model(&self) -> Result<&StaticModel> {
        load_once(&self.model, &self.loading, || {
            let read = |name: &str| {
                let path = self.dir.join(name);
                std::fs::read(&path).map_err(|e| Error::io_at(&path, e))
            };
            let tokenizer = read("tokenizer.json")?;
            let weights = read("model.safetensors")?;
            let config = read("config.json")?;
            StaticModel::from_bytes(tokenizer, weights, config, None)
                .map_err(|e| Error::Inference(format!("loading {}: {e:#}", self.id)))
        })
    }
}

impl TextEmbedder for Model2VecEmbedder {
    fn model_key(&self) -> &str {
        &self.id
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let model = self.model()?;
        let vectors = model.encode_with_args(texts, Some(self.max_length), BATCH);
        finish(&self.id, texts.len(), vectors)
    }
}

/// `/v1/embeddings` of an `openai-compatible-http` provider.
pub struct HttpEmbedder {
    provider: String,
    key: String,
    url: String,
    model: String,
    api_key: Option<String>,
    /// The context's shared agent (connections and TLS configuration).
    agent: ureq::Agent,
    /// The vector size of everything this embedder returned: later replies
    /// of another size are refused, never mixed in.
    dim: OnceLock<usize>,
}

impl std::fmt::Debug for HttpEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpEmbedder")
            .field("provider", &self.provider)
            .field("url", &self.url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl HttpEmbedder {
    /// Refuses unless the provider is an HTTP provider, enabled with recorded
    /// consent, with `base_url` and a valid `model` configured. `api_key`
    /// comes from the platform `SecretStore` (`llm::http::api_key_secret`).
    /// The vector-cache key names the provider, the model and the server's
    /// normalised origin: vectors from another server are never mixed in. An
    /// `https` server needs the context's OS trust roots
    /// (`trust_store_unavailable` otherwise, before connecting).
    pub fn new(
        entry: &ProviderEntry,
        config: &ProviderConfig,
        api_key: Option<String>,
        execution: &ExecutionContext,
    ) -> Result<Self> {
        if !config.is_consented() {
            return Err(Error::InvalidInput(format!(
                "provider {} is not enabled; enable it and accept its consent text first",
                entry.id
            )));
        }
        if entry.runtime != ProviderRuntime::OpenaiCompatibleHttp
            || entry.purpose != ProviderPurpose::TextEmbedding
        {
            return Err(Error::InvalidInput(format!(
                "provider {} is not an HTTP text-embedding provider",
                entry.id
            )));
        }
        let base = config
            .base_url
            .as_deref()
            .filter(|b| !b.trim().is_empty())
            .ok_or_else(|| Error::InvalidInput(format!("provider {} needs base_url", entry.id)))?;
        let model = config
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .ok_or_else(|| Error::InvalidInput(format!("provider {} needs model", entry.id)))?;
        validate_model_name(&model)?;
        let origin = normalized_origin(base)?;
        let url = endpoint_url(base, EMBEDDINGS_ENDPOINT)?;
        let agent = execution.http_agent(&url)?.clone();
        Ok(Self {
            provider: entry.id.clone(),
            key: format!("{}:{model}@{origin}", entry.id),
            url,
            model,
            api_key: api_key.filter(|k| !k.is_empty()),
            agent,
            dim: OnceLock::new(),
        })
    }

    /// One request's vectors, and the reply's top-level `model` when it is a
    /// valid model name (anything else there is ignored, as before).
    fn post(&self, texts: &[String]) -> Result<(Vec<Vec<f32>>, Option<String>)> {
        #[derive(Deserialize)]
        struct Reply {
            data: Vec<Item>,
            #[serde(default)]
            model: Option<serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct Item {
            #[serde(default)]
            index: Option<usize>,
            embedding: Vec<f32>,
        }

        let body = json!({ "model": self.model, "input": texts, "encoding_format": "float" });
        let body = serde_json::to_string(&body)
            .map_err(|e| Error::Internal(format!("serialising the request: {e}")))?;
        let mut req = self
            .agent
            .post(&self.url)
            .config()
            .timeout_global(Some(HTTP_TIMEOUT))
            .build()
            .header("Content-Type", "application/json");
        if let Some(key) = &self.api_key {
            req = req.header("Authorization", &format!("Bearer {key}"));
        }
        // Errors name the provider only: ureq messages may carry the URL.
        let fail = |what: &str| Error::Inference(format!("{}: {what}", self.provider));
        let mut resp = req
            .send(body)
            .map_err(|_| fail("embedding request failed"))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(fail(&format!("embedding request returned HTTP {status}")));
        }
        let text = resp
            .body_mut()
            .with_config()
            .limit(64 << 20)
            .read_to_string()
            .map_err(|_| fail("reading the embedding reply failed"))?;
        let reply: Reply =
            serde_json::from_str(&text).map_err(|_| fail("the embedding reply is not valid"))?;
        let model = reply
            .model
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .filter(|m| validate_model_name(m).is_ok())
            .map(str::to_owned);
        let mut items = reply.data;
        // Indexes on every item (a permutation of the inputs) or on none
        // (reply order); a reply mixing both cannot be placed.
        let indexed = items.iter().filter(|i| i.index.is_some()).count();
        if indexed == items.len() {
            items.sort_by_key(|i| i.index);
            if items.iter().enumerate().any(|(n, i)| i.index != Some(n)) {
                return Err(fail("the embedding reply has missing or repeated indexes"));
            }
        } else if indexed > 0 {
            return Err(fail("the embedding reply indexes only some of its vectors"));
        }
        Ok((items.into_iter().map(|i| i.embedding).collect(), model))
    }

    /// [`TextEmbedder::embed`], also returning the model the server says
    /// answered: the replies' top-level `model` when every batch names the
    /// same valid model name, else `None` (the request always names the
    /// configured model; a server may answer with another, as an alias
    /// resolving to a version). The vectors and their checks are
    /// [`TextEmbedder::embed`]'s.
    pub fn embed_reporting_model(
        &self,
        texts: &[String],
    ) -> Result<(Vec<Vec<f32>>, Option<String>)> {
        let mut out = Vec::with_capacity(texts.len());
        // `None` until a batch answers; then the model all batches named.
        let mut reported: Option<Option<String>> = None;
        for batch in texts.chunks(BATCH) {
            let (vectors, model) = self.post(batch)?;
            let vectors = finish(&self.key, batch.len(), vectors)?;
            let dim = vectors.first().map_or(0, Vec::len);
            let expected = *self.dim.get_or_init(|| dim);
            if dim != expected {
                return Err(Error::Inference(format!(
                    "text model {} returned {dim}-dimensional vectors after {expected}-dimensional ones",
                    self.key
                )));
            }
            out.extend(vectors);
            reported = Some(match reported {
                None => model,
                Some(earlier) => earlier.filter(|e| model.as_deref() == Some(e.as_str())),
            });
        }
        Ok((out, reported.flatten()))
    }
}

impl TextEmbedder for HttpEmbedder {
    fn model_key(&self) -> &str {
        &self.key
    }

    /// Every batch is checked before anything is returned: one failed or
    /// mismatched batch fails the whole call, so no caller stores part of
    /// a reply or vectors of two sizes under one key.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_reporting_model(texts)
            .map(|(vectors, _)| vectors)
    }
}

/// Deterministic bag-of-words embedder for tests: each lowercase
/// alphanumeric word adds 1 to a hashed dimension, so texts sharing words
/// are similar. Counts its calls and embedded texts.
#[derive(Debug)]
pub struct FakeTextEmbedder {
    key: String,
    dim: usize,
    calls: AtomicUsize,
    texts: AtomicUsize,
}

impl FakeTextEmbedder {
    pub fn new(key: &str, dim: usize) -> Self {
        Self {
            key: key.into(),
            dim: dim.max(1),
            calls: AtomicUsize::new(0),
            texts: AtomicUsize::new(0),
        }
    }

    /// Number of [`TextEmbedder::embed`] calls.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Number of texts embedded over all calls.
    pub fn texts(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }

    /// The (unnormalised) vector of one text.
    pub fn vector(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0; self.dim];
        for word in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            // FNV-1a: stable across runs and platforms.
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in word.to_lowercase().bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            v[(h % self.dim as u64) as usize] += 1.0;
        }
        if v.iter().all(|x| *x == 0.0) {
            v[0] = 1.0;
        }
        v
    }
}

impl TextEmbedder for FakeTextEmbedder {
    fn model_key(&self) -> &str {
        &self.key
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.texts.fetch_add(texts.len(), Ordering::SeqCst);
        finish(
            &self.key,
            texts.len(),
            texts.iter().map(|t| self.vector(t)).collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Registry;

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn fake_is_deterministic_normalised_and_counts() {
        let f = FakeTextEmbedder::new("fake", 64);
        let texts = vec![
            "Budget approved".to_owned(),
            "the budget".to_owned(),
            "zebra crossing".to_owned(),
        ];
        let v = f.embed(&texts).unwrap();
        assert_eq!((f.calls(), f.texts()), (1, 3));
        for x in &v {
            assert!((dot(x, x) - 1.0).abs() < 1e-5);
        }
        assert!(dot(&v[0], &v[1]) > dot(&v[0], &v[2]));
        assert_eq!(f.embed(&texts).unwrap(), v);
    }

    #[test]
    fn registry_lists_the_pinned_model2vec_files() {
        let r = Registry::builtin().unwrap();
        let m = r.get("potion-multilingual-128m").unwrap();
        assert_eq!(m.runtime, MODEL2VEC_RUNTIME);
        assert_eq!(m.roles, [Role::TextEmbedding]);
        let names: Vec<&str> = m.files.iter().map(|f| f.file_name()).collect();
        assert_eq!(names, MODEL2VEC_FILES);
        for f in &m.files {
            assert!(f.url.starts_with(
                "https://huggingface.co/minishlab/potion-multilingual-128M/resolve/73908c3438cf03b6a01bcb9611d62b23d0726f08/"
            ));
            assert!(!f.archive);
        }
    }

    #[test]
    fn model2vec_refuses_a_missing_install() {
        let dir = tempfile::tempdir().unwrap();
        let r = Registry::builtin().unwrap();
        let m = r.get("potion-multilingual-128m").unwrap();
        let err = Model2VecEmbedder::new(m, &ModelStore::new(dir.path()))
            .err()
            .unwrap();
        assert_eq!(err.code(), "model_missing");
        let vad = r.get("silero-vad").unwrap();
        let err = Model2VecEmbedder::new(vad, &ModelStore::new(dir.path()))
            .err()
            .unwrap();
        assert_eq!(err.code(), "invalid_input");
    }

    fn http_entry() -> ProviderEntry {
        crate::llm::registry::ProviderRegistry::builtin()
            .unwrap()
            .get("openai-embeddings")
            .unwrap()
            .clone()
    }

    /// A context whose (synthetic) OS store has one root: HTTPS allowed.
    fn system_ctx() -> ExecutionContext {
        let trust = std::sync::Arc::new(broscribe_platform::fakes::FakeTrustStore::default());
        *trust.policy.lock().unwrap() = Some(broscribe_platform::TlsTrust::Certificates(vec![
            vec![0x30, 0x03, 0x02, 0x01, 0x01],
        ]));
        ExecutionContext::new(
            std::sync::Arc::new(broscribe_platform::fakes::FakeProcessTreeControl::default()),
            trust,
        )
    }

    #[test]
    fn http_embedder_requires_consent_and_config() {
        let entry = http_entry();
        let x = system_ctx();
        let consented: ProviderConfig = serde_json::from_value(json!({
            "enabled": true,
            "consented_at": "2026-11-14T10:00:00Z",
            "model": "text-embedding-3-small",
            "base_url": "https://api.example.com/v1",
        }))
        .unwrap();
        let mut config = ProviderConfig {
            consented_at: None,
            ..consented.clone()
        };
        let err = HttpEmbedder::new(&entry, &config, None, &x).unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        assert!(err.to_string().contains("not enabled"));

        config.consented_at = consented.consented_at;
        config.enabled = false;
        assert!(HttpEmbedder::new(&entry, &config, None, &x).is_err());

        config.enabled = true;
        let e = HttpEmbedder::new(&entry, &config, Some("secret".into()), &x).unwrap();
        assert_eq!(
            e.model_key(),
            "openai-embeddings:text-embedding-3-small@https://api.example.com"
        );
        assert_eq!(e.url, "https://api.example.com/v1/embeddings");
        assert!(!format!("{e:?}").contains("secret"));
        let chat = crate::llm::registry::ProviderRegistry::builtin()
            .unwrap()
            .get("openai-http")
            .unwrap()
            .clone();
        assert!(HttpEmbedder::new(&chat, &config, None, &x).is_err());

        // Without OS roots, HTTPS is refused before connecting; a local
        // server still works.
        let (_, unavailable) = crate::execution::test_context();
        let err = HttpEmbedder::new(&entry, &config, None, &unavailable).unwrap_err();
        assert_eq!(err.code(), crate::TRUST_STORE_UNAVAILABLE, "{err}");
        config.base_url = Some("http://localhost:11434/v1".into());
        assert!(HttpEmbedder::new(&entry, &config, None, &unavailable).is_ok());

        config.base_url = Some("http://remote.example.com/v1".into());
        assert!(HttpEmbedder::new(&entry, &config, None, &x).is_err());
        config.base_url = None;
        assert!(HttpEmbedder::new(&entry, &config, None, &x).is_err());
    }

    /// Real model: `BROSCRIBE_TEXT_MODEL_DIR` holds `model.safetensors`,
    /// `tokenizer.json` and `config.json` of potion-multilingual-128M.
    #[test]
    #[ignore = "needs the downloaded potion-multilingual-128M files"]
    fn real_model2vec_embeds_multilingual_text() {
        let dir = PathBuf::from(std::env::var("BROSCRIBE_TEXT_MODEL_DIR").unwrap());
        let e = Model2VecEmbedder {
            id: "potion-multilingual-128m".into(),
            dir,
            max_length: DEFAULT_MAX_LENGTH,
            model: OnceLock::new(),
            loading: Mutex::new(()),
        };
        let texts = vec![
            "The budget was approved.".to_owned(),
            "O orçamento foi aprovado.".to_owned(),
            "My cat sleeps on the sofa.".to_owned(),
        ];
        let v = e.embed(&texts).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].len(), 256);
        assert!(dot(&v[0], &v[1]) > dot(&v[0], &v[2]));
    }

    fn embeddings_config(base: &str, model: &str) -> ProviderConfig {
        serde_json::from_value(json!({
            "enabled": true,
            "consented_at": "2026-11-14T10:00:00Z",
            "model": model,
            "base_url": base,
        }))
        .unwrap()
    }

    #[test]
    fn http_cache_key_names_the_server_origin() {
        let entry = http_entry();
        let x = system_ctx();
        let key = |base: &str| {
            HttpEmbedder::new(&entry, &embeddings_config(base, "m-1"), None, &x)
                .unwrap()
                .model_key()
                .to_owned()
        };
        assert_eq!(
            key("https://API.example.com:443/v1"),
            key("https://api.example.com/v2/")
        );
        assert_ne!(
            key("https://api.example.com/v1"),
            key("https://other.example.com/v1")
        );
        assert_ne!(
            key("http://localhost:11434/v1"),
            key("http://localhost:8080/v1")
        );
        for bad in ["-m", "two words", "a\nb"] {
            let err = HttpEmbedder::new(
                &entry,
                &embeddings_config("https://api.example.com/v1", bad),
                None,
                &x,
            )
            .unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{bad:?}");
        }
    }

    /// A loopback server answering one request per reply, in order.
    fn serve_replies(replies: Vec<String>) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for body in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut request = vec![0u8; len];
                reader.read_exact(&mut request).unwrap();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    fn vectors_reply(n: usize, dim: usize, indexed: impl Fn(usize) -> bool) -> String {
        let data: Vec<serde_json::Value> = (0..n)
            .map(|i| {
                let embedding: Vec<f32> = (0..dim).map(|d| (i + d + 1) as f32).collect();
                if indexed(i) {
                    json!({"index": i, "embedding": embedding})
                } else {
                    json!({"embedding": embedding})
                }
            })
            .collect();
        json!({ "data": data }).to_string()
    }

    fn texts(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("synthetic text {i}")).collect()
    }

    #[test]
    fn http_reply_indexes_all_or_none() {
        let entry = http_entry();
        for (reply, ok) in [
            (vectors_reply(2, 3, |_| true), true),
            (vectors_reply(2, 3, |_| false), true),
            (vectors_reply(2, 3, |i| i == 0), false),
        ] {
            let base = serve_replies(vec![reply]);
            let e = HttpEmbedder::new(&entry, &embeddings_config(&base, "m"), None, &system_ctx())
                .unwrap();
            assert_eq!(e.embed(&texts(2)).is_ok(), ok);
        }
    }

    /// `vectors_reply` naming `model` at the top level, as servers do.
    fn reply_naming(model: serde_json::Value, n: usize) -> String {
        let mut reply: serde_json::Value =
            serde_json::from_str(&vectors_reply(n, 3, |_| true)).unwrap();
        reply["model"] = model;
        reply.to_string()
    }

    /// The server's `model` comes back beside the vectors when it is a
    /// valid name every batch agrees on; anything else is no report, and
    /// never fails the vectors (SET-63).
    #[test]
    fn http_reply_reports_the_model_the_server_named() {
        let entry = http_entry();
        let x = system_ctx();
        let reporting = |replies: Vec<String>, n: usize| {
            let base = serve_replies(replies);
            let e = HttpEmbedder::new(&entry, &embeddings_config(&base, "embed-latest"), None, &x)
                .unwrap();
            let (vectors, model) = e.embed_reporting_model(&texts(n)).unwrap();
            assert_eq!(vectors.len(), n);
            model
        };
        assert_eq!(
            reporting(vec![reply_naming(json!("embed-v2"), 1)], 1).as_deref(),
            Some("embed-v2")
        );
        assert_eq!(reporting(vec![vectors_reply(1, 3, |_| true)], 1), None);
        for unusable in [json!(null), json!(42), json!("two words"), json!("")] {
            assert_eq!(
                reporting(vec![reply_naming(unusable.clone(), 1)], 1),
                None,
                "{unusable}"
            );
        }
        // Two batches: one model when both name it, none when they differ
        // or one names none.
        let two = |first: serde_json::Value, second: Option<serde_json::Value>| {
            let second = match second {
                Some(model) => reply_naming(model, 1),
                None => vectors_reply(1, 3, |_| true),
            };
            reporting(vec![reply_naming(first, BATCH), second], BATCH + 1)
        };
        assert_eq!(
            two(json!("embed-v2"), Some(json!("embed-v2"))).as_deref(),
            Some("embed-v2")
        );
        assert_eq!(two(json!("embed-v2"), Some(json!("embed-v3"))), None);
        assert_eq!(two(json!("embed-v2"), None), None);

        // `embed` returns the same vectors and ignores the report.
        let base = serve_replies(vec![reply_naming(json!("embed-v2"), 1)]);
        let e =
            HttpEmbedder::new(&entry, &embeddings_config(&base, "embed-latest"), None, &x).unwrap();
        assert_eq!(e.embed(&texts(1)).unwrap()[0].len(), 3);
    }

    #[test]
    fn http_vector_sizes_never_mix_across_batches_or_calls() {
        let entry = http_entry();
        // Two batches of one call: the second changes size; nothing is returned.
        let base = serve_replies(vec![
            vectors_reply(BATCH, 3, |_| true),
            vectors_reply(1, 4, |_| true),
        ]);
        let x = system_ctx();
        let e = HttpEmbedder::new(&entry, &embeddings_config(&base, "m"), None, &x).unwrap();
        let err = e.embed(&texts(BATCH + 1)).unwrap_err();
        assert_eq!(err.code(), "inference");

        // Across calls of one embedder: the size of the first reply holds.
        let base = serve_replies(vec![
            vectors_reply(1, 3, |_| true),
            vectors_reply(1, 5, |_| true),
        ]);
        let e = HttpEmbedder::new(&entry, &embeddings_config(&base, "m"), None, &x).unwrap();
        assert_eq!(e.embed(&texts(1)).unwrap()[0].len(), 3);
        assert_eq!(e.embed(&texts(1)).unwrap_err().code(), "inference");
    }

    #[test]
    fn first_model_load_is_single_flight() {
        let cell: OnceLock<usize> = OnceLock::new();
        let gate = Mutex::new(());
        let loads = AtomicUsize::new(0);
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    barrier.wait();
                    let v = load_once(&cell, &gate, || {
                        loads.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(50));
                        Ok(7)
                    })
                    .unwrap();
                    assert_eq!(*v, 7);
                });
            }
        });
        assert_eq!(loads.load(Ordering::SeqCst), 1, "one expensive load");

        // A failed first load is not cached: the next call loads again.
        let cell: OnceLock<usize> = OnceLock::new();
        assert!(load_once(&cell, &gate, || Err(Error::Inference("x".into()))).is_err());
        assert_eq!(*load_once(&cell, &gate, || Ok(9)).unwrap(), 9);
    }
}
