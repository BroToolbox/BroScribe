//! `openai-compatible-http` runtime: `/v1/chat/completions` on a local server
//! or a paid API (design §8.2). The API key comes from the platform
//! `SecretStore` (the caller reads it under [`api_key_secret`]) and is never
//! logged.

use std::time::Duration;

use broscribe_core::config::{ProviderConfig, validate_model_name};
use broscribe_core::provider_address::{loopback_host, parse_provider_address};
use broscribe_core::{Error, Result};
use serde_json::{Value, json};

use crate::execution::ExecutionContext;

use super::agent_cli::check_consent;
use super::json::{check_schema, parse_json_text};
use super::registry::{ProviderEntry, ProviderRuntime};
use super::{Completion, LlmProvider};

/// `SecretStore` key of a provider's API key (shared with remote text embeddings).
pub fn api_key_secret(provider_id: &str) -> String {
    format!("provider/{provider_id}/api_key")
}

pub struct OpenAiHttpProvider {
    id: String,
    url: String,
    model: String,
    api_key: Option<String>,
    /// The context's shared connection pool and TLS strategy. Each request sets
    /// socket-I/O budgets; synchronous OS verification can outlive those timers.
    agent: ureq::Agent,
}

impl std::fmt::Debug for OpenAiHttpProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiHttpProvider")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

pub fn http_endpoint_is_local(base_url: &str) -> Result<bool> {
    Ok(loopback_host(parse_provider_address(base_url)?.host))
}

/// `base_url` + `endpoint`. `https` anywhere; plain `http` only for loopback
/// hosts, so transcript text never crosses a network unencrypted.
pub fn endpoint_url(base_url: &str, endpoint: &str) -> Result<String> {
    let base = parse_provider_address(base_url)?;
    Ok(format!(
        "{}/{}",
        base.trimmed.trim_end_matches('/'),
        endpoint.trim_start_matches('/')
    ))
}

/// The server a `base_url` names, as `scheme://host[:port]`: scheme and host
/// lowercase, the scheme's default port omitted. Two base URLs with the same
/// origin reach the same server.
pub fn normalized_origin(base_url: &str) -> Result<String> {
    let base = parse_provider_address(base_url)?;
    let (scheme, default_port) = if base.https {
        ("https", 443)
    } else {
        ("http", 80)
    };
    let host = base.host.to_ascii_lowercase();
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    Ok(match base.port.filter(|p| *p != default_port) {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    })
}

impl OpenAiHttpProvider {
    /// Refuses unless the provider is enabled with consent and `base_url`
    /// and a model are set. `model` (this run only) replaces the configured
    /// one; either must pass [`validate_model_name`]. An `https` server
    /// needs a successful initial trust probe; without it this fails with
    /// `trust_store_unavailable` before anything connects. Native evaluation
    /// errors arrive later as sanitized transfer errors, without fallback.
    pub fn new(
        entry: &ProviderEntry,
        config: &ProviderConfig,
        api_key: Option<String>,
        model: Option<&str>,
        execution: &ExecutionContext,
    ) -> Result<Self> {
        check_consent(entry, config)?;
        if entry.runtime != ProviderRuntime::OpenaiCompatibleHttp {
            return Err(Error::InvalidInput(format!(
                "provider {} is not an HTTP provider",
                entry.id
            )));
        }
        entry.require_llm()?;
        let endpoint = entry.endpoint.as_deref().unwrap_or_default();
        let base = base_url(entry, config)?;
        let model = match model {
            Some(m) => m.to_owned(),
            None => config
                .model
                .clone()
                .filter(|m| !m.trim().is_empty())
                .ok_or_else(|| Error::InvalidInput(format!("provider {} needs model", entry.id)))?,
        };
        validate_model_name(&model)?;
        let url = endpoint_url(base, endpoint)?;
        let agent = execution.http_agent(&url)?.clone();
        Ok(Self {
            id: entry.id.clone(),
            url,
            model,
            api_key: api_key.filter(|k| !k.is_empty()),
            agent,
        })
    }

    fn post(&self, body: &Value, timeout: Duration) -> Result<std::result::Result<String, u16>> {
        let body = serde_json::to_string(body)
            .map_err(|e| Error::Internal(format!("serialising the request: {e}")))?;
        let mut req = self
            .agent
            .post(&self.url)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("Content-Type", "application/json");
        if let Some(key) = &self.api_key {
            req = req.header("Authorization", &format!("Bearer {key}"));
        }
        read_reply(&self.id, req.send(body))
    }
}

/// The configured `base_url`, or an error naming the provider.
fn base_url<'c>(entry: &ProviderEntry, config: &'c ProviderConfig) -> Result<&'c str> {
    config
        .base_url
        .as_deref()
        .filter(|b| !b.trim().is_empty())
        .ok_or_else(|| Error::InvalidInput(format!("provider {} needs base_url", entry.id)))
}

