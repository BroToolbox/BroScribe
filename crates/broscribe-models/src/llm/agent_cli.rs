//! `agent-cli` runtime: an installed agent CLI in its non-interactive mode
//! under the user's own login (design §8.4). The prompt goes on stdin or in a
//! 0600 file, never in argv.

use std::path::{Path, PathBuf};
use std::time::Duration;

use broscribe_core::config::{ProviderConfig, validate_model_name};
use broscribe_core::{Error, Result};
use serde_json::Value;

use super::detect::{Detection, SearchPaths, agent_child_path, detect};
use super::json::{check_schema, parse_json_text, unwrap_envelope};
use super::process::{MAX_STDOUT, RunSpec, Sandbox, child_env, run};
use super::registry::{
    OutputSource, PromptTransport, ProviderEntry, ProviderRuntime, ReportedModel, StderrHeader,
};
use super::{Completion, LlmProvider};
use crate::execution::ExecutionContext;

const PROMPT_FILE: &str = "prompt.txt";
const SCHEMA_FILE: &str = "schema.json";
const OUTPUT_FILE: &str = "output.json";

#[derive(Debug)]
pub struct AgentCliProvider {
    entry: ProviderEntry,
    detection: Detection,
    binary: PathBuf,
    /// Added to every call's `PATH` ([`agent_child_path`]), as for the probes.
    child_path: Vec<PathBuf>,
    model: Option<String>,
    /// Contains every call's process tree.
    execution: ExecutionContext,
}

impl AgentCliProvider {
    /// Detects the CLI (probes contained by `execution`, a usable detection
    /// reused) and builds the provider; see [`Self::from_detection`].
    pub fn new(
        entry: &ProviderEntry,
        config: &ProviderConfig,
        search: &SearchPaths,
        model: Option<&str>,
        execution: &ExecutionContext,
    ) -> Result<Self> {
        check_consent(entry, config)?;
        model.map(validate_model_name).transpose()?;
        let detection = detect(entry, search, execution);
        Self::from_detection(entry, config, detection, search, model, execution)
    }

    /// Refuses unless the provider is enabled with consent (defence in depth;
    /// the pipeline checks first), its prompt transport is verified and the
    /// detection found every required and safety flag. `model` is chosen for
    /// this run and replaces the configured one; either must pass
    /// [`validate_model_name`], and a chosen one is refused when the CLI's
    /// model flag was not detected (never silently dropped). Every call runs
    /// as a tree contained by `execution`, with the CLI's folder and the
    /// dirs of `search` (the detection's) on its `PATH`.
    pub fn from_detection(
        entry: &ProviderEntry,
        config: &ProviderConfig,
        detection: Detection,
        search: &SearchPaths,
        model: Option<&str>,
        execution: &ExecutionContext,
    ) -> Result<Self> {
        check_consent(entry, config)?;
        if entry.runtime != ProviderRuntime::AgentCli {
            return Err(Error::InvalidInput(format!(
                "provider {} is not an agent CLI",
                entry.id
            )));
        }
        entry.require_llm()?;
        if entry.prompt_transport == PromptTransport::Unverified {
            return Err(Error::Unsupported(format!(
                "provider {} is refused: no verified way to pass the prompt outside the command line and turn tools off",
                entry.id
            )));
        }
        if detection.provider != entry.id {
            return Err(Error::Internal(format!(
                "detection of {} used for {}",
                detection.provider, entry.id
            )));
        }
        if !detection.status.usable() {
            return Err(Error::Unsupported(format!(
                "provider {} is refused: {}",
                entry.id,
                detection.reasons.join("; ")
            )));
        }
        let binary = detection
            .path
            .clone()
            .ok_or_else(|| Error::Internal(format!("{}: detection without a path", entry.id)))?;
        let model = match model {
            Some(m) => {
                validate_model_name(m)?;
                if entry.model_args.is_empty() || !detection.model_args_supported {
                    return Err(Error::Unsupported(format!(
                        "provider {} cannot be given a model: {}",
                        entry.id,
                        match entry.model_args.first() {
                            Some(flag) => format!("its help does not list {flag}"),
                            None => "the registry lists no model flag".to_owned(),
                        }
                    )));
                }
                Some(m.to_owned())
            }
            // A configured model the CLI cannot be told about is neither
            // passed nor recorded; one it can is checked like a chosen one.
            None => match config.model.as_deref().filter(|m| !m.trim().is_empty()) {
                Some(m) if detection.model_args_supported => {
                    validate_model_name(m)?;
                    Some(m.to_owned())
                }
                _ => None,
            },
        };
        Ok(Self {
            entry: entry.clone(),
            detection,
            child_path: agent_child_path(&binary, search),
            binary,
            model,
            execution: execution.clone(),
        })
    }

    pub fn detection(&self) -> &Detection {
        &self.detection
    }

