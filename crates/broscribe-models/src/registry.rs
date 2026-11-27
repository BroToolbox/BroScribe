//! Model registry: the embedded `registry.toml` plus optional user overrides in
//! `<data_dir>/models.toml` (same `[[model]]` format; same `id` replaces).
//! Profiles (`[[profile]]`) are defined only in the built-in registry.

use std::path::Path;

use broscribe_core::config::Profile;
use broscribe_core::{Error, Result};
use serde::{Deserialize, Serialize};

const BUILTIN: &str = include_str!("registry.toml");
pub const USER_REGISTRY_FILE: &str = "models.toml";

/// Checks that `id` is usable as a model folder name: a single path
/// component of ASCII `[a-z0-9._-]`, not `.` or `..`. Model IDs name folders
/// under `<data_dir>/models/` that `models pull` deletes and recreates, so an
/// ID must never resolve outside that folder.
pub fn validate_model_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id != "."
        && id != ".."
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "invalid model id {id:?}: use lowercase letters, digits, `.`, `_` and `-`"
        )))
    }
}

/// Model roles (design §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Vad,
    Live,
    Asr,
    Diarization,
    Joint,
    SpeakerEmbedding,
    Aec,
    Llm,
    TextEmbedding,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub roles: Vec<Role>,
    pub runtime: String,
    #[serde(default)]
    pub files: Vec<ModelFile>,
    #[serde(default)]
    pub platforms: Vec<String>,
    /// BCP 47 tags; `*` for any.
    #[serde(default)]
    pub languages: Vec<String>,
    pub license: String,
    #[serde(default)]
    pub license_verified: bool,
    #[serde(default)]
    pub accepts_hotwords: bool,
    /// Runtime parameters, e.g. clustering threshold and thread count.
    #[serde(default)]
    pub params: toml::Table,
    /// For an ASR model without the `live` role: the model the live preview
    /// runs while this one is the profile's ASR model (a declared dependency,
    /// installed with the profile). Unset on a live-capable model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_model: Option<String>,
}

impl ModelEntry {
    pub fn has_role(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }

    /// Gate for `models pull` and `models use`: a model whose licence is not
    /// verified is refused unless the user passed `--allow-unverified-license`.
    pub fn check_license(&self, allow_unverified_license: bool) -> Result<()> {
        if self.license_verified || allow_unverified_license {
            return Ok(());
        }
        Err(Error::InvalidInput(format!(
            "the licence of {} is not verified ({}); pass --allow-unverified-license to use it anyway",
            self.id, self.license
        )))
    }

    /// A numeric parameter (integers are accepted as floats).
    pub fn param_f64(&self, key: &str) -> Result<Option<f64>> {
        match self.params.get(key) {
            None => Ok(None),
            Some(toml::Value::Float(v)) => Ok(Some(*v)),
            Some(toml::Value::Integer(v)) => Ok(Some(*v as f64)),
            Some(_) => Err(self.bad_param(key, "a number")),
        }
    }

    pub fn param_i64(&self, key: &str) -> Result<Option<i64>> {
        match self.params.get(key) {
            None => Ok(None),
            Some(toml::Value::Integer(v)) => Ok(Some(*v)),
            Some(_) => Err(self.bad_param(key, "an integer")),
        }
    }

    pub fn param_str(&self, key: &str) -> Result<Option<&str>> {
        match self.params.get(key) {
            None => Ok(None),
            Some(toml::Value::String(v)) => Ok(Some(v)),
            Some(_) => Err(self.bad_param(key, "a string")),
        }
    }

    fn bad_param(&self, key: &str, expected: &str) -> Error {
        Error::InvalidInput(format!(
            "model {}: parameter `{key}` must be {expected}",
            self.id
        ))
    }

    /// The folder, relative to the model's install folder, holding its files:
    /// the archive root for an extracted archive, else the folder itself.
    pub fn content_root(&self) -> Option<&str> {
        self.files
            .iter()
            .find(|f| f.archive)
            .and_then(|f| f.path_in_archive_root.as_deref())
    }
}