/// The body of a 2xx reply (at most 32 MiB), or the status of another one.
/// Transport errors name the provider only: ureq messages may carry the URL.
fn read_reply(
    id: &str,
    sent: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<std::result::Result<String, u16>> {
    let mut resp =
        sent.map_err(|e| Error::Inference(format!("{id}: request failed ({})", error_kind(&e))))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Ok(Err(status));
    }
    let text = resp
        .body_mut()
        .with_config()
        .limit(32 << 20)
        .read_to_string()
        .map_err(|e| {
            Error::Inference(format!(
                "{id}: reading the reply failed ({})",
                error_kind(&e)
            ))
        })?;
    Ok(Ok(text))
}

/// `GET {base_url}/models` of an OpenAI-compatible provider with its stored
/// key: the reply text. The caller checked consent; nothing about meetings
/// is sent.
pub(super) fn get_models(
    entry: &ProviderEntry,
    config: &ProviderConfig,
    api_key: Option<String>,
    timeout: Duration,
    execution: &ExecutionContext,
) -> Result<String> {
    let url = endpoint_url(base_url(entry, config)?, "models")?;
    let mut req = execution
        .http_agent(&url)?
        .get(&url)
        .config()
        .timeout_global(Some(timeout))
        .build();
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        req = req.header("Authorization", &format!("Bearer {key}"));
    }
    match read_reply(&entry.id, req.call())? {
        Ok(text) => Ok(text),
        Err(status) => Err(Error::Inference(format!("{}: HTTP {status}", entry.id))),
    }
}

fn error_kind(e: &ureq::Error) -> &'static str {
    match e {
        ureq::Error::Timeout(_) => "timed out",
        ureq::Error::Io(_) => "connection error",
        ureq::Error::HostNotFound => "host not found",
        ureq::Error::BodyExceedsLimit(_) => "reply too large",
        _ => "HTTP error",
    }
}

/// Request body; `structured` asks for `response_format: json_schema`.
pub fn request_body(model: &str, prompt: &str, schema: &Value, structured: bool) -> Value {
    if structured {
        json!({
            "model": model,
            "temperature": 0,
            "messages": [{"role": "user", "content": prompt}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "broscribe_reply", "strict": true, "schema": schema}
            }
        })
    } else {
        let content = format!(
            "{prompt}\n\nReply with one JSON object only, matching this JSON Schema:\n{schema}"
        );
        json!({
            "model": model,
            "temperature": 0,
            "messages": [{"role": "user", "content": content}],
            "response_format": {"type": "json_object"}
        })
    }
}

/// The JSON reply in `choices[0].message.content`, with the response's
/// top-level `model` when it is a valid model name.
pub fn reply_content(id: &str, text: &str) -> Result<Completion> {
    let body: Value = serde_json::from_str(text)
        .map_err(|_| Error::Schema(format!("{id}: the server reply is not JSON")))?;
    let content = body
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Schema(format!("{id}: the server reply has no message content")))?;
    let value = parse_json_text(content)
        .ok_or_else(|| Error::Schema(format!("{id}: the reply is not JSON")))?;
    let reported_model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| validate_model_name(m).is_ok())
        .map(str::to_owned);
    Ok(Completion {
        value,
        reported_model,
    })
}