    /// The argument list for one call; `files` gives the paths of the
    /// placeholders that name files.
    fn argv(&self, files: &CallFiles, schema_json: &str, timeout: Duration) -> Vec<String> {
        let subst = |v: &str| {
            v.replace("{schema_file}", &files.schema.to_string_lossy())
                .replace("{prompt_file}", &files.prompt.to_string_lossy())
                .replace("{output_file}", &files.output.to_string_lossy())
                .replace("{timeout_s}", &timeout.as_secs().to_string())
                .replace("{model}", self.model.as_deref().unwrap_or_default())
                .replace("{schema}", schema_json)
        };
        let mut argv: Vec<String> = self.entry.subcommand.clone();
        if let Some(sw) = &self.entry.disable_features {
            for f in &self.detection.disabled_features {
                argv.push(sw.flag.clone());
                argv.push(f.clone());
            }
        }
        for arg in &self.detection.args {
            argv.push(arg.flag.clone());
            argv.extend(arg.values.iter().map(|v| subst(v)));
        }
        if self.model.is_some() && self.detection.model_args_supported {
            argv.extend(self.entry.model_args.iter().map(|v| subst(v)));
        }
        argv.extend(self.entry.trailing_args.iter().map(|v| subst(v)));
        argv
    }

    fn call(&self, prompt: &str, schema: &Value, timeout: Duration) -> Result<Completion> {
        let id = &self.entry.id;
        let sandbox = Sandbox::new()?;
        let schema_json = serde_json::to_string(schema)
            .map_err(|e| Error::Internal(format!("serialising the schema: {e}")))?;
        let files = CallFiles {
            schema: sandbox.path().join(SCHEMA_FILE),
            prompt: sandbox.path().join(PROMPT_FILE),
            output: sandbox.path().join(OUTPUT_FILE),
        };
        let argv = self.argv(&files, &schema_json, timeout);
        let uses = |p: &Path| argv.iter().any(|a| a.contains(&*p.to_string_lossy()));
        if uses(&files.schema) {
            sandbox.write_private(SCHEMA_FILE, schema_json.as_bytes())?;
        }
        if self.entry.prompt_transport == PromptTransport::File {
            sandbox.write_private(PROMPT_FILE, prompt.as_bytes())?;
        }
        if self.entry.output == OutputSource::File {
            // Pre-created so the reply lands in a 0600 file.
            sandbox.write_private(OUTPUT_FILE, b"")?;
        }
        // The registry has no prompt placeholder; this guards user entries
        // and future edits all the same.
        // (Short prompts could match schema text by chance.)
        if prompt.trim().len() >= 32 && argv.iter().any(|a| a.contains(prompt)) {
            return Err(Error::Internal(format!(
                "{id}: refusing to put the prompt in argv"
            )));
        }
        let env = child_env(std::env::vars_os(), &self.child_path);
        let out = run(
            &RunSpec {
                program: &self.binary,
                args: &argv,
                cwd: sandbox.path(),
                env: &env,
                stdin: (self.entry.prompt_transport == PromptTransport::Stdin)
                    .then_some(prompt.as_bytes()),
                timeout,
            },
            &self.execution,
        )?;
        // stderr may echo the prompt: report only its size.
        if !out.status.success() {
            return Err(Error::Inference(format!(
                "{id} failed ({}; {} bytes on stderr)",
                out.status, out.stderr_bytes
            )));
        }
        let text = match self.entry.output {
            OutputSource::Stdout => String::from_utf8_lossy(&out.stdout).into_owned(),
            OutputSource::File => read_capped(&files.output)?,
        };
        sandbox.close()?;
        let envelope = parse_json_text(&text)
            .ok_or_else(|| Error::Schema(format!("{id}: the reply is not JSON")))?;
        if let Some(key) = &self.entry.error_flag_key
            && envelope.get(key) == Some(&Value::Bool(true))
        {
            return Err(Error::Inference(format!("{id} reported an error")));
        }
        let reported_model = match &self.entry.reported_model {
            None => None,
            Some(ReportedModel::JsonObject {
                json_object,
                rank_by,
            }) => reported_from_json(&envelope, json_object, rank_by),
            Some(ReportedModel::StderrHeader { stderr_header }) => {
                reported_from_stderr(&out.stderr_head, stderr_header)
            }
        };
        let reply = unwrap_envelope(envelope, &self.entry.output_keys)
            .map_err(|e| Error::Schema(format!("{id}: {e}")))?;
        check_schema(&reply, schema).map_err(|e| Error::Schema(format!("{id}: {e}")))?;
        Ok(Completion {
            value: reply,
            reported_model,
        })
    }
}

/// A reported name, kept only when it is a valid model name.
fn valid_name(name: &str) -> Option<String> {
    validate_model_name(name).is_ok().then(|| name.to_owned())
}

