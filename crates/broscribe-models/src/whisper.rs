//! whisper.cpp (through `whisper-rs`) implementation of [`Asr`] (plan §5,
//! §12.12): language per call, hotwords as Whisper's initial prompt (at most
//! [`MAX_PROMPT_TOKENS`] tokens counted with the model's own tokenizer) and
//! token-level timestamps converted to [`AsrToken`]s.

use std::path::Path;
use std::sync::Mutex;

use broscribe_core::{Error, Result};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError,
    WhisperState, WhisperTokenId,
};

use crate::adapters::{Asr, AsrOutput, AsrToken};

/// Registry `runtime` of whisper.cpp models.
pub const RUNTIME: &str = "whisper";
/// Whisper conditions on at most half its 448-token text context.
pub const MAX_PROMPT_TOKENS: usize = 224;
/// whisper.cpp's command-line default.
pub const DEFAULT_BEAM_SIZE: i64 = 5;
/// Joins hotwords in the prompt.
const PROMPT_SEPARATOR: &str = ", ";
/// Input is 16 kHz mono; whisper.cpp times are in centiseconds.
const SAMPLES_PER_CENTISECOND: i64 = 160;

/// A loaded whisper.cpp model with one decoding state, reused (and reset by
/// whisper.cpp) on every call.
pub struct WhisperAsr {
    ctx: WhisperContext,
    state: Mutex<WhisperState>,
    /// The tokenized hotword prompt; empty for none.
    prompt: Vec<WhisperTokenId>,
    threads: i32,
    beam_size: i32,
}

impl WhisperAsr {
    /// Loads the ggml model file. With the `metal` feature whisper.cpp runs on
    /// the GPU. whisper.cpp and ggml logs are discarded: decoder diagnostics
    /// can carry transcript text.
    pub fn load(model_id: &str, model_file: &Path, threads: i32, beam_size: i32) -> Result<Self> {
        whisper_rs::install_logging_hooks();
        let ctx = WhisperContext::new_with_params(model_file, WhisperContextParameters::default())
            .map_err(|e| failed(&format!("could not load model {model_id}"), e))?;
        let state = ctx
            .create_state()
            .map_err(|e| failed(&format!("could not create a state for {model_id}"), e))?;
        Ok(Self {
            ctx,
            state: Mutex::new(state),
            prompt: Vec::new(),
            threads: threads.max(1),
            beam_size: beam_size.max(1),
        })
    }

    /// Number of Whisper tokens in `text`.
    fn count_tokens(&self, text: &str) -> Result<usize> {
        // Every byte-level BPE token covers at least one byte, so `len + 1`
        // slots always suffice (whisper.cpp fails when there are too few).
        self.ctx
            .tokenize(text, text.len() + 1)
            .map(|t| t.len())
            .map_err(|e| failed("could not tokenize the hotword prompt", e))
    }

    fn prompt_limit(&self) -> usize {
        usize::try_from(self.ctx.n_text_ctx() / 2)
            .unwrap_or(0)
            .min(MAX_PROMPT_TOKENS)
    }
}

impl Asr for WhisperAsr {
    fn transcribe(&self, samples_16k: &[f32]) -> Result<AsrOutput> {
        self.transcribe_with(samples_16k, None)
    }

    fn transcribe_with(&self, samples_16k: &[f32], language: Option<&str>) -> Result<AsrOutput> {
        if samples_16k.is_empty() {
            return Ok(AsrOutput::default());
        }
        let language = language.and_then(language_or_detect);
        let mut params = FullParams::new(if self.beam_size > 1 {
            SamplingStrategy::BeamSearch {
                beam_size: self.beam_size,
                patience: -1.0,
            }
        } else {
            SamplingStrategy::Greedy { best_of: 5 }
        });
        params.set_n_threads(self.threads);
        // `None` makes whisper.cpp detect the language.
        params.set_language(language);
        params.set_translate(false);
        // Every call is an independent segment: no text carried over.
        params.set_no_context(true);
        params.set_token_timestamps(true);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        if !self.prompt.is_empty() {
            params.set_tokens(&self.prompt);
        }

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .full(params, samples_16k)
            .map_err(|e| failed("speech recognition failed", e))?;
        // Text tokens precede every special token (end of text, timestamps…).
        let eot = self.ctx.token_eot();
        let mut asm = TokenAssembler::new(samples_16k.len());
        for segment in state.as_iter() {
            for i in 0..segment.n_tokens() {
                let Some(token) = segment.get_token(i) else {
                    continue;
                };
                if token.token_id() >= eot {
                    continue;
                }
                let data = token.token_data();
                let bytes = token
                    .to_bytes()
                    .map_err(|e| failed("returned a token without text", e))?;
                asm.push(bytes, data.t0, data.t1);
            }
        }
        drop(state);
        let tokens = asm.finish();
        let text: String = tokens.iter().map(|t| t.text.as_str()).collect();
        Ok(AsrOutput {
            text: text.trim().to_owned(),
            tokens,
        })
    }