impl LlmProvider for OpenAiHttpProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn model(&self) -> Option<&str> {
        Some(&self.model)
    }

    fn complete_json(&self, prompt: &str, schema: &Value, timeout: Duration) -> Result<Completion> {
        let started = std::time::Instant::now();
        let text = match self.post(&request_body(&self.model, prompt, schema, true), timeout)? {
            Ok(text) => text,
            // Servers without structured outputs reject `json_schema`.
            Err(400 | 422) => {
                let left = timeout.saturating_sub(started.elapsed());
                match self.post(&request_body(&self.model, prompt, schema, false), left)? {
                    Ok(text) => text,
                    Err(status) => {
                        return Err(Error::Inference(format!("{}: HTTP {status}", self.id)));
                    }
                }
            }
            Err(status) => return Err(Error::Inference(format!("{}: HTTP {status}", self.id))),
        };
        let reply = reply_content(&self.id, &text)?;
        check_schema(&reply.value, schema)
            .map_err(|e| Error::Schema(format!("{}: {e}", self.id)))?;
        Ok(reply)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    use super::*;
    use crate::llm::registry::ProviderRegistry;

    #[test]
    fn urls() {
        assert_eq!(
            endpoint_url("https://api.example.org/v1/", "chat/completions").unwrap(),
            "https://api.example.org/v1/chat/completions"
        );
        assert!(endpoint_url("http://localhost:11434/v1", "chat/completions").is_ok());
        assert!(endpoint_url("http://127.0.0.1:1234/v1", "x").is_ok());
        assert!(endpoint_url("http://[::1]:8080/v1", "x").is_ok());
        assert!(endpoint_url("http://example.org/v1", "x").is_err());
        assert!(endpoint_url("https://user:pw@example.org/v1", "x").is_err());
        assert!(endpoint_url("file:///etc", "x").is_err());
        assert!(endpoint_url("example.org", "x").is_err());
    }

    #[test]
    fn origins_are_normalised() {
        for (base, origin) in [
            ("https://API.Example.org/v1", "https://api.example.org"),
            ("HTTPS://api.example.org:443/v1/", "https://api.example.org"),
            (
                "https://api.example.org:8443/x",
                "https://api.example.org:8443",
            ),
            ("http://localhost:11434/v1", "http://localhost:11434"),
            ("http://127.0.0.1:80/v1", "http://127.0.0.1"),
            ("http://[::1]:8080/v1", "http://[::1]:8080"),
        ] {
            assert_eq!(normalized_origin(base).unwrap(), origin, "{base}");
        }
        assert_ne!(
            normalized_origin("https://a.example.org/v1").unwrap(),
            normalized_origin("https://b.example.org/v1").unwrap()
        );
        assert!(normalized_origin("https://example.org:notaport/v1").is_err());
        assert!(endpoint_url("https://example.org:99999/v1", "x").is_err());
    }

    #[test]
    fn secret_key_name() {
        assert_eq!(
            api_key_secret("openai-http"),
            "provider/openai-http/api_key"
        );
    }

    fn ctx() -> ExecutionContext {
        crate::execution::test_context().1
    }

    #[test]
    fn needs_consent_base_url_and_model() {
        let reg = ProviderRegistry::builtin().unwrap();
        let e = reg.get("openai-http").unwrap();
        let x = ctx();
        let mut cfg = ProviderConfig {
            enabled: true,
            consented_at: None,
            model: Some("m".into()),
            base_url: Some("http://localhost:1/v1".into()),
        };
        assert!(OpenAiHttpProvider::new(e, &cfg, None, None, &x).is_err());
        cfg.consented_at = Some(broscribe_core::schema::now());
        assert!(OpenAiHttpProvider::new(e, &cfg, None, None, &x).is_ok());
        cfg.model = None;
        assert!(OpenAiHttpProvider::new(e, &cfg, None, None, &x).is_err());
        // A model chosen for the run is enough, and it is validated.
        let p = OpenAiHttpProvider::new(e, &cfg, None, Some("chosen-1"), &x).unwrap();
        assert_eq!(p.model(), Some("chosen-1"));
        for bad in ["", "-x", "a b", "a\nb"] {
            let err = OpenAiHttpProvider::new(e, &cfg, None, Some(bad), &x).unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{bad:?}");
        }
    }

    #[test]
    fn https_needs_the_os_trust_store_and_loopback_does_not() {
        use broscribe_platform::fakes::{FakeProcessTreeControl, FakeTrustStore};
        let reg = ProviderRegistry::builtin().unwrap();
        let e = reg.get("openai-http").unwrap();
        let cfg = |base: &str| ProviderConfig {
            enabled: true,
            consented_at: Some(broscribe_core::schema::now()),
            model: Some("m".into()),
            base_url: Some(base.into()),
        };
        let unavailable = ctx();
        let err =
            OpenAiHttpProvider::new(e, &cfg("https://127.0.0.1:1/v1"), None, None, &unavailable)
                .unwrap_err();
        assert_eq!(err.code(), crate::TRUST_STORE_UNAVAILABLE, "{err}");
        assert!(
            OpenAiHttpProvider::new(e, &cfg("http://127.0.0.1:1/v1"), None, None, &unavailable)
                .is_ok(),
            "a local server needs no trust store"
        );
        let trust = std::sync::Arc::new(FakeTrustStore::default());
        *trust.policy.lock().unwrap() = Some(broscribe_platform::TlsTrust::Certificates(vec![
            vec![0x30, 0x03, 0x02, 0x01, 0x01],
        ]));
        let system = ExecutionContext::new(
            std::sync::Arc::new(FakeProcessTreeControl::default()),
            trust,
        );
        assert!(
            OpenAiHttpProvider::new(e, &cfg("https://api.example.org/v1"), None, None, &system)
                .is_ok()
        );
    }

    #[test]
    fn configured_model_names_are_validated_like_overrides() {
        let reg = ProviderRegistry::builtin().unwrap();
        let e = reg.get("openai-http").unwrap();
        for bad in ["-x", "a b", "a\nb", "--help"] {
            let cfg = ProviderConfig {
                enabled: true,
                consented_at: Some(broscribe_core::schema::now()),
                model: Some(bad.into()),
                base_url: Some("http://localhost:1/v1".into()),
            };
            let err = OpenAiHttpProvider::new(e, &cfg, None, None, &ctx()).unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{bad:?}");
        }
    }

    #[test]
    fn chosen_model_replaces_the_configured_one() {
        let reg = ProviderRegistry::builtin().unwrap();
        let cfg = ProviderConfig {
            enabled: true,
            consented_at: Some(broscribe_core::schema::now()),
            model: Some("configured".into()),
            base_url: Some("http://localhost:1/v1".into()),
        };
        let e = reg.get("openai-http").unwrap();
        let x = ctx();
        let p = OpenAiHttpProvider::new(e, &cfg, None, Some("chosen"), &x).unwrap();
        assert_eq!(p.model(), Some("chosen"));
        let p = OpenAiHttpProvider::new(e, &cfg, None, None, &x).unwrap();
        assert_eq!(p.model(), Some("configured"));
    }

    #[test]
    fn debug_redacts_key() {
        let reg = ProviderRegistry::builtin().unwrap();
        let cfg = ProviderConfig {
            enabled: true,
            consented_at: Some(broscribe_core::schema::now()),
            model: Some("m".into()),
            base_url: Some("http://localhost:1/v1".into()),
        };
        let p = OpenAiHttpProvider::new(
            reg.get("openai-http").unwrap(),
            &cfg,
            Some("sk-test-123".into()),
            None,
            &ctx(),
        )
        .unwrap();
        assert!(!format!("{p:?}").contains("sk-test-123"));
    }

    /// One-request HTTP server on loopback answering `status` + `body`;
    /// returns the request it received.
    pub(crate) fn serve_once(
        status: u16,
        body: String,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut req_body = vec![0u8; len];
            reader.read_exact(&mut req_body).unwrap();
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            head + &String::from_utf8(req_body).unwrap()
        });
        (format!("http://127.0.0.1:{port}/v1"), handle)
    }

    #[test]
    fn structured_request_and_validation() {
        let content = serde_json::to_string(&json!({"summary": "ok"})).unwrap();
        let reply = json!({"choices": [{"message": {"role": "assistant", "content": content}}]});
        let (base, server) = serve_once(200, reply.to_string());
        let reg = ProviderRegistry::builtin().unwrap();
        let cfg = ProviderConfig {
            enabled: true,
            consented_at: Some(broscribe_core::schema::now()),
            model: Some("local-model".into()),
            base_url: Some(base),
        };
        let p = OpenAiHttpProvider::new(
            reg.get("openai-http").unwrap(),
            &cfg,
            Some("sk-test".into()),
            None,
            &ctx(),
        )
        .unwrap();
        let schema = json!({"type":"object","required":["summary"],"properties":{"summary":{"type":"string"}}});
        let out = p
            .complete_json("synthetic prompt", &schema, Duration::from_secs(10))
            .unwrap();
        assert_eq!(out.value, json!({"summary": "ok"}));
        assert_eq!(out.reported_model, None, "the response named no model");
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /v1/chat/completions"));
        assert!(
            request.contains("authorization: Bearer sk-test")
                || request.contains("Authorization: Bearer sk-test")
        );
        assert!(request.contains("\"json_schema\""));
    }

    #[test]
    fn response_model_is_reported() {
        let content = serde_json::to_string(&json!({"summary": "ok"})).unwrap();
        let reply = json!({
            "model": "local-model-2026-11",
            "choices": [{"message": {"role": "assistant", "content": content}}]
        });
        let (base, server) = serve_once(200, reply.to_string());
        let reg = ProviderRegistry::builtin().unwrap();
        let cfg = ProviderConfig {
            enabled: true,
            consented_at: Some(broscribe_core::schema::now()),
            model: Some("local-model".into()),
            base_url: Some(base),
        };
        let p = OpenAiHttpProvider::new(reg.get("openai-http").unwrap(), &cfg, None, None, &ctx())
            .unwrap();
        let schema = json!({"type":"object","required":["summary"],"properties":{"summary":{"type":"string"}}});
        let out = p
            .complete_json("synthetic prompt", &schema, Duration::from_secs(10))
            .unwrap();
        server.join().unwrap();
        assert_eq!(out.reported_model.as_deref(), Some("local-model-2026-11"));
        assert_eq!(p.model(), Some("local-model"));

        // Invalid or missing names are ignored.
        let body = |model: Value| {
            json!({"model": model, "choices": [{"message": {"content": content}}]}).to_string()
        };
        for bad in [json!("two words"), json!(""), json!(7), json!("-x")] {
            let out = reply_content("t", &body(bad.clone())).unwrap();
            assert_eq!(out.reported_model, None, "{bad}");
        }
    }
}
