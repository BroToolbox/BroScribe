//! Live model lists (design §8.4 "Live model lists"): each provider lists
//! its models itself, as its `[provider.models]` registry entry says. Agent
//! CLIs run their own listing command like detection does (no stdin,
//! allow-listed environment, private temp dir, 30 s); OpenAI-compatible
//! servers answer `GET {base_url}/models` with the stored key. Nothing about
//! meetings is sent. Vendor formats are *verify* per release: a reply that
//! does not parse is an error, never a guessed list.

use std::time::Duration;

use broscribe_core::config::{ProviderConfig, validate_model_name};
use broscribe_core::{Error, Result};
use serde::Serialize;
use serde_json::Value;

use super::agent_cli::check_consent;
use super::detect::{SearchPaths, probe};
use super::http::get_models;
use super::registry::{ModelListFormat, ProviderEntry};
use crate::execution::ExecutionContext;

/// Time allowed for `GET /models` (agent CLIs use the 30 s probe timeout).
const HTTP_LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Most models kept from one list.
const MAX_MODELS: usize = 500;

/// Where a list came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    /// The CLI's own listing command.
    Cli,
    /// The server's `/models` endpoint.
    Api,
    /// Names the CLI's help text gives for its model flag.
    Help,
    /// The provider has no way to list (no `[provider.models]`).
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ListedModel {
    /// What `--model` / `"model"` takes.
    pub id: String,
    /// Display name, when the provider gives one.
    pub name: Option<String>,
    /// The provider marks it as its default.
    pub default: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelList {
    pub source: ModelSource,
    pub models: Vec<ListedModel>,
}

/// Lists `entry`'s models. Refuses providers that are not enabled with
/// consent (the caller checks first and reports `consent_required`) and
/// non-LLM providers; `api_key` is the stored key of an HTTP provider. A
/// listing command runs as a contained tree and a `/models` request uses the
/// context's trust (HTTPS refused before connecting without OS roots).
pub fn list_models(
    entry: &ProviderEntry,
    config: &ProviderConfig,
    search: &SearchPaths,
    api_key: Option<String>,
    execution: &ExecutionContext,
) -> Result<ModelList> {
    check_consent(entry, config)?;
    entry.require_llm()?;
    let Some(listing) = &entry.models else {
        return Ok(ModelList {
            source: ModelSource::None,
            models: Vec::new(),
        });
    };
    let id = &entry.id;
    let parse_err = |why: String| Error::Schema(format!("{id}: model list: {why}"));
    if listing.format == ModelListFormat::OpenaiModels {
        let text = get_models(entry, config, api_key, HTTP_LIST_TIMEOUT, execution)?;
        let models = parse_openai_models(&text).map_err(parse_err)?;
        return Ok(ModelList {
            source: ModelSource::Api,
            models,
        });
    }
    let binary = entry.binary.as_deref().unwrap_or_default();
    let path = search
        .find(binary)
        .ok_or_else(|| Error::NotFound(format!("{id}: `{binary}` was not found")))?;
    let text = probe(&path, &listing.args, search, execution)
        .map_err(|why| Error::Inference(format!("{id}: listing models: {why}")))?;
    let (source, models) = match listing.format {
        ModelListFormat::CodexJson => (ModelSource::Cli, parse_codex_json(&text)),
        ModelListFormat::MarkedLines => (ModelSource::Cli, parse_marked_lines(&text)),
        ModelListFormat::HelpAliases => {
            let flag = entry.model_args.first().map(String::as_str).unwrap_or("");
            (ModelSource::Help, parse_help_aliases(&text, flag))
        }
        ModelListFormat::OpenaiModels => unreachable!("handled above"),
    };
    Ok(ModelList {
        source,
        models: models.map_err(parse_err)?,
    })
}

/// Keeps valid, distinct IDs (in order, at most [`MAX_MODELS`]); an empty
/// result is an error.
fn finish(
    found: impl IntoIterator<Item = ListedModel>,
) -> std::result::Result<Vec<ListedModel>, String> {
    let mut out: Vec<ListedModel> = Vec::new();
    for m in found {
        if validate_model_name(&m.id).is_err() || out.iter().any(|o| o.id == m.id) {
            continue;
        }
        out.push(m);
        if out.len() == MAX_MODELS {
            break;
        }
    }
    if out.is_empty() {
        Err("no model names found".into())
    } else {
        Ok(out)
    }
}

/// `{"models": [{"slug": "…", "display_name": "…"?, "visibility": "…"?}, …]}`
/// (Codex `debug models`). Entries Codex itself marks `"visibility": "hide"`
/// are not offered; entries without the field are.
pub fn parse_codex_json(text: &str) -> std::result::Result<Vec<ListedModel>, String> {
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| "not JSON".to_owned())?;
    let models = v
        .get("models")
        .and_then(Value::as_array)
        .ok_or("no `models` array")?;
    finish(models.iter().filter_map(|m| {
        if m.get("visibility").and_then(Value::as_str) == Some("hide") {
            return None;
        }
        Some(ListedModel {
            id: m.get("slug")?.as_str()?.to_owned(),
            name: m
                .get("display_name")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())
                .map(str::to_owned),
            default: false,
        })
    }))
}