/// A pinned download.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFile {
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
    /// A `.tar.bz2` extracted into the model folder.
    #[serde(default)]
    pub archive: bool,
    /// Top-level folder inside the archive.
    #[serde(default)]
    pub path_in_archive_root: Option<String>,
}

impl ModelFile {
    /// Last URL path segment.
    pub fn file_name(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or(&self.url)
    }
}

/// A named choice of speech models (design §4.1 step 3, §8.3). Choosing one
/// sets these four roles; the text-embedding model and LLM providers stay as
/// the user set them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub id: String,
    /// One line shown where the profile is chosen: the speech model's name.
    pub description: String,
    pub vad: String,
    pub asr: String,
    pub diarization: String,
    pub speaker_embedding: String,
}

impl ProfileEntry {
    /// `active` with this profile's models.
    pub fn apply(&self, active: &Profile) -> Profile {
        Profile {
            vad: self.vad.clone(),
            asr: self.asr.clone(),
            diarization: self.diarization.clone(),
            speaker_embedding: self.speaker_embedding.clone(),
            ..active.clone()
        }
    }

    /// Whether `active` uses exactly this profile's models.
    pub fn is_active(&self, active: &Profile) -> bool {
        self.apply(active) == *active
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(rename = "model", default)]
    pub models: Vec<ModelEntry>,
    #[serde(rename = "profile", default)]
    pub profiles: Vec<ProfileEntry>,
}

impl Registry {
    /// The models shipped with BroScribe.
    pub fn builtin() -> Result<Self> {
        Self::parse(BUILTIN, "built-in registry")
    }