/// [`ReportedModel::JsonObject`]: the only key of `envelope[object]`, or the
/// key whose `rank_by` number is strictly the largest; otherwise `None`.
/// Never depends on the map's key order.
pub(crate) fn reported_from_json(envelope: &Value, object: &str, rank_by: &str) -> Option<String> {
    let map = envelope.get(object)?.as_object()?;
    let mut keys = map.keys();
    let name = match (keys.next(), keys.next()) {
        (Some(only), None) => only,
        (Some(_), Some(_)) => {
            let mut best: Option<(&String, f64)> = None;
            let mut tied = false;
            for (key, usage) in map {
                let rank = usage.get(rank_by)?.as_f64()?;
                match best {
                    Some((_, top)) if rank < top => {}
                    Some((_, top)) if rank == top => tied = true,
                    _ => {
                        best = Some((key, rank));
                        tied = false;
                    }
                }
            }
            if tied {
                return None;
            }
            best?.0
        }
        _ => return None,
    };
    valid_name(name)
}

/// [`ReportedModel::StderrHeader`]: the first line starting with `prefix`
/// strictly between the first two lines equal to `separator`. Anything after
/// the second separator (e.g. a prompt echo) is never read; without a second
/// separator nothing is reported.
pub(crate) fn reported_from_stderr(stderr: &str, header: &StderrHeader) -> Option<String> {
    let is_sep = |line: &str| line.trim_end() == header.separator;
    let lines = stderr.lines().skip_while(|l| !is_sep(l)).skip(1);
    let mut found: Option<&str> = None;
    for line in lines {
        if is_sep(line) {
            return found.and_then(valid_name);
        }
        if found.is_none() {
            found = line.strip_prefix(header.prefix.as_str()).map(str::trim);
        }
    }
    None
}

struct CallFiles {
    schema: PathBuf,
    prompt: PathBuf,
    output: PathBuf,
}

/// Reads the reply file, refusing one over the stdout cap rather than
/// truncating it.
fn read_capped(path: &Path) -> Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| Error::io_at(path, e))?;
    let too_big = || {
        Error::Inference(format!(
            "the reply file is larger than {} MiB",
            MAX_STDOUT >> 20
        ))
    };
    let len = file.metadata().map_err(|e| Error::io_at(path, e))?.len();
    if len > MAX_STDOUT as u64 {
        return Err(too_big());
    }
    let mut text = String::new();
    // The size can change after the check; never hold more than the cap.
    file.take(MAX_STDOUT as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|e| Error::io_at(path, e))?;
    if text.len() > MAX_STDOUT {
        return Err(too_big());
    }
    Ok(text)
}

/// Enabled with a recorded consent, or refused.
pub fn check_consent(entry: &ProviderEntry, config: &ProviderConfig) -> Result<()> {
    if config.enabled && config.consented_at.is_some() {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "provider {} is not enabled; enable it and accept its consent text first",
            entry.id
        )))
    }
}

impl LlmProvider for AgentCliProvider {
    fn id(&self) -> &str {
        &self.entry.id
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn complete_json(&self, prompt: &str, schema: &Value, timeout: Duration) -> Result<Completion> {
        self.call(prompt, schema, timeout)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;

    use super::*;
    use crate::llm::detect::Availability;
    use crate::llm::registry::ProviderRegistry;

    const SECRET: &str = "Zebulon said the launch moves to Thursday";

    fn consented() -> ProviderConfig {
        ProviderConfig {
            enabled: true,
            consented_at: Some(broscribe_core::schema::now()),
            ..ProviderConfig::default()
        }
    }

    fn ctx() -> ExecutionContext {
        crate::execution::test_context().1
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["summary"],
            "properties": {"summary": {"type": "string"}}
        })
    }

    /// A fake CLI in `dir/bin/<name>`: prints its help for `--help`, records
    /// argv, env and stdin under `dir/log`, then runs `body`.
    fn fake_cli(dir: &Path, name: &str, help: &str, body: &str) -> SearchPaths {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(dir.join("log")).unwrap();
        let log = dir.join("log");
        let script = format!(
            "#!/bin/sh\n\
             case \"$1\" in --version) echo '{name} 9.9.9'; exit 0;; esac\n\
             for a in \"$@\"; do case \"$a\" in --help) cat <<'EOF'\n{help}\nEOF\n exit 0;; esac; done\n\
             printf '%s\\n' \"$@\" > {log}/argv\n\
             env > {log}/env\n\
             pwd > {log}/cwd\n\
             cat > {log}/stdin\n\
             {body}\n",
            log = log.display()
        );
        let path = bin.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        SearchPaths::new(vec![bin, "/usr/bin".into(), "/bin".into()])
    }

    fn entry(text: &str) -> ProviderEntry {
        ProviderRegistry::parse(text, "test")
            .unwrap()
            .providers
            .remove(0)
    }

    const STDIN_ENTRY: &str = r#"
[[provider]]
id = "fake-cli"
name = "Fake"
vendor = "Nobody"
runtime = "agent-cli"
binary = "fakecli"
prompt_transport = "stdin"
output_keys = ["structured_output", "result"]
error_flag_key = "is_error"
model_args = ["--model", "{model}"]
args = [
  { flag = "-p", need = "required" },
  { flag = "--json-schema", values = ["{schema}"], need = "required" },
  { flag = "--tools", values = [""], need = "safety" },
  { flag = "--fancy", need = "optional" },
]
"#;
    const STDIN_HELP: &str = "  -p, --print\n  --json-schema <s>\n  --tools <t>\n  --model <m>";