/// Lines `* id (default)` / `- id` (Grok `models`); other lines are ignored.
pub fn parse_marked_lines(text: &str) -> std::result::Result<Vec<ListedModel>, String> {
    finish(text.lines().filter_map(|line| {
        let rest = line
            .trim()
            .strip_prefix("* ")
            .or_else(|| line.trim().strip_prefix("- "))?;
        let mut words = rest.split_whitespace();
        let id = words.next()?.to_owned();
        let default = words.any(|w| w == "(default)");
        Some(ListedModel {
            id,
            name: None,
            default,
        })
    }))
}

/// Quoted names (`'…'`, `"…"` or `` `…` ``) in the description of `flag` in
/// a help text: the option's own line and its continuation lines, up to the
/// next option or a blank line.
pub fn parse_help_aliases(help: &str, flag: &str) -> std::result::Result<Vec<ListedModel>, String> {
    if flag.is_empty() {
        return Err("no model flag to look up".into());
    }
    let mut lines = help.lines();
    let mut description = String::new();
    let mut found = false;
    for line in lines.by_ref() {
        if names_option(line, flag) {
            description.push_str(line);
            found = true;
            break;
        }
    }
    if !found {
        return Err(format!("the help does not describe {flag}"));
    }
    for line in lines {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with('-') {
            break;
        }
        description.push(' ');
        description.push_str(t);
    }
    let mut names = Vec::new();
    let mut rest = description.as_str();
    while let Some(start) = rest.find(['\'', '"', '`']) {
        let quote = rest[start..].chars().next().unwrap_or('\'');
        let after = &rest[start + 1..];
        let Some(end) = after.find(quote) else {
            break;
        };
        names.push(after[..end].to_owned());
        rest = &after[end + 1..];
    }
    finish(names.into_iter().map(|id| ListedModel {
        id,
        name: None,
        default: false,
    }))
}

/// Whether `line` is the help entry of option `flag` (`--model <m>`,
/// `-m, --model <m>`, `--model=<m>`), not a mention in another description.
fn names_option(line: &str, flag: &str) -> bool {
    for token in line.split_whitespace() {
        if !token.starts_with('-') {
            return false;
        }
        let name = token.trim_end_matches(',');
        let name = name.split('=').next().unwrap_or(name);
        if name == flag {
            return true;
        }
    }
    false
}