    fn accepts_hotwords(&self) -> bool {
        true
    }

    fn set_hotwords(&mut self, terms: &[String]) -> Result<()> {
        let limit = self.prompt_limit();
        let prompt = fit_prompt(terms, limit, |text| self.count_tokens(text))?;
        self.prompt = if prompt.is_empty() {
            Vec::new()
        } else {
            self.ctx
                .tokenize(&prompt, prompt.len() + 1)
                .map_err(|e| failed("could not tokenize the hotword prompt", e))?
        };
        if self.prompt.len() > limit {
            return Err(Error::Internal(format!(
                "hotword prompt has {} tokens, more than {limit}",
                self.prompt.len()
            )));
        }
        Ok(())
    }
}

fn failed(what: &str, e: WhisperError) -> Error {
    Error::Inference(format!("whisper.cpp {what}: {e}"))
}

/// Whisper's code for a BCP 47 tag (its primary subtag: `pt-BR` → `pt`,
/// with whisper.cpp's spellings of [`LANGUAGE_ALIASES`]); `None` for
/// `und`/`auto` (detect), `Error::Unsupported` for a language Whisper does
/// not know.
pub fn whisper_language(tag: &str) -> Result<Option<&'static str>> {
    let primary = tag
        .split(['-', '_'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if primary.is_empty() || primary == "und" || primary == "auto" {
        return Ok(None);
    }
    let unsupported = || Error::Unsupported(format!("Whisper does not support language {tag:?}"));
    if !primary.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(unsupported());
    }
    let code = LANGUAGE_ALIASES
        .iter()
        .find(|(alias, _)| *alias == primary)
        .map_or(primary.as_str(), |(_, code)| code);
    whisper_rs::get_lang_id(code)
        .and_then(whisper_rs::get_lang_str)
        .map(Some)
        .ok_or_else(unsupported)
}

/// Valid BCP 47 primary subtags whisper.cpp spells differently.
const LANGUAGE_ALIASES: [(&str, &str); 4] =
    [("nb", "no"), ("iw", "he"), ("jv", "jw"), ("fil", "tl")];

/// The language to decode `tag` in; a tag Whisper does not know is detected
/// instead (with a warning) rather than failing the whole transcription.
fn language_or_detect(tag: &str) -> Option<&'static str> {
    whisper_language(tag).unwrap_or_else(|e| {
        tracing::warn!("{e}; detecting the language instead");
        None
    })
}

/// The longest prompt of hotwords that fits `max_tokens`: `terms` in order
/// (earlier ones win), trimmed, deduplicated case-insensitively and joined
/// with `, `; a term that does not fit is skipped and later, shorter ones are
/// still tried. `count` returns the token count of a candidate prompt.
pub(crate) fn fit_prompt(
    terms: &[String],
    max_tokens: usize,
    mut count: impl FnMut(&str) -> Result<usize>,
) -> Result<String> {
    let mut prompt = String::new();
    let mut taken: Vec<String> = Vec::new();
    for term in terms {
        let term = term.trim();
        if term.is_empty() || term.contains('\0') {
            continue;
        }
        let key = term.to_lowercase();
        if taken.contains(&key) {
            continue;
        }
        let len = prompt.len();
        if !prompt.is_empty() {
            prompt.push_str(PROMPT_SEPARATOR);
        }
        prompt.push_str(term);
        if count(&prompt)? <= max_tokens {
            taken.push(key);
        } else {
            prompt.truncate(len);
        }
    }
    Ok(prompt)
}

/// Whisper token times are centiseconds from the start of the input.
pub(crate) fn centis_to_s(cs: i64) -> f32 {
    cs as f32 / 100.0
}

