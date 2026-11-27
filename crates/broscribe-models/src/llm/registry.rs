//! `[[provider]]` entries of the model registry (design §8.4): the embedded
//! `registry.toml` plus `[[provider]]` entries in `<data_dir>/models.toml`
//! (same `id` replaces). Providers come from here, never from code.

use std::path::Path;

use broscribe_core::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::registry::{USER_REGISTRY_FILE, validate_model_id};

const BUILTIN: &str = include_str!("../registry.toml");

/// Placeholders allowed in argument values. There is deliberately none for
/// the prompt: transcripts never go in argv.
pub const PLACEHOLDERS: [&str; 6] = [
    "{schema}",
    "{schema_file}",
    "{prompt_file}",
    "{output_file}",
    "{model}",
    "{timeout_s}",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderRuntime {
    AgentCli,
    OpenaiCompatibleHttp,
}

/// What a provider is for; enrichment uses only `llm` providers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderPurpose {
    #[default]
    Llm,
    TextEmbedding,
}

/// How an agent CLI receives the prompt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptTransport {
    /// Written to the child's stdin.
    Stdin,
    /// A mode-0600 file in the call's temp dir, named by `{prompt_file}`.
    File,
    /// No verified way to keep the prompt off argv or tools off: refused.
    #[default]
    Unverified,
}

/// Where the agent CLI writes its reply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputSource {
    #[default]
    Stdout,
    /// The file named by `{output_file}`.
    File,
}

/// Why an argument is passed; decides what a missing flag means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgNeed {
    /// The call cannot work without it: missing → refused.
    Required,
    /// Tools off, no MCP, no session persistence, user config off: missing → refused.
    Safety,
    /// Dropped when missing; the provider is reported as degraded.
    Optional,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgSpec {
    pub flag: String,
    #[serde(default)]
    pub values: Vec<String>,
    pub need: ArgNeed,
}

/// Feature switches turned off by a repeatable flag (Codex `--disable`),
/// restricted at runtime to the names the CLI itself lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureSwitches {
    pub flag: String,
    /// Arguments that print the feature table (first column = name).
    pub list_args: Vec<String>,
    pub features: Vec<String>,
}

/// How a provider's model list is read (`[provider.models] format`). Each
/// format has its parser in `llm::models`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelListFormat {
    /// `{"models": [{"slug", "display_name"?}]}` (Codex `debug models`).
    CodexJson,
    /// Lines `* id (default)` / `- id` (Grok `models`).
    MarkedLines,
    /// Quoted names in the description of the `model_args` flag in the
    /// CLI's help text (Claude Code, which has no list command).
    HelpAliases,
    /// `GET {base_url}/models` → `data[].id` (OpenAI-compatible servers).
    OpenaiModels,
}

/// `[provider.models]`: how to list a provider's models.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelListing {
    /// Arguments of the agent CLI's listing command (no placeholders); empty
    /// for `openai-models`.
    #[serde(default)]
    pub args: Vec<String>,
    pub format: ModelListFormat,
}

/// `reported_model`: where an agent CLI reports the model that actually
/// answered (aliases and the provider default resolve to a versioned name).
/// Extracted names must pass `validate_model_name`; anything else is ignored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ReportedModel {
    /// The keys of the object under `json_object` in the CLI's JSON envelope
    /// (before `output_keys` unwrapping): one key is the model; with several,
    /// the key whose `rank_by` number is strictly the largest; otherwise none.
    JsonObject {
        json_object: String,
        rank_by: String,
    },
    /// The first line starting with `prefix` strictly between the first two
    /// lines equal to `separator` in the head of stderr.
    StderrHeader { stderr_header: StderrHeader },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StderrHeader {
    pub separator: String,
    pub prefix: String,
}

fn default_version_args() -> Vec<String> {
    vec!["--version".into()]
}