/// `{"data": [{"id": "…"}, …]}` (OpenAI-compatible `GET /models`).
pub fn parse_openai_models(text: &str) -> std::result::Result<Vec<ListedModel>, String> {
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| "not JSON".to_owned())?;
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or("no `data` array")?;
    finish(data.iter().filter_map(|m| {
        Some(ListedModel {
            id: m.get("id")?.as_str()?.to_owned(),
            name: None,
            default: false,
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::registry::ProviderRegistry;

    fn ids(models: &[ListedModel]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn codex_json() {
        let text = r#"{"models":[
            {"slug":"alpha-5","display_name":"Alpha 5","extra":1},
            {"slug":"alpha-5-mini"},
            {"slug":"bad name"},
            {"display_name":"no slug"},
            {"slug":"alpha-5"},
            {"slug":"hidden-1","visibility":"hide"},
            {"slug":"listed-1","visibility":"list"}
        ]}"#;
        let models = parse_codex_json(text).unwrap();
        assert_eq!(ids(&models), ["alpha-5", "alpha-5-mini", "listed-1"]);
        assert_eq!(models[0].name.as_deref(), Some("Alpha 5"));
        assert_eq!(models[1].name, None);
        assert!(models.iter().all(|m| !m.default));
        for bad in ["", "not json", r#"{"data":[]}"#, r#"{"models":[]}"#] {
            assert!(parse_codex_json(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn marked_lines() {
        let text = "Available models:\n  * nova-2 (default)\n  - nova-2-fast\n- nova-1\n\nUse -m to pick one.\n";
        let models = parse_marked_lines(text).unwrap();
        assert_eq!(ids(&models), ["nova-2", "nova-2-fast", "nova-1"]);
        assert_eq!(
            models.iter().map(|m| m.default).collect::<Vec<_>>(),
            [true, false, false]
        );
        assert!(parse_marked_lines("no models here\n").is_err());
    }

    #[test]
    fn help_aliases() {
        let help = "\
Options:
  -p, --print                 Print and exit
  --fallback-model <model>    Fallback when the --model is busy ('ignored-a')
  --model <model>             Model for the session. Provide an alias (e.g.
                              'quill' or \"sonnet-x\") or a full name
                              (e.g. `quill-2026-01-01`).
  --tools <tools...>          Tools ('not-a-model')
";
        let models = parse_help_aliases(help, "--model").unwrap();
        assert_eq!(ids(&models), ["quill", "sonnet-x", "quill-2026-01-01"]);
        let short = "  -m, --model=<m>  e.g. 'one' or 'two'\n";
        assert_eq!(
            ids(&parse_help_aliases(short, "--model").unwrap()),
            ["one", "two"]
        );
        assert!(parse_help_aliases("  --model <m>  The model.\n", "--model").is_err());
        assert!(parse_help_aliases("  --other 'x'\n", "--model").is_err());
    }

    #[test]
    fn openai_models_shape() {
        let models =
            parse_openai_models(r#"{"object":"list","data":[{"id":"m-one"},{"id":"m-two"}]}"#)
                .unwrap();
        assert_eq!(ids(&models), ["m-one", "m-two"]);
        assert!(parse_openai_models(r#"{"data":[]}"#).is_err());
        assert!(parse_openai_models("<html>").is_err());
    }

    fn consented() -> ProviderConfig {
        ProviderConfig {
            enabled: true,
            consented_at: Some(broscribe_core::schema::now()),
            ..ProviderConfig::default()
        }
    }

    #[test]
    fn openai_models_over_loopback() {
        let body = r#"{"data":[{"id":"local-a"},{"id":"local-b"}]}"#.to_owned();
        let (base, server) = crate::llm::http::tests::serve_once(200, body);
        let reg = ProviderRegistry::builtin().unwrap();
        let (_, execution) = crate::execution::test_context();
        let mut cfg = consented();
        cfg.base_url = Some(base);
        let list = list_models(
            reg.get("openai-http").unwrap(),
            &cfg,
            &SearchPaths::default(),
            Some("sk-synthetic".into()),
            &execution,
        )
        .unwrap();
        assert_eq!(list.source, ModelSource::Api);
        assert_eq!(ids(&list.models), ["local-a", "local-b"]);
        let request = server.join().unwrap();
        assert!(request.starts_with("GET /v1/models "), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-synthetic")
        );

        let (base, server) = crate::llm::http::tests::serve_once(401, "{}".into());
        cfg.base_url = Some(base);
        let err = list_models(
            reg.get("openai-http").unwrap(),
            &cfg,
            &SearchPaths::default(),
            None,
            &execution,
        )
        .unwrap_err();
        assert!(err.to_string().contains("HTTP 401"), "{err}");
        server.join().unwrap();

        cfg.base_url = Some("http://example.org/v1".into());
        assert!(
            list_models(
                reg.get("openai-http").unwrap(),
                &cfg,
                &SearchPaths::default(),
                None,
                &execution,
            )
            .is_err(),
            "plain http to a remote host is refused"
        );

        // No OS roots in this context: HTTPS fails before any connection.
        cfg.base_url = Some("https://127.0.0.1:1/v1".into());
        let err = list_models(
            reg.get("openai-http").unwrap(),
            &cfg,
            &SearchPaths::default(),
            None,
            &execution,
        )
        .unwrap_err();
        assert_eq!(err.code(), crate::TRUST_STORE_UNAVAILABLE, "{err}");
    }

    #[test]
    fn no_listing_is_source_none() {
        let reg = ProviderRegistry::builtin().unwrap();
        let (_, execution) = crate::execution::test_context();
        let list = list_models(
            reg.get("gemini-cli").unwrap(),
            &consented(),
            &SearchPaths::default(),
            None,
            &execution,
        )
        .unwrap();
        assert_eq!(list.source, ModelSource::None);
        assert!(list.models.is_empty());
        assert!(
            list_models(
                reg.get("openai-embeddings").unwrap(),
                &consented(),
                &SearchPaths::default(),
                None,
                &execution,
            )
            .is_err(),
            "not an LLM provider"
        );
    }

    #[cfg(unix)]
    mod cli {
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;

        use super::*;

        /// A fake `binary` in `dir/bin` that records each run's argv in
        /// `dir/calls` and prints `stdout`.
        fn fake(dir: &Path, binary: &str, stdout: &str) -> SearchPaths {
            let bin = dir.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {calls}\npwd >> {calls}\ncat <<'EOF'\n{stdout}\nEOF\n",
                calls = dir.join("calls").display()
            );
            let path = bin.join(binary);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            SearchPaths::new(vec![bin])
        }

        fn calls(dir: &Path) -> String {
            std::fs::read_to_string(dir.join("calls")).unwrap_or_default()
        }

        #[test]
        fn each_builtin_cli_lists_with_its_own_command() {
            let reg = ProviderRegistry::builtin().unwrap();
            let cases = [
                (
                    "codex-cli",
                    "codex",
                    r#"{"models":[{"slug":"c-1","display_name":"C One"}]}"#,
                    "debug models",
                    ModelSource::Cli,
                    vec!["c-1"],
                ),
                (
                    "grok-cli",
                    "grok",
                    "* g-2 (default)\n- g-1",
                    "models",
                    ModelSource::Cli,
                    vec!["g-2", "g-1"],
                ),
                (
                    "claude-cli",
                    "claude",
                    "  --model <model>  Alias (e.g. 'k-a' or 'k-b').\n  --tools <t>",
                    "--help",
                    ModelSource::Help,
                    vec!["k-a", "k-b"],
                ),
            ];
            for (id, binary, stdout, argv, source, want) in cases {
                let tmp = tempfile::tempdir().unwrap();
                let search = fake(tmp.path(), binary, stdout);
                let (tree, execution) = crate::execution::test_context();
                let list = list_models(
                    reg.get(id).unwrap(),
                    &consented(),
                    &search,
                    None,
                    &execution,
                )
                .unwrap();
                assert_eq!(list.source, source, "{id}");
                assert_eq!(ids(&list.models), want, "{id}");
                assert_eq!(tree.kills(), 1, "{id}: the listing ran as a contained tree");
                let log = calls(tmp.path());
                let mut lines = log.lines();
                assert_eq!(lines.next(), Some(argv), "{id}: one listing call");
                let cwd = lines.next().unwrap();
                assert!(cwd.contains("broscribe-llm-"), "{id}: private cwd {cwd}");
                assert_eq!(lines.next(), None, "{id}: nothing else ran");
            }
        }

        #[test]
        fn unparsable_output_is_an_error() {
            let reg = ProviderRegistry::builtin().unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let search = fake(tmp.path(), "codex", "Error: not logged in");
            let (_, execution) = crate::execution::test_context();
            let err = list_models(
                reg.get("codex-cli").unwrap(),
                &consented(),
                &search,
                None,
                &execution,
            )
            .unwrap_err();
            assert_eq!(err.code(), "schema", "{err}");
        }

        #[test]
        fn refused_without_consent_runs_nothing() {
            let reg = ProviderRegistry::builtin().unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let search = fake(tmp.path(), "grok", "* g-1");
            let (tree, execution) = crate::execution::test_context();
            for cfg in [
                ProviderConfig::default(),
                ProviderConfig {
                    enabled: true,
                    ..ProviderConfig::default()
                },
                ProviderConfig {
                    consented_at: Some(broscribe_core::schema::now()),
                    ..ProviderConfig::default()
                },
            ] {
                assert!(
                    list_models(
                        reg.get("grok-cli").unwrap(),
                        &cfg,
                        &search,
                        None,
                        &execution
                    )
                    .is_err()
                );
            }
            assert_eq!(calls(tmp.path()), "", "never executed");
            assert_eq!(tree.prepared.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }
}
