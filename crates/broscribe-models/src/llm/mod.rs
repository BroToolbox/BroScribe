//! LLM providers (design §8.4, plan M2 §2, §9.7): agent CLIs under the user's
//! own login and OpenAI-compatible HTTP servers, all from `[[provider]]`
//! registry entries. Providers are off until enabled with consent; the
//! pipeline checks consent first and the constructors check it again.
//! Nothing here may run while the library lock is held.

pub mod agent_cli;
pub mod chain;
pub mod detect;
pub mod fake;
pub mod http;
pub mod json;
pub mod models;
pub mod process;
pub mod registry;
pub mod reply;

use std::time::Duration;

use broscribe_core::Result;
use broscribe_core::config::ProviderConfig;
use serde_json::Value;

use crate::execution::ExecutionContext;

pub use agent_cli::{AgentCliProvider, check_consent};
pub use chain::{Attempt, ChainOutput, Verdict, complete_with_fallback};
pub use detect::{Availability, Detection, SearchPaths, detect};
pub use fake::FakeLlm;
pub use http::{OpenAiHttpProvider, api_key_secret, http_endpoint_is_local, normalized_origin};
pub use models::{ListedModel, ModelList, ModelSource, list_models};
pub use registry::{
    ModelListFormat, ModelListing, ProviderEntry, ProviderPurpose, ProviderRegistry,
    ProviderRuntime, ReportedModel, StderrHeader,
};
pub use reply::{
    EnrichmentReply, ReplyContext, TemplateReply, check_enrichment, check_template,
    enrichment_schema, template_schema,
};

/// Default time allowed for one provider call.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// One provider reply.
#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    /// The reply, already checked against the schema.
    pub value: Value,
    /// Model the vendor reported for this call (the registry's
    /// `reported_model` source, or the HTTP response's `model`), when it
    /// passes `validate_model_name`. Never the requested model.
    pub reported_model: Option<String>,
}

pub trait LlmProvider: Send + Sync {
    /// Registry provider id.
    fn id(&self) -> &str;
    /// Model requested for this provider's calls, if any (`None`: the CLI's default).
    fn model(&self) -> Option<&str>;
    /// Sends `prompt` and returns the reply, already checked against `schema`,
    /// with the model the vendor reported. Errors never contain prompt or
    /// reply text, nor reported model text.
    fn complete_json(&self, prompt: &str, schema: &Value, timeout: Duration) -> Result<Completion>;
}

/// Builds the provider for `entry` with its configured model; see
/// [`open_provider_with_model`].
pub fn open_provider(
    entry: &ProviderEntry,
    config: &ProviderConfig,
    search: &SearchPaths,
    api_key: Option<String>,
    execution: &ExecutionContext,
) -> Result<Box<dyn LlmProvider>> {
    open_provider_with_model(entry, config, search, api_key, None, execution)
}

/// Builds the provider for `entry`. `api_key` is the `SecretStore` value under
/// [`api_key_secret`] (HTTP providers only). `model` is chosen for this run
/// only (never written to config) and replaces the configured one; it must
/// pass `validate_model_name`, and an agent CLI whose model flag was not
/// detected refuses it. Refuses providers that are not enabled with
/// consent, not for the `llm` role, unverified or missing a required or
/// safety flag. Agent-CLI processes run as trees contained by `execution`;
/// HTTPS servers need its OS trust roots (`trust_store_unavailable`
/// otherwise, before connecting).
pub fn open_provider_with_model(
    entry: &ProviderEntry,
    config: &ProviderConfig,
    search: &SearchPaths,
    api_key: Option<String>,
    model: Option<&str>,
    execution: &ExecutionContext,
) -> Result<Box<dyn LlmProvider>> {
    check_consent(entry, config)?;
    entry.require_llm()?;
    Ok(match entry.runtime {
        ProviderRuntime::AgentCli => Box::new(AgentCliProvider::new(
            entry, config, search, model, execution,
        )?),
        ProviderRuntime::OpenaiCompatibleHttp => Box::new(OpenAiHttpProvider::new(
            entry, config, api_key, model, execution,
        )?),
    })
}