    #[test]
    fn transcript_never_in_argv_stdin_transport() {
        let tmp = tempfile::tempdir().unwrap();
        let search = fake_cli(
            tmp.path(),
            "fakecli",
            STDIN_HELP,
            r#"echo '{"type":"result","is_error":false,"structured_output":{"summary":"ok"}}'"#,
        );
        let e = entry(STDIN_ENTRY);
        let mut cfg = consented();
        cfg.model = Some("m1".into());
        let p = AgentCliProvider::new(&e, &cfg, &search, None, &ctx()).unwrap();
        assert_eq!(p.detection().status, Availability::Degraded);
        assert_eq!(p.detection().not_applied, vec!["--fancy"]);
        let prompt = format!("<transcript>\n[00:00:01] S1: {SECRET}\n</transcript>");
        let out = p
            .complete_json(&prompt, &schema(), Duration::from_secs(20))
            .unwrap();
        assert_eq!(out.value, json!({"summary": "ok"}));
        assert_eq!(out.reported_model, None, "no reported_model source");

        let log = tmp.path().join("log");
        let argv = std::fs::read_to_string(log.join("argv")).unwrap();
        assert!(!argv.contains("Zebulon"), "transcript leaked into argv");
        assert!(
            argv.contains("--tools\n\n"),
            "empty tools value passed: {argv:?}"
        );
        assert!(argv.contains("--model\nm1\n"));
        assert!(!argv.contains("--fancy"));
        assert_eq!(std::fs::read_to_string(log.join("stdin")).unwrap(), prompt);
        // cwd was a fresh private temp dir, removed afterwards.
        let cwd = std::fs::read_to_string(log.join("cwd")).unwrap();
        assert!(cwd.contains("broscribe-llm-"), "{cwd}");
        assert!(!Path::new(cwd.trim()).exists());
    }

    #[test]
    fn env_is_allow_listed_for_the_child() {
        let tmp = tempfile::tempdir().unwrap();
        let search = fake_cli(
            tmp.path(),
            "fakecli",
            STDIN_HELP,
            r#"echo '{"result":"{\"summary\":\"ok\"}"}'"#,
        );
        let p = AgentCliProvider::new(&entry(STDIN_ENTRY), &consented(), &search, None, &ctx())
            .unwrap();
        p.complete_json("hi", &schema(), Duration::from_secs(20))
            .unwrap();
        let env = std::fs::read_to_string(tmp.path().join("log/env")).unwrap();
        for line in env.lines() {
            let name = line.split('=').next().unwrap();
            assert!(
                crate::llm::process::ENV_ALLOW.contains(&name)
                    || name.starts_with("XDG_")
                    // set by /bin/sh itself
                    || ["PWD", "SHLVL", "_", "OLDPWD"].contains(&name),
                "{name} passed to the child"
            );
        }
    }

    #[test]
    fn file_transport_uses_private_file() {
        let tmp = tempfile::tempdir().unwrap();
        let e = entry(
            r#"
[[provider]]
id = "fake-cli"
name = "Fake"
vendor = "Nobody"
runtime = "agent-cli"
binary = "fakecli"
prompt_transport = "file"
output = "file"
args = [
  { flag = "--prompt-file", values = ["{prompt_file}"], need = "required" },
  { flag = "-o", values = ["{output_file}"], need = "required" },
  { flag = "--no-tools", need = "safety" },
]
"#,
        );
        // Records the prompt file's mode and contents, writes the reply file.
        let body = r#"
pf=""; of=""
while [ $# -gt 0 ]; do case "$1" in --prompt-file) pf="$2"; shift;; -o) of="$2"; shift;; esac; shift; done
ls -l "$pf" | cut -c1-10 > LOGDIR/mode
cp "$pf" LOGDIR/prompt
printf '{"summary":"ok"}' > "$of"
"#
        .replace("LOGDIR", &tmp.path().join("log").display().to_string());
        let search = fake_cli(
            tmp.path(),
            "fakecli",
            "  --prompt-file <f>\n  -o <f>\n  --no-tools",
            &body,
        );
        let p = AgentCliProvider::new(&e, &consented(), &search, None, &ctx()).unwrap();
        assert_eq!(p.detection().status, Availability::Ready);
        let out = p
            .complete_json(SECRET, &schema(), Duration::from_secs(20))
            .unwrap();
        assert_eq!(out.value, json!({"summary": "ok"}));
        let log = tmp.path().join("log");
        assert!(
            !std::fs::read_to_string(log.join("argv"))
                .unwrap()
                .contains("Zebulon")
        );
        assert_eq!(std::fs::read_to_string(log.join("prompt")).unwrap(), SECRET);
        assert_eq!(
            std::fs::read_to_string(log.join("mode")).unwrap().trim(),
            "-rw-------"
        );
        assert_eq!(std::fs::read_to_string(log.join("stdin")).unwrap(), "");
    }

    #[test]
    fn missing_safety_flag_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let search = fake_cli(tmp.path(), "fakecli", "  -p\n  --json-schema <s>", "true");
        let err = AgentCliProvider::new(&entry(STDIN_ENTRY), &consented(), &search, None, &ctx())
            .unwrap_err();
        assert!(err.to_string().contains("safety flag --tools"), "{err}");
    }