fn default_help_args() -> Vec<String> {
    vec!["--help".into()]
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub id: String,
    pub name: String,
    /// Who receives the transcript text (consent text).
    pub vendor: String,
    #[serde(default)]
    pub terms_url: Option<String>,
    pub runtime: ProviderRuntime,
    #[serde(default)]
    pub purpose: ProviderPurpose,

    // agent-cli
    #[serde(default)]
    pub binary: Option<String>,
    /// CLI version the flags were checked against.
    #[serde(default)]
    pub version_checked: Option<String>,
    /// Leading positional arguments (`exec`, `run`).
    #[serde(default)]
    pub subcommand: Vec<String>,
    #[serde(default = "default_version_args")]
    pub version_args: Vec<String>,
    #[serde(default = "default_help_args")]
    pub help_args: Vec<String>,
    #[serde(default)]
    pub prompt_transport: PromptTransport,
    #[serde(default)]
    pub output: OutputSource,
    /// Keys of the CLI's JSON envelope that hold the reply, tried in order
    /// (an object, or a string containing JSON). Empty: the output is the reply.
    #[serde(default)]
    pub output_keys: Vec<String>,
    /// Envelope key that is `true` when the CLI reports a failed turn.
    #[serde(default)]
    pub error_flag_key: Option<String>,
    #[serde(default)]
    pub args: Vec<ArgSpec>,
    /// Appended only when a model is configured or chosen for the run.
    #[serde(default)]
    pub model_args: Vec<String>,
    #[serde(default)]
    pub trailing_args: Vec<String>,
    #[serde(default)]
    pub disable_features: Option<FeatureSwitches>,

    // openai-compatible-http
    /// Path appended to the configured base URL (which includes `/v1`).
    #[serde(default)]
    pub endpoint: Option<String>,

    /// How to list the provider's models; `None`: no live list.
    #[serde(default)]
    pub models: Option<ModelListing>,

    /// Where the CLI reports the model that answered; `None`: not reported.
    /// HTTP providers use the response's `model` and take no key.
    #[serde(default)]
    pub reported_model: Option<ReportedModel>,
}

impl ProviderEntry {
    /// Refuses providers that are not for the `llm` role (e.g. remote text embeddings).
    pub fn require_llm(&self) -> Result<()> {
        if self.purpose == ProviderPurpose::Llm {
            Ok(())
        } else {
            Err(Error::InvalidInput(format!(
                "provider {} is not an LLM provider",
                self.id
            )))
        }
    }

    /// The text shown before enabling the provider (design §8.4 "Terms and consent").
    pub fn consent_text(&self) -> String {
        let mut text = format!(
            "Enabling {} sends meeting transcript text (speaker labels, timestamps and what was said) to {}",
            self.name, self.vendor
        );
        match self.runtime {
            ProviderRuntime::AgentCli => text.push_str(
                " through its installed command-line tool, under your own account and plan. \
                 Some consumer plans restrict automated or third-party use; that is your call.",
            ),
            ProviderRuntime::OpenaiCompatibleHttp => {
                text.push_str(", under the terms of whoever runs that server.")
            }
        }
        text.push_str(" Audio and voice data are never sent.");
        if let Some(url) = &self.terms_url {
            text.push_str(&format!(" Terms: {url}"));
        }
        text
    }

    fn all_arg_values(&self) -> impl Iterator<Item = &str> {
        self.args
            .iter()
            .flat_map(|a| a.values.iter())
            .chain(self.model_args.iter())
            .chain(self.trailing_args.iter())
            .map(String::as_str)
    }

    fn uses(&self, placeholder: &str) -> bool {
        self.all_arg_values().any(|v| v.contains(placeholder))
    }