    /// Built-in models with `<data_dir>/models.toml` applied when it exists.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let mut reg = Self::builtin()?;
        let path = data_dir.join(USER_REGISTRY_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let user = Self::parse(&text, &path.display().to_string())?;
                if !user.profiles.is_empty() {
                    return Err(Error::InvalidInput(format!(
                        "{}: profiles are defined only in the built-in registry",
                        path.display()
                    )));
                }
                reg.merge(user)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io_at(&path, e)),
        }
        Ok(reg)
    }

    fn parse(text: &str, what: &str) -> Result<Self> {
        let reg: Self =
            toml::from_str(text).map_err(|e| Error::InvalidInput(format!("{what}: {e}")))?;
        for (i, m) in reg.models.iter().enumerate() {
            if validate_model_id(&m.id).is_err() {
                return Err(Error::InvalidInput(format!(
                    "{what}: invalid model id {:?}: use lowercase letters, digits, `.`, `_` and `-`",
                    m.id
                )));
            }
            if reg.models[..i].iter().any(|o| o.id == m.id) {
                return Err(Error::InvalidInput(format!(
                    "{what}: model {} listed twice",
                    m.id
                )));
            }
            for f in &m.files {
                if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(Error::InvalidInput(format!(
                        "{what}: model {} has an invalid sha256",
                        m.id
                    )));
                }
            }
            if let Some(live) = &m.live_model
                && validate_model_id(live).is_err()
            {
                return Err(Error::InvalidInput(format!(
                    "{what}: model {} declares an invalid live_model {live:?}",
                    m.id
                )));
            }
        }
        Ok(reg)
    }

    /// Adds `other`'s models, replacing those with the same ID.
    pub fn merge(&mut self, other: Registry) {
        for m in other.models {
            match self.models.iter_mut().find(|o| o.id == m.id) {
                Some(slot) => *slot = m,
                None => self.models.push(m),
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    /// The profile `id`, or `Error::InvalidInput` naming the available ones.
    pub fn profile(&self, id: &str) -> Result<&ProfileEntry> {
        self.profiles.iter().find(|p| p.id == id).ok_or_else(|| {
            let ids: Vec<&str> = self.profiles.iter().map(|p| p.id.as_str()).collect();
            Error::InvalidInput(format!(
                "unknown profile {id:?} (available: {})",
                ids.join(", ")
            ))
        })
    }

    /// The model, or `Error::ModelMissing` if the registry does not list it.
    pub fn require(&self, id: &str) -> Result<&ModelEntry> {
        self.get(id)
            .ok_or_else(|| Error::ModelMissing(format!("{id} is not in the model registry")))
    }

    pub fn with_role(&self, role: Role) -> impl Iterator<Item = &ModelEntry> {
        self.models.iter().filter(move |m| m.has_role(role))
    }

    /// `id` as the model of `role`: listed, holding the role, and on a
    /// runtime that can load it in that role.
    fn require_as(&self, id: &str, role: Role) -> Result<&ModelEntry> {
        let entry = self.require(id)?;
        if !entry.has_role(role) {
            return Err(Error::InvalidInput(format!(
                "model {id} does not have the {role:?} role"
            )));
        }
        if !role_runtimes(role).contains(&entry.runtime.as_str()) {
            return Err(Error::InvalidInput(format!(
                "model {id} runs on `{}`, which cannot run the {role:?} role",
                entry.runtime
            )));
        }
        Ok(entry)
    }

    /// The live-preview model of `profile`: its ASR model when that one has
    /// the `live` role, else the live model the ASR model declares
    /// (`live_model`). Never another live model picked silently.
    pub fn live_model(&self, profile: &Profile) -> Result<&ModelEntry> {
        let asr = self.require_as(&profile.asr, Role::Asr)?;
        if asr.has_role(Role::Live) {
            return self.require_as(&asr.id, Role::Live);
        }
        let live = asr.live_model.as_deref().ok_or_else(|| {
            Error::InvalidInput(format!(
                "ASR model {} has no live role and declares no live_model for the live preview",
                asr.id
            ))
        })?;
        self.require_as(live, Role::Live)
    }

    /// Every model `profile` needs, each validated for its role: VAD, ASR,
    /// the live-preview model, diarization and speaker embedding, each listed
    /// once (the live model is the ASR model on live-capable profiles). Text
    /// embedding and LLM providers are not speech-profile models.
    pub fn profile_models(&self, profile: &Profile) -> Result<Vec<&ModelEntry>> {
        let all = [
            self.require_as(&profile.vad, Role::Vad)?,
            self.require_as(&profile.asr, Role::Asr)?,
            self.live_model(profile)?,
            self.require_as(&profile.diarization, Role::Diarization)?,
            self.require_as(&profile.speaker_embedding, Role::SpeakerEmbedding)?,
        ];
        let mut out: Vec<&ModelEntry> = Vec::with_capacity(all.len());
        for entry in all {
            if !out.iter().any(|o| o.id == entry.id) {
                out.push(entry);
            }
        }
        Ok(out)
    }
}

/// Runtimes the loaders accept for `role`.
fn role_runtimes(role: Role) -> &'static [&'static str] {
    match role {
        Role::Asr => &[crate::sherpa::RUNTIME, crate::whisper::RUNTIME],
        Role::Vad | Role::Live | Role::Diarization | Role::SpeakerEmbedding => {
            &[crate::sherpa::RUNTIME]
        }
        Role::TextEmbedding => &[crate::text_embedding::MODEL2VEC_RUNTIME],
        Role::Joint | Role::Aec | Role::Llm => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_profiles_pick_the_asr_and_keep_the_embedder() {
        let r = Registry::builtin().unwrap();
        let default = r.profile("default").unwrap();
        assert_eq!(default.apply(&Profile::default()), Profile::default());
        let multi = r.profile("multilingual").unwrap();
        assert_eq!(multi.asr, "whisper-large-v3-turbo");
        assert_eq!(r.require(&multi.asr).unwrap().runtime, "whisper");
        // Same VAD, diarizer and speaker embedder: switching never re-embeds.
        assert_eq!(multi.speaker_embedding, default.speaker_embedding);
        assert_eq!(multi.vad, default.vad);
        assert_eq!(multi.diarization, default.diarization);
        for p in &r.profiles {
            for (role, id) in [
                (Role::Vad, &p.vad),
                (Role::Asr, &p.asr),
                (Role::Diarization, &p.diarization),
                (Role::SpeakerEmbedding, &p.speaker_embedding),
            ] {
                assert!(r.require(id).unwrap().has_role(role), "{} {id}", p.id);
            }
        }
        // The user's text embedding and LLM providers survive a switch.
        let active = Profile {
            text_embedding: "openai-embeddings".into(),
            llm: vec!["claude-cli".into()],
            ..Profile::default()
        };
        let next = multi.apply(&active);
        assert_eq!(next.text_embedding, "openai-embeddings");
        assert_eq!(next.llm, ["claude-cli"]);
        assert!(multi.is_active(&next));
        assert!(!default.is_active(&next));
        assert!(r.profile("no-such-profile").is_err());
    }

    #[test]
    fn user_registry_cannot_define_profiles() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(USER_REGISTRY_FILE),
            "[[profile]]\nid = \"mine\"\ndescription = \"x\"\nvad = \"silero-vad\"\n\
             asr = \"parakeet-tdt-0.6b-v3\"\ndiarization = \"pyannote-segmentation-3.0\"\n\
             speaker_embedding = \"nemo-titanet-large\"\n",
        )
        .unwrap();
        assert!(Registry::load(dir.path()).is_err());
    }

    #[test]
    fn builtin_has_the_pinned_m1_models() {
        let r = Registry::builtin().unwrap();
        for id in [
            "silero-vad",
            "parakeet-tdt-0.6b-v3",
            "pyannote-segmentation-3.0",
            "nemo-titanet-large",
            "nemo-titanet-small",
        ] {
            assert_eq!(r.get(id).map(|m| m.runtime.as_str()), Some("onnx"), "{id}");
        }
        let parakeet = r.get("parakeet-tdt-0.6b-v3").unwrap();
        assert_eq!(parakeet.roles, [Role::Live, Role::Asr]);
        assert_eq!(parakeet.languages.len(), 25);
        assert!(!parakeet.accepts_hotwords);
        assert_eq!(
            parakeet.files[0].sha256,
            "1495e91b59d7fc53ddca91f3431758ec99f8fa253fdb0e1e851be0f72b14b0f9"
        );
        assert_eq!(
            parakeet.files[0].file_name(),
            "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2"
        );
        let pyannote = r.get("pyannote-segmentation-3.0").unwrap();
        assert_eq!(pyannote.params["threshold"].as_float(), Some(1.0));
        assert_eq!(pyannote.params["num_threads"].as_integer(), Some(6));
        assert!(!r.get("nemo-titanet-small").unwrap().license_verified);
        assert_eq!(r.with_role(Role::SpeakerEmbedding).count(), 2);
        assert_eq!(r.require("nope").unwrap_err().code(), "model_missing");
    }

    #[test]
    fn builtin_entries_are_complete() {
        let r = Registry::builtin().unwrap();
        assert!(r.models.len() >= 6);
        for m in &r.models {
            assert!(!m.roles.is_empty(), "{}", m.id);
            assert!(!m.runtime.is_empty(), "{}", m.id);
            assert!(!m.files.is_empty(), "{}", m.id);
            assert!(!m.license.is_empty(), "{}", m.id);
            if m.runtime == "onnx" {
                assert!(!m.accepts_hotwords, "{}", m.id);
            }
            for f in &m.files {
                assert_eq!(f.sha256.len(), 64);
                assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
                assert!(f.bytes > 0);
                assert!(f.url.starts_with("https://"), "{}", f.url);
                assert_eq!(f.archive, f.file_name().ends_with(".tar.bz2"));
                assert_eq!(f.archive, f.path_in_archive_root.is_some());
            }
        }
        let whisper = r.get("whisper-large-v3-turbo").unwrap();
        assert_eq!(whisper.runtime, "whisper");
        assert_eq!(whisper.roles, [Role::Asr]);
        assert_eq!(whisper.languages, ["*"]);
        assert!(whisper.accepts_hotwords && whisper.license_verified);
        // The ggml model, then the pinned example WAV for setup's model test.
        assert_eq!(whisper.files.len(), 2);
        assert_eq!(whisper.files[1].file_name(), "jfk.wav");
        assert!(
            whisper.files[0]
                .url
                .starts_with("https://huggingface.co/ggerganov/whisper.cpp/resolve/")
        );
        assert_eq!(
            whisper.param_str("model").unwrap(),
            Some(whisper.files[0].file_name())
        );
        let vad = r.get("silero-vad").unwrap();
        assert_eq!(vad.param_f64("max_speech_duration").unwrap(), Some(28.0));
        assert_eq!(vad.param_i64("window_size").unwrap(), Some(512));
        assert_eq!(vad.content_root(), None);
        let asr = r.get("parakeet-tdt-0.6b-v3").unwrap();
        assert_eq!(
            asr.param_str("model_type").unwrap(),
            Some("nemo_transducer")
        );
        assert_eq!(
            asr.content_root(),
            Some("sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8")
        );
        let diar = r.get("pyannote-segmentation-3.0").unwrap();
        assert_eq!(diar.param_f64("num_threads").unwrap(), Some(6.0));
        assert_eq!(diar.param_str("missing").unwrap(), None);
        assert_eq!(
            diar.param_i64("threshold").unwrap_err().code(),
            "invalid_input"
        );
    }

    #[test]
    fn unverified_licence_needs_the_flag() {
        let r = Registry::builtin().unwrap();
        let small = r.get("nemo-titanet-small").unwrap();
        let err = small.check_license(false).unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        assert!(err.to_string().contains("--allow-unverified-license"));
        small.check_license(true).unwrap();
        r.get("nemo-titanet-large")
            .unwrap()
            .check_license(false)
            .unwrap();
    }

    #[test]
    fn user_file_overrides_and_adds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(USER_REGISTRY_FILE),
            r#"
[[model]]
id = "silero-vad"
roles = ["vad"]
runtime = "onnx"
license = "MIT"
params = { threshold = 0.4 }

[[model]]
id = "whisper-large-v3-turbo-q8"
roles = ["asr"]
runtime = "whisper"
languages = ["*"]
license = "MIT"
"#,
        )
        .unwrap();
        let r = Registry::load(dir.path()).unwrap();
        assert_eq!(
            r.models.len(),
            Registry::builtin().unwrap().models.len() + 1
        );
        assert_eq!(
            r.get("silero-vad").unwrap().params["threshold"].as_float(),
            Some(0.4)
        );
        assert!(
            r.get("whisper-large-v3-turbo-q8")
                .unwrap()
                .has_role(Role::Asr)
        );
    }

    #[test]
    fn model_ids_must_be_plain_folder_names() {
        for good in ["silero-vad", "parakeet-tdt-0.6b-v3", "a_b", "x..y"] {
            validate_model_id(good).unwrap();
        }
        for bad in [
            "", ".", "..", "a/b", "/abs", "/", "a\\b", "Upper", "a b", "../x",
        ] {
            assert_eq!(
                validate_model_id(bad).unwrap_err().code(),
                "invalid_input",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn user_file_with_an_escaping_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["..", "a/b", "/tmp/x", ""] {
            std::fs::write(
                dir.path().join(USER_REGISTRY_FILE),
                format!("[[model]]\nid = {id:?}\nroles = [\"vad\"]\nruntime = \"onnx\"\nlicense = \"MIT\"\n"),
            )
            .unwrap();
            let err = Registry::load(dir.path()).unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{id:?}");
        }
    }

    fn profile_ids(models: Vec<&ModelEntry>) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn fresh_multilingual_profile_requires_an_executable_live_model() {
        let r = Registry::builtin().unwrap();
        let multi = r
            .profile("multilingual")
            .unwrap()
            .apply(&Profile::default());
        let live = r.live_model(&multi).unwrap();
        assert!(live.has_role(Role::Live));
        assert_eq!(live.runtime, crate::sherpa::RUNTIME);
        assert_ne!(live.id, multi.asr, "whisper cannot run the live preview");
        assert_eq!(
            profile_ids(r.profile_models(&multi).unwrap()),
            [
                "silero-vad",
                "whisper-large-v3-turbo",
                live.id.as_str(),
                "pyannote-segmentation-3.0",
                "nemo-titanet-large",
            ]
        );
        // The default profile's ASR model is its own live model: listed once.
        let default = Profile::default();
        assert_eq!(r.live_model(&default).unwrap().id, default.asr);
        assert_eq!(
            profile_ids(r.profile_models(&default).unwrap()),
            [
                "silero-vad",
                "parakeet-tdt-0.6b-v3",
                "pyannote-segmentation-3.0",
                "nemo-titanet-large",
            ]
        );
        for p in &r.profiles {
            let models = r.profile_models(&p.apply(&Profile::default())).unwrap();
            assert!(models.iter().any(|m| m.has_role(Role::Live)), "{}", p.id);
        }
    }

    #[test]
    fn profile_models_validate_roles_runtimes_and_live_dependencies() {
        let r = Registry::builtin().unwrap();
        let wrong_role = Profile {
            vad: "nemo-titanet-large".into(),
            ..Profile::default()
        };
        assert_eq!(
            r.profile_models(&wrong_role).unwrap_err().code(),
            "invalid_input"
        );
        let unknown = Profile {
            diarization: "no-such-model".into(),
            ..Profile::default()
        };
        assert_eq!(
            r.profile_models(&unknown).unwrap_err().code(),
            "model_missing"
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(USER_REGISTRY_FILE),
            r#"
[[model]]
id = "whisper-no-live"
roles = ["asr"]
runtime = "whisper"
license = "MIT"

[[model]]
id = "whisper-bad-live"
roles = ["asr"]
runtime = "whisper"
license = "MIT"
live_model = "silero-vad"

[[model]]
id = "whisper-missing-live"
roles = ["asr"]
runtime = "whisper"
license = "MIT"
live_model = "not-listed"

[[model]]
id = "streaming-whisper"
roles = ["live", "asr"]
runtime = "whisper"
license = "MIT"

[[model]]
id = "strange-runtime"
roles = ["asr"]
runtime = "tflite"
license = "MIT"
live_model = "parakeet-tdt-0.6b-v3"
"#,
        )
        .unwrap();
        let r = Registry::load(dir.path()).unwrap();
        let with_asr = |asr: &str| Profile {
            asr: asr.into(),
            ..Profile::default()
        };
        // Never a silent pick of another installed live model.
        assert_eq!(
            r.live_model(&with_asr("whisper-no-live"))
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        assert_eq!(
            r.live_model(&with_asr("whisper-bad-live"))
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        assert_eq!(
            r.live_model(&with_asr("whisper-missing-live"))
                .unwrap_err()
                .code(),
            "model_missing"
        );
        // A live role on a runtime the live worker cannot run is refused.
        assert_eq!(
            r.live_model(&with_asr("streaming-whisper"))
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        assert_eq!(
            r.profile_models(&with_asr("strange-runtime"))
                .unwrap_err()
                .code(),
            "invalid_input"
        );
    }

    #[test]
    fn declared_live_model_must_be_a_plain_model_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(USER_REGISTRY_FILE),
            "[[model]]\nid = \"w\"\nroles = [\"asr\"]\nruntime = \"whisper\"\nlicense = \"MIT\"\nlive_model = \"../x\"\n",
        )
        .unwrap();
        assert_eq!(
            Registry::load(dir.path()).unwrap_err().code(),
            "invalid_input"
        );
    }
}