    #[test]
    fn unverified_transport_refused_even_when_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let search = fake_cli(tmp.path(), "agy", "  --print\n  --sandbox", "true");
        let reg = ProviderRegistry::builtin().unwrap();
        let agy = reg.get("agy-cli").unwrap();
        let x = ctx();
        let d = detect(agy, &search, &x);
        assert_eq!(d.status, Availability::Refused);
        assert_eq!(d.version.as_deref(), Some("agy 9.9.9"));
        let err = AgentCliProvider::new(agy, &consented(), &search, None, &x).unwrap_err();
        assert!(err.to_string().contains("refused"), "{err}");
        assert!(
            !tmp.path().join("log/argv").exists(),
            "never invoked with a prompt"
        );
    }

    #[test]
    fn consent_required() {
        let tmp = tempfile::tempdir().unwrap();
        let search = fake_cli(tmp.path(), "fakecli", STDIN_HELP, "true");
        let e = entry(STDIN_ENTRY);
        let not_consented = ProviderConfig {
            enabled: true,
            ..ProviderConfig::default()
        };
        assert!(AgentCliProvider::new(&e, &not_consented, &search, None, &ctx()).is_err());
        let disabled = ProviderConfig {
            enabled: false,
            consented_at: Some(broscribe_core::schema::now()),
            ..ProviderConfig::default()
        };
        assert!(AgentCliProvider::new(&e, &disabled, &search, None, &ctx()).is_err());
    }

    #[test]
    fn invalid_reply_and_failures_are_errors_without_content() {
        let tmp = tempfile::tempdir().unwrap();
        let search = fake_cli(
            tmp.path(),
            "fakecli",
            STDIN_HELP,
            r#"echo '{"structured_output":{"summary":"x","extra":"Zebulon"}}'"#,
        );
        let p = AgentCliProvider::new(&entry(STDIN_ENTRY), &consented(), &search, None, &ctx())
            .unwrap();
        let err = p
            .complete_json(SECRET, &schema(), Duration::from_secs(20))
            .unwrap_err();
        assert_eq!(err.code(), Error::Schema(String::new()).code());
        assert!(!err.to_string().contains("Zebulon"));

        let tmp = tempfile::tempdir().unwrap();
        let search = fake_cli(
            tmp.path(),
            "fakecli",
            STDIN_HELP,
            &format!(
                "echo \"bad input: $(cat {}/log/stdin)\" >&2; exit 3",
                tmp.path().display()
            ),
        );
        let p = AgentCliProvider::new(&entry(STDIN_ENTRY), &consented(), &search, None, &ctx())
            .unwrap();
        let err = p
            .complete_json(SECRET, &schema(), Duration::from_secs(20))
            .unwrap_err();
        assert!(!err.to_string().contains("Zebulon"), "{err}");

        let tmp = tempfile::tempdir().unwrap();
        let search = fake_cli(
            tmp.path(),
            "fakecli",
            STDIN_HELP,
            r#"echo '{"is_error":true,"result":"quota"}'"#,
        );
        let p = AgentCliProvider::new(&entry(STDIN_ENTRY), &consented(), &search, None, &ctx())
            .unwrap();
        assert!(
            p.complete_json("hi", &schema(), Duration::from_secs(20))
                .is_err()
        );
    }

    #[test]
    fn timeout_kills_the_cli() {
        let tmp = tempfile::tempdir().unwrap();
        let search = fake_cli(tmp.path(), "fakecli", STDIN_HELP, "exec sleep 30");
        let (tree, execution) = crate::execution::test_context();
        let p = AgentCliProvider::new(&entry(STDIN_ENTRY), &consented(), &search, None, &execution)
            .unwrap();
        let probes = tree.kills();
        let started = std::time::Instant::now();
        let err = p
            .complete_json("hi", &schema(), Duration::from_millis(700))
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(tree.kills(), probes + 1, "the call's tree was stopped");
    }

    #[test]
    fn codex_features_and_argv_order() {
        let reg = ProviderRegistry::builtin().unwrap();
        let codex = reg.get("codex-cli").unwrap();
        let help: String = codex
            .args
            .iter()
            .map(|a| format!("  {} x\n", a.flag))
            .chain(["  --disable <FEATURE>\n  -m <MODEL>\n".to_string()])
            .collect();
        let mut d = Detection {
            provider: codex.id.clone(),
            path: Some("/usr/bin/true".into()),
            version: None,
            status: Availability::Ready,
            reasons: vec![],
            not_applied: vec![],
            args: vec![],
            model_args_supported: true,
            disabled_features: vec![],
        };
        crate::llm::detect::apply_help(codex, &help, &mut d);
        crate::llm::detect::apply_features(
            codex,
            "shell_tool stable true\napps stable true\n",
            &mut d,
        );
        let p = AgentCliProvider::from_detection(
            codex,
            &consented(),
            d,
            &SearchPaths::default(),
            None,
            &ctx(),
        )
        .unwrap();
        let files = CallFiles {
            schema: "/t/schema.json".into(),
            prompt: "/t/prompt.txt".into(),
            output: "/t/output.json".into(),
        };
        let argv = p.argv(&files, "{}", Duration::from_secs(600));
        assert_eq!(argv[0], "exec");
        assert_eq!(
            &argv[1..5],
            ["--disable", "shell_tool", "--disable", "apps"]
        );
        assert!(argv.windows(2).any(|w| w == ["--sandbox", "read-only"]));
        assert!(
            argv.windows(2)
                .any(|w| w == ["--output-schema", "/t/schema.json"])
        );
        assert!(argv.windows(2).any(|w| w == ["-o", "/t/output.json"]));
        assert_eq!(argv.last().unwrap(), "-");
    }

    fn codex_detection(help_extra: &str) -> (ProviderEntry, Detection) {
        let reg = ProviderRegistry::builtin().unwrap();
        let codex = reg.get("codex-cli").unwrap().clone();
        let help: String = codex
            .args
            .iter()
            .map(|a| format!("  {} x\n", a.flag))
            .chain([format!("  --disable <FEATURE>\n{help_extra}")])
            .collect();
        let mut d = Detection {
            provider: codex.id.clone(),
            path: Some("/usr/bin/true".into()),
            version: None,
            status: Availability::Ready,
            reasons: vec![],
            not_applied: vec![],
            args: vec![],
            model_args_supported: true,
            disabled_features: vec![],
        };
        crate::llm::detect::apply_help(&codex, &help, &mut d);
        (codex, d)
    }

    fn codex_with_help(help_extra: &str, model: &str) -> AgentCliProvider {
        let (codex, d) = codex_detection(help_extra);
        let mut cfg = consented();
        cfg.model = Some(model.into());
        AgentCliProvider::from_detection(&codex, &cfg, d, &SearchPaths::default(), None, &ctx())
            .unwrap()
    }

    #[test]
    fn chosen_model_needs_the_detected_flag() {
        let (codex, d) = codex_detection("");
        let err = AgentCliProvider::from_detection(
            &codex,
            &consented(),
            d,
            &SearchPaths::default(),
            Some("model-q7"),
            &ctx(),
        )
        .unwrap_err();
        assert_eq!(err.code(), "unsupported", "{err}");
        assert!(err.to_string().contains("-m"), "{err}");

        let (codex, d) = codex_detection("  -m <MODEL>\n");
        for bad in ["-m", "two words", "", "x\u{7}"] {
            let err = AgentCliProvider::from_detection(
                &codex,
                &consented(),
                d.clone(),
                &SearchPaths::default(),
                Some(bad),
                &ctx(),
            )
            .unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{bad:?}");
        }
        let mut no_flag = codex.clone();
        no_flag.model_args.clear();
        let err = AgentCliProvider::from_detection(
            &no_flag,
            &consented(),
            d,
            &SearchPaths::default(),
            Some("model-q7"),
            &ctx(),
        )
        .unwrap_err();
        assert_eq!(err.code(), "unsupported", "{err}");
    }

    #[test]
    fn chosen_model_reaches_argv_only_through_model_args() {
        let tmp = tempfile::tempdir().unwrap();
        let search = fake_cli(
            tmp.path(),
            "fakecli",
            STDIN_HELP,
            r#"echo '{"structured_output":{"summary":"ok"}}'"#,
        );
        let mut cfg = consented();
        cfg.model = Some("configured-m".into());
        let p = AgentCliProvider::new(&entry(STDIN_ENTRY), &cfg, &search, Some("chosen-m"), &ctx())
            .unwrap();
        assert_eq!(p.model(), Some("chosen-m"));
        p.complete_json("hi", &schema(), Duration::from_secs(20))
            .unwrap();
        let argv = std::fs::read_to_string(tmp.path().join("log/argv")).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        let at: Vec<usize> = (0..argv.len())
            .filter(|&i| argv[i].contains("chosen-m"))
            .collect();
        assert_eq!(at.len(), 1, "{argv:?}");
        assert_eq!(argv[at[0] - 1], "--model", "{argv:?}");
        assert!(!argv.iter().any(|a| a.contains("configured-m")), "{argv:?}");

        // A CLI whose help lacks the model flag is refused before any call.
        let tmp = tempfile::tempdir().unwrap();
        let help = STDIN_HELP.replace("--model <m>", "");
        let search = fake_cli(tmp.path(), "fakecli", &help, "true");
        let err =
            AgentCliProvider::new(&entry(STDIN_ENTRY), &cfg, &search, Some("chosen-m"), &ctx())
                .unwrap_err();
        assert_eq!(err.code(), "unsupported", "{err}");
        assert!(!tmp.path().join("log/argv").exists());
    }

    #[test]
    fn model_args_only_when_detection_found_the_flag() {
        let files = CallFiles {
            schema: "/t/schema.json".into(),
            prompt: "/t/prompt.txt".into(),
            output: "/t/output.json".into(),
        };
        let supported = codex_with_help("  -m <MODEL>\n", "model-q7");
        assert!(supported.detection().model_args_supported);
        let argv = supported.argv(&files, "{}", Duration::from_secs(600));
        assert!(argv.windows(2).any(|w| w == ["-m", "model-q7"]), "{argv:?}");
        assert_eq!(supported.model(), Some("model-q7"));

        let unsupported = codex_with_help("", "model-q7");
        assert!(!unsupported.detection().model_args_supported);
        assert!(
            unsupported
                .detection()
                .not_applied
                .contains(&"-m (model selection)".to_string())
        );
        let argv = unsupported.argv(&files, "{}", Duration::from_secs(600));
        assert!(
            !argv.iter().any(|a| a == "-m" || a.contains("model-q7")),
            "{argv:?}"
        );
        assert_eq!(
            unsupported.model(),
            None,
            "an unapplied model is not recorded"
        );
    }

    #[test]
    fn configured_model_is_validated_before_it_can_reach_argv() {
        let (codex, d) = codex_detection("  -m <MODEL>\n");
        for bad in ["--dangerously-bypass", "two words", "x\u{7}"] {
            let mut cfg = consented();
            cfg.model = Some(bad.into());
            let err = AgentCliProvider::from_detection(
                &codex,
                &cfg,
                d.clone(),
                &SearchPaths::default(),
                None,
                &ctx(),
            )
            .unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{bad:?}");
        }
        let mut cfg = consented();
        cfg.model = Some("model-q7".into());
        let p = AgentCliProvider::from_detection(
            &codex,
            &cfg,
            d,
            &SearchPaths::default(),
            None,
            &ctx(),
        )
        .unwrap();
        assert_eq!(p.model(), Some("model-q7"));
    }

    #[test]
    fn oversize_reply_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let small = tmp.path().join("small.json");
        std::fs::write(&small, r#"{"summary":"ok"}"#).unwrap();
        assert_eq!(read_capped(&small).unwrap(), r#"{"summary":"ok"}"#);
        let big = tmp.path().join("big.json");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_STDOUT as u64 + 1).unwrap();
        let err = read_capped(&big).unwrap_err();
        assert!(err.to_string().contains("larger than 32 MiB"), "{err}");

        // Through a call: the CLI writes a sparse reply file over the cap.
        let e = entry(
            r#"
[[provider]]
id = "fake-cli"
name = "Fake"
vendor = "Nobody"
runtime = "agent-cli"
binary = "fakecli"
prompt_transport = "stdin"
output = "file"
args = [
  { flag = "-o", values = ["{output_file}"], need = "required" },
  { flag = "--no-tools", need = "safety" },
]
"#,
        );
        let body = r#"
of=""
while [ $# -gt 0 ]; do case "$1" in -o) of="$2"; shift;; esac; shift; done
dd if=/dev/zero of="$of" bs=1 count=1 seek=33554432 2>/dev/null
"#;
        let search = fake_cli(tmp.path(), "fakecli", "  -o <f>\n  --no-tools", body);
        let p = AgentCliProvider::new(&e, &consented(), &search, None, &ctx()).unwrap();
        let err = p
            .complete_json("hi", &schema(), Duration::from_secs(20))
            .unwrap_err();
        assert!(err.to_string().contains("larger than 32 MiB"), "{err}");
    }

    #[test]
    fn json_object_source_needs_one_key_or_a_strict_maximum() {
        let get = |v: Value| reported_from_json(&v, "modelUsage", "outputTokens");
        assert_eq!(
            get(json!({"modelUsage": {"model-q7": {"outputTokens": 3}}})).as_deref(),
            Some("model-q7")
        );
        // One key needs no rank.
        assert_eq!(
            get(json!({"modelUsage": {"model-q7": {}}})).as_deref(),
            Some("model-q7")
        );
        // Several keys: the strictly largest rank, wherever it is.
        assert_eq!(
            get(json!({"modelUsage": {
                "z-model": {"outputTokens": 5},
                "a-model": {"outputTokens": 9},
            }}))
            .as_deref(),
            Some("a-model")
        );
        assert_eq!(
            get(json!({"modelUsage": {
                "z-model": {"outputTokens": 90},
                "a-model": {"outputTokens": 9},
            }}))
            .as_deref(),
            Some("z-model")
        );
        for none in [
            // No counts: never the first key in document order.
            json!({"modelUsage": {"z-model": {}, "a-model": {}}}),
            // A count missing or not a number.
            json!({"modelUsage": {"z-model": {"outputTokens": 5}, "a-model": {}}}),
            json!({"modelUsage": {"z-model": {"outputTokens": "5"}, "a-model": {"outputTokens": 1}}}),
            // A tie.
            json!({"modelUsage": {"z-model": {"outputTokens": 5}, "a-model": {"outputTokens": 5}}}),
            // Missing, empty or not an object.
            json!({"result": "ok"}),
            json!({"modelUsage": {}}),
            json!({"modelUsage": ["model-q7"]}),
            // Invalid names are ignored.
            json!({"modelUsage": {"two words": {"outputTokens": 1}}}),
            json!({"modelUsage": {"-flag": {}}}),
            json!({"modelUsage": {"bad\nname": {"outputTokens": 9}, "ok-model": {"outputTokens": 1}}}),
        ] {
            assert_eq!(get(none.clone()), None, "{none}");
        }
    }

    fn header() -> StderrHeader {
        StderrHeader {
            separator: "--------".into(),
            prefix: "model: ".into(),
        }
    }

    #[test]
    fn stderr_source_reads_only_between_the_first_two_separators() {
        let get = |s: &str| reported_from_stderr(s, &header());
        let head = "tool v1\n--------\nworkdir: /tmp/x\nmodel: model-q7\nprovider: p\n--------\nuser\nmodel: evil\n";
        assert_eq!(get(head).as_deref(), Some("model-q7"));
        assert_eq!(
            get(&head.replace('\n', "\r\n")).as_deref(),
            Some("model-q7")
        );
        // The first prefixed line wins.
        assert_eq!(
            get("--------\nmodel: first-m\nmodel: second-m\n--------\n").as_deref(),
            Some("first-m")
        );
        for none in [
            // Only in the prompt echo after the second separator.
            "--------\nworkdir: /tmp/x\n--------\nuser\nmodel: evil\n",
            // Before the first separator.
            "model: evil\n--------\nworkdir: /tmp/x\n--------\n",
            // No second separator (cut off or not a header).
            "--------\nmodel: model-q7\nworkdir: /tmp/x\n",
            "model: model-q7\n",
            "",
            // Invalid names are ignored.
            "--------\nmodel: two words\n--------\n",
            "--------\nmodel: \n--------\n",
            "--------\nmodel: -m\n--------\n",
        ] {
            assert_eq!(get(none), None, "{none:?}");
        }
    }

    #[test]
    fn json_object_source_through_a_call() {
        let tmp = tempfile::tempdir().unwrap();
        let search = fake_cli(
            tmp.path(),
            "fakecli",
            STDIN_HELP,
            r#"echo '{"modelUsage":{"small-m":{"outputTokens":2},"big-m-5":{"outputTokens":40}},"structured_output":{"summary":"ok"}}'"#,
        );
        let text = STDIN_ENTRY.replace(
            "model_args =",
            "reported_model = { json_object = \"modelUsage\", rank_by = \"outputTokens\" }\nmodel_args =",
        );
        let mut cfg = consented();
        cfg.model = Some("alias".into());
        let p = AgentCliProvider::new(&entry(&text), &cfg, &search, None, &ctx()).unwrap();
        let out = p
            .complete_json(SECRET, &schema(), Duration::from_secs(20))
            .unwrap();
        assert_eq!(out.value, json!({"summary": "ok"}));
        assert_eq!(out.reported_model.as_deref(), Some("big-m-5"));
        assert_eq!(p.model(), Some("alias"), "the request is unchanged");
    }

    #[test]
    fn stderr_source_through_a_call_ignores_the_prompt_echo() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("log");
        // Header, then the prompt echoed after the second separator.
        let body = format!(
            "printf '%s\\n' 'fake v9' -------- 'workdir: /tmp' 'model: model-q7' -------- user >&2\n\
             cat {}/stdin >&2\n\
             echo '{{\"summary\":\"ok\"}}'",
            log.display()
        );
        let search = fake_cli(tmp.path(), "fakecli", STDIN_HELP, &body);
        let text = STDIN_ENTRY
            .replace("output_keys = [\"structured_output\", \"result\"]\n", "")
            .replace(
                "model_args =",
                "reported_model = { stderr_header = { separator = \"--------\", prefix = \"model: \" } }\nmodel_args =",
            );
        let p = AgentCliProvider::new(&entry(&text), &consented(), &search, None, &ctx()).unwrap();
        let out = p
            .complete_json(
                "model: evil\n--------\nmodel: evil2",
                &schema(),
                Duration::from_secs(20),
            )
            .unwrap();
        assert_eq!(out.reported_model.as_deref(), Some("model-q7"));

        // Without a header the reply stands, unreported.
        let tmp = tempfile::tempdir().unwrap();
        let body = format!(
            "cat {}/log/stdin >&2\necho '{{\"summary\":\"ok\"}}'",
            tmp.path().display()
        );
        let search = fake_cli(tmp.path(), "fakecli", STDIN_HELP, &body);
        let p = AgentCliProvider::new(&entry(&text), &consented(), &search, None, &ctx()).unwrap();
        let out = p
            .complete_json("model: evil\n", &schema(), Duration::from_secs(20))
            .unwrap();
        assert_eq!(out.value, json!({"summary": "ok"}));
        assert_eq!(out.reported_model, None);
    }
}