    fn check(&self, what: &str) -> Result<()> {
        let bad = |msg: String| {
            Err(Error::InvalidInput(format!(
                "{what}: provider {}: {msg}",
                self.id
            )))
        };
        if validate_model_id(&self.id).is_err() {
            return bad("invalid id: use lowercase letters, digits, `.`, `_` and `-`".into());
        }
        match self.runtime {
            ProviderRuntime::AgentCli => {
                if self.binary.as_deref().is_none_or(str::is_empty) {
                    return bad("agent-cli providers need `binary`".into());
                }
                for v in self.all_arg_values() {
                    let mut rest = v;
                    while let Some(start) = rest.find('{') {
                        let tail = &rest[start..];
                        let Some(end) = tail.find('}') else {
                            return bad(format!("unterminated placeholder in {v:?}"));
                        };
                        let ph = &tail[..=end];
                        if !PLACEHOLDERS.contains(&ph) {
                            return bad(format!("unknown placeholder {ph}"));
                        }
                        rest = &tail[end + 1..];
                    }
                }
                let file_prompt = self.uses("{prompt_file}");
                match self.prompt_transport {
                    PromptTransport::File if !file_prompt => {
                        return bad("prompt_transport = \"file\" needs {prompt_file}".into());
                    }
                    PromptTransport::Stdin if file_prompt => {
                        return bad("{prompt_file} needs prompt_transport = \"file\"".into());
                    }
                    _ => {}
                }
                if self.output == OutputSource::File && !self.uses("{output_file}") {
                    return bad("output = \"file\" needs {output_file}".into());
                }
                if !self.model_args.is_empty()
                    && !self.model_args.iter().any(|a| a.contains("{model}"))
                {
                    return bad("model_args must contain {model}".into());
                }
            }
            ProviderRuntime::OpenaiCompatibleHttp => {
                if self.endpoint.as_deref().is_none_or(str::is_empty) {
                    return bad("openai-compatible-http providers need `endpoint`".into());
                }
            }
        }
        if let Some(listing) = &self.models {
            self.check_listing(listing).or_else(bad)?;
        }
        if let Some(reported) = &self.reported_model {
            if self.runtime == ProviderRuntime::OpenaiCompatibleHttp {
                return bad(
                    "reported_model is for agent CLIs; HTTP uses the response's `model`".into(),
                );
            }
            let (a, b) = match reported {
                ReportedModel::JsonObject {
                    json_object,
                    rank_by,
                } => (json_object, rank_by),
                ReportedModel::StderrHeader { stderr_header } => {
                    (&stderr_header.separator, &stderr_header.prefix)
                }
            };
            if a.trim().is_empty() || b.trim().is_empty() {
                return bad("reported_model fields must not be empty".into());
            }
        }
        Ok(())
    }

    /// `[provider.models]` fits the provider: an LLM provider, a format of
    /// its runtime, literal arguments only, and (agent CLIs) a verified
    /// prompt transport, since only those can generate.
    fn check_listing(&self, listing: &ModelListing) -> std::result::Result<(), String> {
        if self.purpose != ProviderPurpose::Llm {
            return Err("models: only LLM providers list models".into());
        }
        if let Some(a) = listing.args.iter().find(|a| a.contains(['{', '}'])) {
            return Err(format!("models.args must not contain placeholders: {a:?}"));
        }
        let http = listing.format == ModelListFormat::OpenaiModels;
        match self.runtime {
            ProviderRuntime::OpenaiCompatibleHttp if !http => {
                Err("models.format must be \"openai-models\" for an HTTP provider".into())
            }
            ProviderRuntime::OpenaiCompatibleHttp if !listing.args.is_empty() => {
                Err("models.args are not used by \"openai-models\"".into())
            }
            ProviderRuntime::OpenaiCompatibleHttp => Ok(()),
            ProviderRuntime::AgentCli if http => {
                Err("models.format \"openai-models\" needs an HTTP provider".into())
            }
            ProviderRuntime::AgentCli if listing.args.is_empty() => {
                Err("models.args must name the listing command".into())
            }
            ProviderRuntime::AgentCli if self.prompt_transport == PromptTransport::Unverified => {
                Err("models: a provider with an unverified prompt transport cannot list".into())
            }
            ProviderRuntime::AgentCli
                if listing.format == ModelListFormat::HelpAliases
                    && !self.model_args.first().is_some_and(|f| f.starts_with('-')) =>
            {
                Err("models.format \"help-aliases\" needs a model flag in model_args".into())
            }
            ProviderRuntime::AgentCli => Ok(()),
        }
    }
}

