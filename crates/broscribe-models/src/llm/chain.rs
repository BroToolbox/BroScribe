//! Ordered fallback over the profile's `llm` providers (design §8.4): each
//! provider gets one retry; a reply that is usable but flawed is retried once
//! and then kept.

use std::time::Duration;

use broscribe_core::{Error, Result};
use serde::Serialize;
use serde_json::Value;

use super::LlmProvider;

/// The caller's judgement of a schema-valid reply.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict<T> {
    Accept(T),
    /// Usable after cleanup (e.g. unknown speaker references cleared):
    /// retried once, kept if the retry is no better.
    Flawed {
        value: T,
        issues: Vec<String>,
    },
    /// Unusable: retry, then the next provider.
    Reject(String),
}

/// One provider call, for reports. `outcome` never holds reply text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Attempt {
    pub provider: String,
    pub outcome: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChainOutput<T> {
    pub value: T,
    pub provider: String,
    /// The model requested from the provider (`None`: its default).
    pub requested_model: Option<String>,
    /// The model the provider reported for the kept reply, if any.
    pub reported_model: Option<String>,
    pub attempts: Vec<Attempt>,
    /// Issues of a flawed reply that was kept; empty when accepted.
    pub issues: Vec<String>,
}

/// Tries `providers` in order, each at most twice, until `check` accepts a
/// reply. Must not be called while holding the library lock.
pub fn complete_with_fallback<T>(
    providers: &[&dyn LlmProvider],
    prompt: &str,
    schema: &Value,
    timeout: Duration,
    check: impl Fn(&Value) -> Verdict<T>,
) -> Result<ChainOutput<T>> {
    if providers.is_empty() {
        return Err(Error::InvalidInput(
            "no LLM provider is enabled for this profile".into(),
        ));
    }
    let mut attempts = Vec::new();
    for provider in providers {
        let mut flawed: Option<(T, Vec<String>, Option<String>)> = None;
        for _ in 0..2 {
            let outcome = match provider.complete_json(prompt, schema, timeout) {
                Err(e) => e.to_string(),
                Ok(reply) => match check(&reply.value) {
                    Verdict::Accept(value) => {
                        attempts.push(Attempt {
                            provider: provider.id().to_string(),
                            outcome: "ok".into(),
                        });
                        return Ok(ChainOutput {
                            value,
                            provider: provider.id().to_string(),
                            requested_model: provider.model().map(str::to_string),
                            reported_model: reply.reported_model,
                            attempts,
                            issues: Vec::new(),
                        });
                    }
                    Verdict::Flawed { value, issues } => {
                        let outcome = format!("flawed: {}", issues.join("; "));
                        flawed = Some((value, issues, reply.reported_model));
                        outcome
                    }
                    Verdict::Reject(why) => format!("rejected: {why}"),
                },
            };
            attempts.push(Attempt {
                provider: provider.id().to_string(),
                outcome,
            });
        }
        if let Some((value, issues, reported_model)) = flawed {
            return Ok(ChainOutput {
                value,
                provider: provider.id().to_string(),
                requested_model: provider.model().map(str::to_string),
                reported_model,
                attempts,
                issues,
            });
        }
    }
    let summary: Vec<String> = attempts
        .iter()
        .map(|a| format!("{}: {}", a.provider, a.outcome))
        .collect();
    Err(Error::Inference(format!(
        "no provider produced a usable reply ({})",
        summary.join("; ")
    )))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::llm::fake::FakeLlm;

    fn schema() -> Value {
        json!({"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}})
    }

    fn check(v: &Value) -> Verdict<i64> {
        match v["n"].as_i64() {
            Some(n) if n >= 10 => Verdict::Accept(n),
            Some(n) if n >= 0 => Verdict::Flawed {
                value: n,
                issues: vec!["small".into()],
            },
            _ => Verdict::Reject("negative".into()),
        }
    }

    #[test]
    fn first_provider_accepts() {
        let a = FakeLlm::new("a");
        a.push_reply(json!({"n": 10}));
        let b = FakeLlm::new("b");
        let out = complete_with_fallback(&[&a, &b], "p", &schema(), Duration::from_secs(1), check)
            .unwrap();
        assert_eq!((out.value, out.provider.as_str()), (10, "a"));
        assert_eq!(b.calls(), 0);
    }

    #[test]
    fn schema_failure_retries_once_then_falls_back() {
        let a = FakeLlm::new("a");
        a.push_reply(json!({"n": "x"}));
        a.push_reply(json!({"wrong": 1}));
        let b = FakeLlm::new("b");
        b.push_reply(json!({"n": 11}));
        let out = complete_with_fallback(&[&a, &b], "p", &schema(), Duration::from_secs(1), check)
            .unwrap();
        assert_eq!(a.calls(), 2);
        assert_eq!((out.value, out.provider.as_str()), (11, "b"));
        assert_eq!(out.attempts.len(), 3);
    }

    #[test]
    fn retry_succeeds_on_same_provider() {
        let a = FakeLlm::new("a");
        a.push_reply(json!({"n": -1}));
        a.push_reply(json!({"n": 12}));
        let out =
            complete_with_fallback(&[&a], "p", &schema(), Duration::from_secs(1), check).unwrap();
        assert_eq!(out.value, 12);
        assert_eq!(a.calls(), 2);
    }

    #[test]
    fn flawed_is_retried_then_kept() {
        let a = FakeLlm::new("a");
        a.push_reply(json!({"n": 3}));
        a.push_reply(json!({"n": 4}));
        let b = FakeLlm::new("b");
        b.push_reply(json!({"n": 50}));
        let out = complete_with_fallback(&[&a, &b], "p", &schema(), Duration::from_secs(1), check)
            .unwrap();
        assert_eq!((out.value, out.provider.as_str()), (4, "a"));
        assert_eq!(out.issues, vec!["small"]);
        assert_eq!(b.calls(), 0, "a flawed reply does not fall back");
    }

    #[test]
    fn all_fail() {
        let a = FakeLlm::new("a");
        a.push_error(Error::Inference("a timed out".into()));
        let err = complete_with_fallback(&[&a], "p", &schema(), Duration::from_secs(1), check)
            .unwrap_err();
        assert!(err.to_string().contains("a timed out"), "{err}");
        assert_eq!(a.calls(), 2, "unscripted second call also fails");
        assert!(
            complete_with_fallback(&[], "p", &schema(), Duration::from_secs(1), check).is_err()
        );
    }

    #[test]
    fn records_requested_and_reported_models() {
        let a = FakeLlm::new("a")
            .with_model("alias")
            .with_reported_model("alias-v5");
        a.push_reply(json!({"n": 10}));
        let out =
            complete_with_fallback(&[&a], "p", &schema(), Duration::from_secs(1), check).unwrap();
        assert_eq!(out.requested_model.as_deref(), Some("alias"));
        assert_eq!(out.reported_model.as_deref(), Some("alias-v5"));

        // Provider default, nothing reported; a kept flawed reply too.
        let b = FakeLlm::new("b");
        b.push_reply(json!({"n": 3}));
        b.push_reply(json!({"n": 4}));
        let out =
            complete_with_fallback(&[&b], "p", &schema(), Duration::from_secs(1), check).unwrap();
        assert_eq!((out.requested_model, out.reported_model), (None, None));
    }
}