/// Builds [`AsrToken`]s from whisper.cpp's byte-level BPE pieces. A piece can
/// end inside a multi-byte UTF-8 character (common in Portuguese: `ç`, `ã`),
/// so pieces are joined until the bytes form whole characters; the joined
/// token starts at its first piece and ends at its last. Starts are kept
/// monotonic and every time is clamped to the input.
pub(crate) struct TokenAssembler {
    tokens: Vec<AsrToken>,
    pending: Vec<u8>,
    pending_t0: i64,
    pending_t1: i64,
    last_start_cs: i64,
    duration_cs: i64,
}

impl TokenAssembler {
    pub(crate) fn new(samples_16k: usize) -> Self {
        Self {
            tokens: Vec::new(),
            pending: Vec::new(),
            pending_t0: 0,
            pending_t1: 0,
            last_start_cs: 0,
            duration_cs: i64::try_from(samples_16k).unwrap_or(i64::MAX) / SAMPLES_PER_CENTISECOND,
        }
    }

    /// Adds one piece with its whisper.cpp times (`-1` when unknown).
    pub(crate) fn push(&mut self, bytes: &[u8], t0_cs: i64, t1_cs: i64) {
        if bytes.is_empty() {
            return;
        }
        if self.pending.is_empty() {
            self.pending_t0 = t0_cs;
        }
        self.pending_t1 = t1_cs;
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            // An incomplete character at the end: wait for the next piece.
            Err(e) if e.error_len().is_none() => {}
            _ => self.flush(),
        }
    }

    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let text = String::from_utf8(std::mem::take(&mut self.pending))
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        // An unknown (-1) or earlier start becomes the previous start.
        let start = self
            .pending_t0
            .clamp(self.last_start_cs, self.duration_cs.max(self.last_start_cs));
        let end = self.pending_t1.clamp(start, self.duration_cs.max(start));
        self.last_start_cs = start;
        self.tokens.push(AsrToken {
            text,
            start_s: centis_to_s(start),
            duration_s: (end > start).then(|| centis_to_s(end - start)),
        });
    }

    pub(crate) fn finish(mut self) -> Vec<AsrToken> {
        self.flush();
        self.tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(terms: &[&str]) -> Vec<String> {
        terms.iter().map(|t| (*t).to_owned()).collect()
    }

    /// One token per comma-separated term's word, plus one per separator.
    fn fake_count(text: &str) -> Result<usize> {
        Ok(text.split_whitespace().count() + text.matches(',').count())
    }

    #[test]
    fn prompt_keeps_terms_in_order_within_the_limit() {
        let terms = words(&["Ana Lima", "Bruno", "  ", "ana lima", "Kubernetes"]);
        // "Ana Lima, Bruno, Kubernetes" = 4 words + 2 commas.
        assert_eq!(
            fit_prompt(&terms, 6, fake_count).unwrap(),
            "Ana Lima, Bruno, Kubernetes"
        );
        assert_eq!(
            fit_prompt(&terms, 4, fake_count).unwrap(),
            "Ana Lima, Bruno"
        );
        assert_eq!(fit_prompt(&terms, 1, fake_count).unwrap(), "Bruno");
        assert_eq!(fit_prompt(&terms, 0, fake_count).unwrap(), "");
        assert_eq!(fit_prompt(&[], 224, fake_count).unwrap(), "");
    }

    #[test]
    fn prompt_skips_a_term_that_does_not_fit_and_tries_later_ones() {
        let terms = words(&["Ana", "a very long product name here", "Bia", "nul\0term"]);
        assert_eq!(fit_prompt(&terms, 3, fake_count).unwrap(), "Ana, Bia");
        // Never more than the limit, however many terms there are.
        let many: Vec<String> = (0..1_000).map(|i| format!("Term{i}")).collect();
        let prompt = fit_prompt(&many, MAX_PROMPT_TOKENS, fake_count).unwrap();
        assert!(fake_count(&prompt).unwrap() <= MAX_PROMPT_TOKENS);
        assert!(prompt.starts_with("Term0, Term1,"));
    }

    #[test]
    fn prompt_counting_errors_propagate() {
        let err =
            fit_prompt(&words(&["x"]), 10, |_| Err(Error::Inference("boom".into()))).unwrap_err();
        assert_eq!(err.code(), "inference");
    }

    #[test]
    fn centiseconds_become_seconds() {
        assert_eq!(centis_to_s(0), 0.0);
        assert_eq!(centis_to_s(150), 1.5);
        assert_eq!(centis_to_s(2_999), 29.99);
    }

    #[test]
    fn tokens_carry_times_in_seconds() {
        // 3 s of input.
        let mut asm = TokenAssembler::new(48_000);
        asm.push(b" Hello", 10, 52);
        asm.push(b" world", 52, 52);
        let out = asm.finish();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, " Hello");
        assert_eq!(out[0].start_s, 0.1);
        assert_eq!(out[0].duration_s, Some(0.42));
        assert_eq!(out[1].start_s, 0.52);
        assert_eq!(out[1].duration_s, None);
    }

    #[test]
    fn split_characters_are_joined() {
        // " ação" with "ç" and "ã" split across pieces, as byte-level BPE may.
        // " ação" = ' ' 'a' C3 A7 ('ç') C3 A3 ('ã') 'o'.
        let text = " ação".as_bytes();
        let mut asm = TokenAssembler::new(32_000);
        asm.push(&text[..2], 100, 110); // " a"
        asm.push(&text[2..3], 110, 120); // first byte of "ç"
        asm.push(&text[3..4], 120, 130); // completes "ç"
        asm.push(&text[4..], 130, 150); // "ão"
        let out = asm.finish();
        let texts: Vec<&str> = out.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, [" a", "ç", "ão"]);
        assert_eq!(out[1].start_s, 1.1);
        assert_eq!(out[1].duration_s, Some(0.2));
        // A piece completing one character and starting the next waits too.
        let mut asm = TokenAssembler::new(32_000);
        asm.push(&text[..3], 100, 110); // " a" + first byte of "ç"
        asm.push(&text[3..5], 110, 120); // rest of "ç" + first byte of "ã"
        asm.push(&text[5..], 120, 150); // rest of "ã" + "o"
        let out = asm.finish();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, " ação");
        assert_eq!(out[0].start_s, 1.0);
        assert_eq!(out[0].duration_s, Some(0.5));
        // A dangling partial character at the end is kept, replaced.
        let mut asm = TokenAssembler::new(16_000);
        asm.push(b" ok", 0, 10);
        asm.push(&"ã".as_bytes()[..1], 10, 20);
        let out = asm.finish();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].text, "\u{fffd}");
    }

    #[test]
    fn times_are_monotonic_and_inside_the_input() {
        // 1 s of input = 100 cs.
        let mut asm = TokenAssembler::new(16_000);
        asm.push(b" a", 50, 60);
        asm.push(b" b", 40, 45); // earlier than the previous start
        asm.push(b" c", -1, -1); // unknown
        asm.push(b" d", 90, 400); // ends past the input
        asm.push(b" e", 500, 600); // starts past the input
        let out = asm.finish();
        let starts: Vec<f32> = out.iter().map(|t| t.start_s).collect();
        assert_eq!(starts, [0.5, 0.5, 0.5, 0.9, 1.0]);
        assert_eq!(out[3].duration_s, Some(0.1));
        assert_eq!(out[4].duration_s, None);
    }

    #[test]
    fn language_tags_map_to_whisper_codes() {
        assert_eq!(whisper_language("pt-BR").unwrap(), Some("pt"));
        assert_eq!(whisper_language("EN").unwrap(), Some("en"));
        assert_eq!(whisper_language("de_CH").unwrap(), Some("de"));
        assert_eq!(whisper_language("und").unwrap(), None);
        assert_eq!(whisper_language("auto").unwrap(), None);
        assert_eq!(whisper_language("").unwrap(), None);
        assert_eq!(whisper_language("xx").unwrap_err().code(), "unsupported");
        assert_eq!(whisper_language("p\0t").unwrap_err().code(), "unsupported");
    }

    #[test]
    fn whisper_cpp_spellings_of_valid_tags() {
        assert_eq!(whisper_language("nb").unwrap(), Some("no"));
        assert_eq!(whisper_language("nb-NO").unwrap(), Some("no"));
        assert_eq!(whisper_language("iw").unwrap(), Some("he"));
        assert_eq!(whisper_language("iw_IL").unwrap(), Some("he"));
        assert_eq!(whisper_language("jv").unwrap(), Some("jw"));
        assert_eq!(whisper_language("fil").unwrap(), Some("tl"));
        assert_eq!(whisper_language("FIL-PH").unwrap(), Some("tl"));
        // Whisper's own codes still work.
        assert_eq!(whisper_language("no").unwrap(), Some("no"));
        assert_eq!(whisper_language("he").unwrap(), Some("he"));
    }

    #[test]
    fn unknown_tag_falls_back_to_detection() {
        assert_eq!(language_or_detect("xx"), None);
        assert_eq!(language_or_detect("p\0t"), None);
        assert_eq!(language_or_detect("nb-NO"), Some("no"));
        assert_eq!(language_or_detect("pt-BR"), Some("pt"));
    }

    /// Real models (`#[ignore]`d): installed into `BROSCRIBE_DATA_DIR`, or a
    /// temporary folder, from `BROSCRIBE_MODEL_MIRROR` when set, else the
    /// network. Prints counts only, never text.
    mod real {
        use std::path::PathBuf;

        use sherpa_onnx as so;

        use super::*;
        use crate::pull::{PullOptions, pull};
        use crate::registry::Registry;
        use crate::sherpa::{SAMPLE_RATE, SherpaProvider};
        use crate::store::ModelStore;

        const WHISPER: &str = "whisper-large-v3-turbo";
        const PARAKEET: &str = "parakeet-tdt-0.6b-v3";

        fn data_dir() -> PathBuf {
            std::env::var_os("BROSCRIBE_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("broscribe-model-tests"))
        }

        fn provider() -> SherpaProvider {
            let data_dir = data_dir();
            let registry = Registry::builtin().unwrap();
            let opts = PullOptions::from_env(false);
            let execution = crate::ExecutionContext::new(
                broscribe_platform_native::NativePlatform::process_tree_control(),
                broscribe_platform_native::NativePlatform::trust_store(),
            );
            for id in [WHISPER, PARAKEET] {
                pull(
                    registry.get(id).unwrap(),
                    &data_dir,
                    &opts,
                    &mut |_, _| {},
                    &execution,
                )
                .unwrap();
            }
            SherpaProvider::new(registry, &data_dir)
        }

        /// Parakeet's `test_wavs/en.wav` (24 kHz) resampled to 16 kHz.
        fn en_wav(p: &SherpaProvider) -> Vec<f32> {
            let entry = p.registry().get(PARAKEET).unwrap();
            let path = ModelStore::new(&data_dir())
                .content_dir(entry)
                .unwrap()
                .join("test_wavs/en.wav");
            let wave = so::Wave::read(path.to_str().unwrap()).expect("read en.wav");
            let samples = wave.samples();
            if wave.sample_rate() == SAMPLE_RATE {
                return samples.to_vec();
            }
            let resampler = so::LinearResampler::create(wave.sample_rate(), SAMPLE_RATE)
                .expect("create resampler");
            let chunks: Vec<&[f32]> = samples.chunks(4_096).collect();
            let mut out = Vec::new();
            for (i, chunk) in chunks.iter().enumerate() {
                out.extend(resampler.resample(chunk, i + 1 == chunks.len()));
            }
            out
        }

        fn check(out: &AsrOutput, duration: f32) {
            assert!(!out.text.is_empty());
            assert!(!out.tokens.is_empty());
            assert!(out.tokens.windows(2).all(|w| w[0].start_s <= w[1].start_s));
            assert!(
                out.tokens
                    .iter()
                    .all(|t| t.start_s >= 0.0 && t.start_s <= duration)
            );
            // Times are seconds, not centiseconds: the last token starts in
            // the second half of the clip.
            let last = out.tokens.last().unwrap().start_s;
            assert!(
                last >= duration / 2.0,
                "last token at {last} s of {duration} s"
            );
        }

        #[test]
        #[ignore = "needs downloaded models"]
        fn whisper_transcribes_the_test_wav() {
            let p = provider();
            let mut asr = p.load_whisper(WHISPER).unwrap();
            assert!(asr.accepts_hotwords());
            let audio = en_wav(&p);
            let duration = audio.len() as f32 / SAMPLE_RATE as f32;

            let auto = asr.transcribe(&audio).unwrap();
            eprintln!(
                "whisper: {} chars, {} tokens, {duration:.2} s",
                auto.text.len(),
                auto.tokens.len()
            );
            check(&auto, duration);
            check(&asr.transcribe_with(&audio, Some("en")).unwrap(), duration);

            let many: Vec<String> = (0..500).map(|i| format!("Term{i}")).collect();
            asr.set_hotwords(&many).unwrap();
            assert!(!asr.prompt.is_empty());
            assert!(asr.prompt.len() <= MAX_PROMPT_TOKENS);
            check(
                &asr.transcribe_with(&audio, Some("en-US")).unwrap(),
                duration,
            );
            asr.set_hotwords(&[]).unwrap();
            assert!(asr.prompt.is_empty());
            assert!(asr.transcribe(&[]).unwrap().tokens.is_empty());
        }
    }
}