#[derive(Default, Deserialize)]
struct ProviderFile {
    #[serde(rename = "provider", default)]
    providers: Vec<ProviderEntry>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderRegistry {
    pub providers: Vec<ProviderEntry>,
}

impl ProviderRegistry {
    /// The providers shipped with BroScribe.
    pub fn builtin() -> Result<Self> {
        Self::parse(BUILTIN, "built-in registry")
    }

    /// Built-in providers with `[[provider]]` entries of `<data_dir>/models.toml` applied.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let mut reg = Self::builtin()?;
        let path = data_dir.join(USER_REGISTRY_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => reg.merge(Self::parse(&text, &path.display().to_string())?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io_at(&path, e)),
        }
        Ok(reg)
    }

    pub fn parse(text: &str, what: &str) -> Result<Self> {
        let file: ProviderFile =
            toml::from_str(text).map_err(|e| Error::InvalidInput(format!("{what}: {e}")))?;
        for (i, p) in file.providers.iter().enumerate() {
            p.check(what)?;
            if file.providers[..i].iter().any(|o| o.id == p.id) {
                return Err(Error::InvalidInput(format!(
                    "{what}: provider {} listed twice",
                    p.id
                )));
            }
        }
        Ok(Self {
            providers: file.providers,
        })
    }

