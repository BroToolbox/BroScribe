//! Model registry (design §8), model store, `pull`, runtime adapter traits
//! and their sherpa-onnx implementations.

pub mod adapters;
pub mod execution;
pub mod fakes;
pub mod http;
pub mod llm;
pub mod preflight;
pub mod pull;
pub mod registry;
pub mod sherpa;
pub mod store;
pub mod text_embedding;
pub mod whisper;

pub use adapters::{
    Asr, AsrOutput, AsrToken, DiarSegment, Diarizer, ModelProvider, ModelSet, SpeakerEmbedder,
    SpeechSegment, Vad,
};
pub use execution::{ExecutionContext, TRUST_STORE_UNAVAILABLE, TlsState};
pub use pull::{MIRROR_ENV, PullOptions, PullOutcome, pull};
pub use registry::{ModelEntry, ModelFile, ProfileEntry, Registry, Role};
pub use sherpa::SherpaProvider;
pub use store::{Health, ModelStore, VerifiedMarker};
pub use text_embedding::{FakeTextEmbedder, HttpEmbedder, Model2VecEmbedder, TextEmbedder};
pub use whisper::WhisperAsr;