    /// Adds `other`'s providers, replacing those with the same ID.
    pub fn merge(&mut self, other: ProviderRegistry) {
        for p in other.providers {
            match self.providers.iter_mut().find(|o| o.id == p.id) {
                Some(slot) => *slot = p,
                None => self.providers.push(p),
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&ProviderEntry> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn require(&self, id: &str) -> Result<&ProviderEntry> {
        self.get(id)
            .ok_or_else(|| Error::NotFound(format!("provider {id} is not in the registry")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_providers_parse() {
        let reg = ProviderRegistry::builtin().unwrap();
        for id in [
            "claude-cli",
            "codex-cli",
            "grok-cli",
            "agy-cli",
            "gemini-cli",
            "opencode-cli",
            "openai-http",
        ] {
            assert!(reg.get(id).is_some(), "{id}");
        }
        for id in ["claude-cli", "codex-cli", "grok-cli"] {
            assert_ne!(
                reg.get(id).unwrap().prompt_transport,
                PromptTransport::Unverified,
                "{id}"
            );
        }
        for id in ["agy-cli", "gemini-cli", "opencode-cli"] {
            assert_eq!(
                reg.get(id).unwrap().prompt_transport,
                PromptTransport::Unverified,
                "{id}"
            );
        }
        let codex = reg.get("codex-cli").unwrap();
        assert!(codex.disable_features.is_some());
        assert_eq!(codex.output, OutputSource::File);
    }

    #[test]
    fn every_agent_cli_has_safety_flags() {
        let reg = ProviderRegistry::builtin().unwrap();
        for p in &reg.providers {
            if p.runtime == ProviderRuntime::AgentCli
                && p.prompt_transport != PromptTransport::Unverified
            {
                assert!(
                    p.args.iter().any(|a| a.need == ArgNeed::Safety),
                    "{} lists no safety flag",
                    p.id
                );
            }
        }
    }

    #[test]
    fn rejects_unknown_placeholder_and_prompt_in_argv() {
        let text = r#"
[[provider]]
id = "x"
name = "X"
vendor = "X"
runtime = "agent-cli"
binary = "x"
prompt_transport = "stdin"
args = [{ flag = "--msg", values = ["{prompt}"], need = "required" }]
"#;
        let err = ProviderRegistry::parse(text, "t").unwrap_err();
        assert!(
            err.to_string().contains("unknown placeholder {prompt}"),
            "{err}"
        );
    }

    #[test]
    fn file_transport_needs_prompt_file() {
        let text = r#"
[[provider]]
id = "x"
name = "X"
vendor = "X"
runtime = "agent-cli"
binary = "x"
prompt_transport = "file"
"#;
        assert!(ProviderRegistry::parse(text, "t").is_err());
    }

    #[test]
    fn user_entries_replace_builtin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(USER_REGISTRY_FILE),
            r#"
[[provider]]
id = "claude-cli"
name = "Mine"
vendor = "Me"
runtime = "openai-compatible-http"
endpoint = "chat/completions"
"#,
        )
        .unwrap();
        let reg = ProviderRegistry::load(dir.path()).unwrap();
        assert_eq!(reg.require("claude-cli").unwrap().name, "Mine");
        assert!(reg.get("codex-cli").is_some());
    }

    #[test]
    fn consent_text_names_vendor_and_terms() {
        let reg = ProviderRegistry::builtin().unwrap();
        let text = reg.get("claude-cli").unwrap().consent_text();
        assert!(text.contains("Anthropic"));
        assert!(text.contains("never sent"));
        assert!(text.contains("https://"));
    }

    #[test]
    fn builtin_model_listings() {
        let reg = ProviderRegistry::builtin().unwrap();
        let format = |id: &str| reg.get(id).unwrap().models.as_ref().map(|m| m.format);
        assert_eq!(format("claude-cli"), Some(ModelListFormat::HelpAliases));
        assert_eq!(format("codex-cli"), Some(ModelListFormat::CodexJson));
        assert_eq!(format("grok-cli"), Some(ModelListFormat::MarkedLines));
        assert_eq!(format("openai-http"), Some(ModelListFormat::OpenaiModels));
        for id in ["agy-cli", "gemini-cli", "opencode-cli", "openai-embeddings"] {
            assert_eq!(format(id), None, "{id}");
        }
        let codex = reg.get("codex-cli").unwrap().models.clone().unwrap();
        assert_eq!(codex.args, ["debug", "models"]);
    }

    fn listing_entry(runtime_and_more: &str, models: &str) -> String {
        format!(
            "[[provider]]\nid = \"x\"\nname = \"X\"\nvendor = \"X\"\n{runtime_and_more}\n\n[provider.models]\n{models}\n"
        )
    }

    const CLI: &str = "runtime = \"agent-cli\"\nbinary = \"x\"\nprompt_transport = \"stdin\"\nmodel_args = [\"--model\", \"{model}\"]";
    const HTTP: &str = "runtime = \"openai-compatible-http\"\nendpoint = \"chat/completions\"";

    #[test]
    fn model_listing_validation() {
        let ok = [
            listing_entry(CLI, "args = [\"models\"]\nformat = \"marked-lines\""),
            listing_entry(CLI, "args = [\"--help\"]\nformat = \"help-aliases\""),
            listing_entry(
                CLI,
                "args = [\"debug\", \"models\"]\nformat = \"codex-json\"",
            ),
            listing_entry(HTTP, "format = \"openai-models\""),
        ];
        for text in &ok {
            ProviderRegistry::parse(text, "t").unwrap();
        }
        let bad = [
            // Unknown format.
            listing_entry(CLI, "args = [\"models\"]\nformat = \"tsv\""),
            listing_entry(CLI, "args = [\"models\"]"),
            // Placeholders (and anything brace-like) in listing args.
            listing_entry(
                CLI,
                "args = [\"models\", \"{model}\"]\nformat = \"marked-lines\"",
            ),
            listing_entry(CLI, "args = [\"{prompt_file}\"]\nformat = \"marked-lines\""),
            listing_entry(CLI, "args = [\"a}\"]\nformat = \"marked-lines\""),
            // Format of the other runtime, missing command, stray args.
            listing_entry(CLI, "args = [\"models\"]\nformat = \"openai-models\""),
            listing_entry(CLI, "format = \"marked-lines\""),
            listing_entry(HTTP, "format = \"codex-json\""),
            listing_entry(HTTP, "args = [\"x\"]\nformat = \"openai-models\""),
            // Unknown key.
            listing_entry(
                CLI,
                "args = [\"models\"]\nformat = \"marked-lines\"\nshell = true",
            ),
            // Unverified transport cannot list.
            listing_entry(
                "runtime = \"agent-cli\"\nbinary = \"x\"\nmodel_args = [\"--model\", \"{model}\"]",
                "args = [\"models\"]\nformat = \"marked-lines\"",
            ),
            // help-aliases needs a model flag.
            listing_entry(
                "runtime = \"agent-cli\"\nbinary = \"x\"\nprompt_transport = \"stdin\"",
                "args = [\"--help\"]\nformat = \"help-aliases\"",
            ),
            // Not an LLM provider.
            listing_entry(
                &format!("{HTTP}\npurpose = \"text_embedding\""),
                "format = \"openai-models\"",
            ),
        ];
        for text in &bad {
            assert!(ProviderRegistry::parse(text, "t").is_err(), "{text}");
        }
    }

    #[test]
    fn builtin_reported_model_sources() {
        let reg = ProviderRegistry::builtin().unwrap();
        let usage = Some(ReportedModel::JsonObject {
            json_object: "modelUsage".into(),
            rank_by: "outputTokens".into(),
        });
        assert_eq!(reg.get("claude-cli").unwrap().reported_model, usage);
        assert_eq!(reg.get("grok-cli").unwrap().reported_model, usage);
        let codex = reg.get("codex-cli").unwrap();
        assert_eq!(
            codex.reported_model,
            Some(ReportedModel::StderrHeader {
                stderr_header: StderrHeader {
                    separator: "--------".into(),
                    prefix: "model: ".into(),
                }
            })
        );
        // Without --json codex prints the header the source reads.
        assert!(!codex.args.iter().any(|a| a.flag == "--json"));
        assert_eq!(reg.get("openai-http").unwrap().reported_model, None);
    }

    #[test]
    fn reported_model_validation() {
        let entry = |more: &str| {
            format!("[[provider]]\nid = \"x\"\nname = \"X\"\nvendor = \"X\"\n{more}\n")
        };
        let ok = [
            entry(&format!(
                "{CLI}\nreported_model = {{ json_object = \"usage\", rank_by = \"tokens\" }}"
            )),
            entry(&format!(
                "{CLI}\nreported_model = {{ stderr_header = {{ separator = \"--\", prefix = \"model: \" }} }}"
            )),
        ];
        for text in &ok {
            ProviderRegistry::parse(text, "t").unwrap();
        }
        let bad = [
            // HTTP uses the response's `model`.
            entry(&format!(
                "{HTTP}\nreported_model = {{ json_object = \"usage\", rank_by = \"tokens\" }}"
            )),
            // Empty fields.
            entry(&format!(
                "{CLI}\nreported_model = {{ json_object = \"\", rank_by = \"tokens\" }}"
            )),
            entry(&format!(
                "{CLI}\nreported_model = {{ json_object = \"usage\", rank_by = \" \" }}"
            )),
            entry(&format!(
                "{CLI}\nreported_model = {{ stderr_header = {{ separator = \"\", prefix = \"model: \" }} }}"
            )),
            entry(&format!(
                "{CLI}\nreported_model = {{ stderr_header = {{ separator = \"--\", prefix = \"\" }} }}"
            )),
            // Missing or unknown keys.
            entry(&format!(
                "{CLI}\nreported_model = {{ json_object = \"usage\" }}"
            )),
            entry(&format!(
                "{CLI}\nreported_model = {{ json_object = \"usage\", rank_by = \"t\", pick = \"first\" }}"
            )),
            entry(&format!(
                "{CLI}\nreported_model = {{ stderr_header = {{ separator = \"--\" }} }}"
            )),
            entry(&format!("{CLI}\nreported_model = \"modelUsage\"")),
        ];
        for text in &bad {
            assert!(ProviderRegistry::parse(text, "t").is_err(), "{text}");
        }
    }
}
