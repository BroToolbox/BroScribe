//! Agent-CLI detection (design §8.4): `PATH` plus common install locations,
//! `--version`, and a runtime `--help` check of every flag the registry lists.
//! A usable result is reused while the registry entry and the CLI binary
//! (path, size, modification time) are unchanged, so opening a provider
//! again does not re-run the probes; a changed CLI is probed afresh.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use super::process::{RunSpec, Sandbox, child_env, is_executable, run};
use super::registry::{ArgNeed, ArgSpec, PromptTransport, ProviderEntry, ProviderRuntime};
use crate::execution::ExecutionContext;

/// Time allowed for `--version`, `--help` and feature listings.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Most detections kept for reuse (registries list a handful of CLIs).
const MAX_CACHED: usize = 16;

/// A usable detection and what it was made from.
struct Cached {
    entry: ProviderEntry,
    path: PathBuf,
    identity: (u64, SystemTime),
    detection: Detection,
}

static DETECTIONS: Mutex<Vec<Cached>> = Mutex::new(Vec::new());

/// Extensions Windows starts a program by: `.com`/`.exe` directly, and
/// `.bat`/`.cmd` (npm's shims) through cmd.exe. Never an extension-less file
/// (npm's `sh` shim beside them: `os error 193`) nor `.ps1`.
const WINDOWS_LAUNCHABLE: [&str; 4] = [".COM", ".EXE", ".BAT", ".CMD"];

/// Size and modification time of the binary: a reinstall or upgrade
/// changes them. `None` (unreadable): never cached.
fn identity(path: &Path) -> Option<(u64, SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// Directories searched for agent CLIs, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchPaths {
    pub dirs: Vec<PathBuf>,
}

impl SearchPaths {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self { dirs }
    }

    /// `PATH`, then `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`,
    /// `~/.opencode/bin` and npm/bun global bins (GUI apps on macOS do not
    /// inherit the shell `PATH`). On Windows also `%APPDATA%\npm`, the npm
    /// prefix itself (Windows npm puts its shims there, not in `bin`) and
    /// `%ProgramFiles%\nodejs`, which a process started with a stale
    /// environment lacks.
    pub fn from_env() -> Self {
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        let home = std::env::home_dir();
        let mut extra: Vec<PathBuf> = Vec::new();
        if let Some(h) = &home {
            extra.push(h.join(".local/bin"));
        }
        extra.push("/opt/homebrew/bin".into());
        extra.push("/usr/local/bin".into());
        if let Some(h) = &home {
            extra.push(h.join(".opencode/bin"));
            extra.push(h.join(".npm-global/bin"));
            extra.push(h.join(".bun/bin"));
        }
        if let Some(prefix) = std::env::var_os("NPM_CONFIG_PREFIX") {
            extra.push(PathBuf::from(prefix).join("bin"));
        }
        if let Some(bun) = std::env::var_os("BUN_INSTALL") {
            extra.push(PathBuf::from(bun).join("bin"));
        }
        if cfg!(windows) {
            if let Some(appdata) = std::env::var_os("APPDATA") {
                extra.push(PathBuf::from(appdata).join("npm"));
            }
            if let Some(prefix) = std::env::var_os("NPM_CONFIG_PREFIX") {
                extra.push(prefix.into());
            }
            if let Some(programs) = std::env::var_os("ProgramFiles") {
                extra.push(PathBuf::from(programs).join("nodejs"));
            }
        }
        for d in extra {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
        Self { dirs }
    }

    /// The first executable `binary` in the search dirs, by this platform's
    /// rules (see [`find_in`]). Names with a path separator are refused
    /// (registry binaries are bare names).
    pub fn find(&self, binary: &str) -> Option<PathBuf> {
        find_in(
            &self.dirs,
            binary,
            std::env::var_os("PATHEXT").as_deref(),
            cfg!(windows),
        )
    }
}

/// The first executable `binary` in the absolute `dirs`. Without `windows`,
/// `dir/binary`. With `windows`, per dir in order, `binary` plus each
/// launchable extension in `pathext` order (ASCII case ignored; `.COM;.EXE;
/// .BAT;.CMD` when `pathext` is unset or lists none of them), or `binary`
/// itself when it already ends in one; never the extension-less file, never
/// `.ps1`. Names with a path separator are refused.
fn find_in(
    dirs: &[PathBuf],
    binary: &str,
    pathext: Option<&OsStr>,
    windows: bool,
) -> Option<PathBuf> {
    if binary.is_empty() || binary.contains(['/', '\\']) {
        return None;
    }
    let names = if windows {
        windows_names(binary, pathext)
    } else {
        vec![binary.to_owned()]
    };
    dirs.iter()
        .filter(|d| d.is_absolute())
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| is_executable(p))
}

/// The file names [`find_in`] tries for `binary` on Windows, in order.
/// Appended extensions are lower-case: Windows matches either case, and the
/// path shown (`codex.cmd`) reads as npm wrote it.
fn windows_names(binary: &str, pathext: Option<&OsStr>) -> Vec<String> {
    let launchable = |ext: &str| {
        WINDOWS_LAUNCHABLE
            .iter()
            .any(|l| l.eq_ignore_ascii_case(ext))
    };
    if binary
        .rfind('.')
        .is_some_and(|dot| launchable(&binary[dot..]))
    {
        return vec![binary.to_owned()];
    }
    let mut exts: Vec<String> = Vec::new();
    for ext in pathext
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|e| launchable(e))
    {
        let ext = ext.to_ascii_lowercase();
        if !exts.contains(&ext) {
            exts.push(ext);
        }
    }
    if exts.is_empty() {
        exts = WINDOWS_LAUNCHABLE
            .iter()
            .map(|e| e.to_ascii_lowercase())
            .collect();
    }
    exts.into_iter().map(|e| format!("{binary}{e}")).collect()
}

/// The folders added to an agent-CLI child's `PATH`, shared by probes and
/// calls (and the CLI's vendor commands) so detection runs the CLI as calls
/// will: the CLI's own folder first (a script finds e.g. `node` beside it),
/// then the absolute dirs of `search`.
pub fn agent_child_path(binary: &Path, search: &SearchPaths) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = binary.parent().map(Path::to_path_buf).into_iter().collect();
    dirs.extend(search.dirs.iter().filter(|d| d.is_absolute()).cloned());
    dirs
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// Every flag present.
    Ready,
    /// Usable; optional flags or feature switches could not be applied.
    Degraded,
    /// A required or safety flag is missing, or the prompt transport is unverified.
    Refused,
    NotInstalled,
}

impl Availability {
    pub fn usable(self) -> bool {
        matches!(self, Self::Ready | Self::Degraded)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Detection {
    pub provider: String,
    pub path: Option<PathBuf>,
    pub version: Option<String>,
    pub status: Availability,
    /// Why the provider is refused.
    pub reasons: Vec<String>,
    /// Optional flags and feature switches that could not be applied.
    pub not_applied: Vec<String>,
    /// The arguments that will be passed (flags confirmed by `--help`).
    #[serde(skip)]
    pub args: Vec<ArgSpec>,
    /// Whether the entry's `model_args` can be passed (`--help` lists their
    /// flag); when not, a configured model is not applied.
    #[serde(skip)]
    pub model_args_supported: bool,
    /// Feature names to switch off with the entry's `disable_features.flag`.
    pub disabled_features: Vec<String>,
}

/// Whether `help` lists `flag` as a whole word (`--tools` does not match
/// `--tools-x`; `-p` matches `-p, --print`).
pub fn help_has_flag(help: &str, flag: &str) -> bool {
    if flag.is_empty() {
        return false;
    }
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let mut from = 0;
    while let Some(i) = help[from..].find(flag) {
        let start = from + i;
        let end = start + flag.len();
        let before_ok = help[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word(c));
        let after_ok = help[end..].chars().next().is_none_or(|c| !is_word(c));
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// Feature names a `features list` table reports as present (first column),
/// skipping removed ones.
pub fn listed_features(table: &str) -> Vec<String> {
    table
        .lines()
        .filter_map(|l| {
            let mut cols = l.split_whitespace();
            let name = cols.next()?;
            let rest: Vec<&str> = cols.collect();
            let valid = name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            (valid
                && rest.len() >= 2
                && matches!(rest.last(), Some(&"true" | &"false"))
                && !rest.contains(&"removed"))
            .then(|| name.to_string())
        })
        .collect()
}

/// Runs `path args` with no stdin in a private temp dir, the allow-listed
/// environment with [`agent_child_path`] on `PATH` (as calls run it) and the
/// probe timeout, contained like every agent-CLI child; stdout (stderr when
/// stdout is empty) or why it failed.
pub(super) fn probe(
    path: &Path,
    args: &[String],
    search: &SearchPaths,
    execution: &ExecutionContext,
) -> Result<String, String> {
    let sandbox = Sandbox::new().map_err(|e| e.to_string())?;
    let env = child_env(std::env::vars_os(), &agent_child_path(path, search));
    let out = run(
        &RunSpec {
            program: path,
            args,
            cwd: sandbox.path(),
            env: &env,
            stdin: None,
            timeout: PROBE_TIMEOUT,
        },
        execution,
    )
    .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let first = out.stderr_head.lines().next().unwrap_or("").trim();
        return Err(format!(
            "`{}` failed ({}){}",
            args.join(" "),
            out.status,
            if first.is_empty() {
                String::new()
            } else {
                format!(": {}", first.chars().take(200).collect::<String>())
            }
        ));
    }
    // Some CLIs print help on stderr.
    Ok(if stdout.trim().is_empty() {
        out.stderr_head
    } else {
        stdout
    })
}

/// Finds the CLI, runs `--version` and its help, and decides which flags
/// can be passed. HTTP providers are not detected here: they are `Ready`.
/// A usable detection of the same entry and unchanged binary is reused (by
/// every caller in this process); refusals and incomplete probes are always
/// re-run.
pub fn detect(
    entry: &ProviderEntry,
    search: &SearchPaths,
    execution: &ExecutionContext,
) -> Detection {
    let mut d = Detection {
        provider: entry.id.clone(),
        path: None,
        version: None,
        status: Availability::Ready,
        reasons: Vec::new(),
        not_applied: Vec::new(),
        args: Vec::new(),
        model_args_supported: true,
        disabled_features: Vec::new(),
    };
    if entry.runtime != ProviderRuntime::AgentCli {
        return d;
    }
    let binary = entry.binary.as_deref().unwrap_or_default();
    let Some(path) = search.find(binary) else {
        d.status = Availability::NotInstalled;
        d.reasons.push(format!("`{binary}` was not found"));
        return d;
    };
    let before = identity(&path);
    if let Some(id) = before {
        let cache = DETECTIONS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(hit) = cache
            .iter()
            .find(|c| c.path == path && c.identity == id && c.entry == *entry)
        {
            return hit.detection.clone();
        }
    }
    d.path = Some(path.clone());
    probe_cli(entry, &path, search, &mut d, execution);
    // Kept only when complete and the binary did not change while probed.
    if let Some(id) = before
        && d.status.usable()
        && d.version.is_some()
        && identity(&path) == Some(id)
    {
        let mut cache = DETECTIONS.lock().unwrap_or_else(PoisonError::into_inner);
        cache.retain(|c| !(c.path == path && c.entry == *entry));
        if cache.len() >= MAX_CACHED {
            cache.remove(0);
        }
        cache.push(Cached {
            entry: entry.clone(),
            path,
            identity: id,
            detection: d.clone(),
        });
    }
    d
}

/// The probes of [`detect`] for the CLI at `path`.
fn probe_cli(
    entry: &ProviderEntry,
    path: &Path,
    search: &SearchPaths,
    d: &mut Detection,
    execution: &ExecutionContext,
) {
    match probe(path, &entry.version_args, search, execution) {
        Ok(v) => {
            d.version = v
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
        }
        Err(e) => d.not_applied.push(format!("version check: {e}")),
    }
    if entry.prompt_transport == PromptTransport::Unverified {
        d.status = Availability::Refused;
        d.reasons.push(
            "no verified way to pass the prompt outside the command line and turn tools off".into(),
        );
        return;
    }
    let help = match probe(path, &entry.help_args, search, execution) {
        Ok(h) => h,
        Err(e) => {
            d.status = Availability::Refused;
            d.reasons.push(format!("could not read its help: {e}"));
            return;
        }
    };
    apply_help(entry, &help, d);
    if d.status != Availability::Refused
        && let Some(sw) = &entry.disable_features
    {
        if !help_has_flag(&help, &sw.flag) {
            d.status = Availability::Refused;
            d.reasons.push(format!(
                "safety flag {} (feature switches) is missing",
                sw.flag
            ));
        } else {
            match probe(path, &sw.list_args, search, execution) {
                Ok(table) => apply_features(entry, &table, d),
                Err(e) => {
                    d.status = Availability::Refused;
                    d.reasons
                        .push(format!("could not establish feature switches: {e}"));
                }
            }
        }
    }
    if d.status == Availability::Ready && !d.not_applied.is_empty() {
        d.status = Availability::Degraded;
    }
}

/// Keeps the flags `help` lists; a missing required or safety flag refuses
/// the provider, a missing optional one is reported.
pub fn apply_help(entry: &ProviderEntry, help: &str, d: &mut Detection) {
    for arg in &entry.args {
        if help_has_flag(help, &arg.flag) {
            d.args.push(arg.clone());
            continue;
        }
        match arg.need {
            ArgNeed::Required => {
                d.status = Availability::Refused;
                d.reasons
                    .push(format!("required flag {} is missing", arg.flag));
            }
            ArgNeed::Safety => {
                d.status = Availability::Refused;
                d.reasons
                    .push(format!("safety flag {} is missing", arg.flag));
            }
            ArgNeed::Optional => d.not_applied.push(arg.flag.clone()),
        }
    }
    if let Some(flag) = entry.model_args.first().filter(|f| f.starts_with('-'))
        && !help_has_flag(help, flag)
    {
        d.model_args_supported = false;
        d.not_applied.push(format!("{flag} (model selection)"));
    }
    if let Some(sw) = &entry.disable_features
        && !help_has_flag(help, &sw.flag)
    {
        d.status = Availability::Refused;
        d.reasons
            .push(format!("safety flag {} is missing", sw.flag));
    }
}

/// Keeps the registry's feature switches that the CLI lists.
pub fn apply_features(entry: &ProviderEntry, table: &str, d: &mut Detection) {
    let Some(sw) = &entry.disable_features else {
        return;
    };
    if !table.lines().any(|line| {
        let columns: Vec<_> = line.split_whitespace().collect();
        columns.len() >= 3 && matches!(columns.last(), Some(&"true" | &"false"))
    }) {
        d.status = Availability::Refused;
        d.reasons
            .push("could not establish feature switches: invalid or empty feature list".into());
        return;
    }
    let listed = listed_features(table);
    for f in &sw.features {
        if listed.contains(f) {
            d.disabled_features.push(f.clone());
        } else {
            d.not_applied.push(format!("{} {f}", sw.flag));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::registry::ProviderRegistry;

    #[test]
    fn flag_matching_is_whole_word() {
        let help = "  -p, --print   Print\n  --tools <tools...>\n  --output-format=<f>\n";
        assert!(help_has_flag(help, "-p"));
        assert!(help_has_flag(help, "--print"));
        assert!(help_has_flag(help, "--tools"));
        assert!(help_has_flag(help, "--output-format"));
        assert!(!help_has_flag(help, "--tool"));
        assert!(!help_has_flag(help, "--output"));
        assert!(!help_has_flag(help, "-o"));
    }

    #[test]
    fn missing_safety_flag_refuses_missing_optional_degrades() {
        let reg = ProviderRegistry::builtin().unwrap();
        let claude = reg.get("claude-cli").unwrap();
        let full: String = claude
            .args
            .iter()
            .map(|a| format!("  {} x\n", a.flag))
            .chain(["  --model <m>\n".to_string()])
            .collect();
        let mut d = blank(claude);
        apply_help(claude, &full, &mut d);
        assert_eq!(d.status, Availability::Ready);
        assert!(d.not_applied.is_empty());
        assert!(d.model_args_supported);

        let mut d = blank(claude);
        apply_help(claude, &full.replace("--model", "--other"), &mut d);
        assert_eq!(d.status, Availability::Ready, "optional only reported");
        assert!(!d.model_args_supported, "model flag missing from help");
        assert_eq!(d.not_applied, vec!["--model (model selection)"]);

        let mut d = blank(claude);
        apply_help(
            claude,
            &full.replace("--disable-slash-commands", "--other"),
            &mut d,
        );
        assert_eq!(d.status, Availability::Ready, "optional only reported");
        assert_eq!(d.not_applied, vec!["--disable-slash-commands"]);

        let mut d = blank(claude);
        apply_help(claude, &full.replace("--safe-mode", "--other"), &mut d);
        assert_eq!(d.status, Availability::Refused);
        assert!(d.reasons[0].contains("safety flag --safe-mode"));
    }

    #[test]
    fn codex_requires_confirmed_feature_disable_support() {
        let reg = ProviderRegistry::builtin().unwrap();
        let codex = reg.get("codex-cli").unwrap();
        let full: String = codex.args.iter().map(|a| format!("{} ", a.flag)).collect();
        let mut detection = blank(codex);
        apply_help(codex, &full, &mut detection);
        assert_eq!(detection.status, Availability::Refused);
        assert!(detection.reasons.iter().any(|r| r.contains("--disable")));
    }

    #[cfg(unix)]
    #[test]
    fn codex_failed_feature_probe_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let reg = ProviderRegistry::builtin().unwrap();
        let codex = reg.get("codex-cli").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join(codex.binary.as_deref().unwrap());
        let flags: String = codex.args.iter().map(|a| format!("{} ", a.flag)).collect();
        std::fs::write(&binary, format!("#!/bin/sh\ncase \"$*\" in\n*features*) exit 1;;\n*) echo '{flags} --disable --model';;\nesac\n")).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (_, execution) = crate::execution::test_context();
        let detection = detect(
            codex,
            &SearchPaths::new(vec![dir.path().to_owned()]),
            &execution,
        );
        assert_eq!(detection.status, Availability::Refused);
        assert!(
            detection
                .reasons
                .iter()
                .any(|r| r.contains("feature switches"))
        );
    }

    #[test]
    fn features_restricted_to_listed_ones() {
        let reg = ProviderRegistry::builtin().unwrap();
        let codex = reg.get("codex-cli").unwrap();
        let table =
            "shell_tool    stable  true\nunified_exec  stable  true\nhooks  removed  false\n";
        let mut d = blank(codex);
        apply_features(codex, table, &mut d);
        assert_eq!(d.disabled_features, vec!["shell_tool", "unified_exec"]);
        assert!(d.not_applied.contains(&"--disable hooks".to_string()));
    }

    #[test]
    fn codex_unreadable_feature_table_is_refused() {
        let registry = ProviderRegistry::builtin().unwrap();
        let codex = registry.get("codex-cli").unwrap();
        for table in [
            "",
            "error: feature listing is unavailable",
            "Usage: codex features list",
        ] {
            let mut detection = blank(codex);
            apply_features(codex, table, &mut detection);
            assert_eq!(detection.status, Availability::Refused, "{table:?}");
        }
    }

    #[test]
    fn unverified_and_missing_are_refused() {
        let reg = ProviderRegistry::builtin().unwrap();
        let empty = SearchPaths::new(vec![]);
        let (tree, execution) = crate::execution::test_context();
        let d = detect(reg.get("agy-cli").unwrap(), &empty, &execution);
        assert_eq!(tree.prepared.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(d.status, Availability::NotInstalled);
        assert!(!d.status.usable());
        assert!(
            SearchPaths::new(vec!["/bin".into()])
                .find("../bin/sh")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn usable_detection_is_reused_until_the_cli_changes() {
        use std::os::unix::fs::PermissionsExt;
        let reg = ProviderRegistry::builtin().unwrap();
        let claude = reg.get("claude-cli").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join(claude.binary.as_deref().unwrap());
        let runs = dir.path().join("runs");
        let flags: String = claude.args.iter().map(|a| format!("{} ", a.flag)).collect();
        let write = |extra: &str| {
            std::fs::write(
                &binary,
                format!(
                    "#!/bin/sh\necho run >> {}\necho '{flags} --model 1.0'\n{extra}\n",
                    runs.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        let probes = || {
            std::fs::read_to_string(&runs)
                .unwrap_or_default()
                .lines()
                .count()
        };
        let search = SearchPaths::new(vec![dir.path().to_owned()]);
        let (tree, execution) = crate::execution::test_context();
        write("");
        let first = detect(claude, &search, &execution);
        assert!(first.status.usable(), "{:?}", first.reasons);
        let after_first = probes();
        assert!(after_first >= 2, "version and help were probed");
        assert_eq!(
            tree.kills(),
            after_first,
            "every probe ran as a contained tree, ended with the call"
        );
        assert_eq!(detect(claude, &search, &execution), first);
        assert_eq!(
            probes(),
            after_first,
            "an unchanged CLI is not probed again"
        );

        // An upgraded binary (other size) is probed afresh.
        write("# upgraded");
        assert!(detect(claude, &search, &execution).status.usable());
        assert_eq!(probes(), after_first * 2);

        // A refusal is never reused.
        let other = tempfile::tempdir().unwrap();
        let refusing = other.path().join(claude.binary.as_deref().unwrap());
        let refusals = other.path().join("runs");
        std::fs::write(
            &refusing,
            format!("#!/bin/sh\necho run >> {}\nexit 1\n", refusals.display()),
        )
        .unwrap();
        std::fs::set_permissions(&refusing, std::fs::Permissions::from_mode(0o700)).unwrap();
        let search = SearchPaths::new(vec![other.path().to_owned()]);
        assert!(!detect(claude, &search, &execution).status.usable());
        let once = std::fs::read_to_string(&refusals).unwrap().lines().count();
        assert!(!detect(claude, &search, &execution).status.usable());
        assert_eq!(
            std::fs::read_to_string(&refusals).unwrap().lines().count(),
            once * 2
        );
    }

    /// An empty file the current user may execute.
    fn exec_file(path: &Path) {
        std::fs::write(path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn windows_find_takes_a_launchable_shim_never_the_bare_one() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec![dir.path().to_owned()];
        let ext = |e: &str| Some(OsStr::new(e).to_owned());
        exec_file(&dir.path().join("codex"));
        assert_eq!(
            find_in(&dirs, "codex", None, true),
            None,
            "npm's extension-less sh shim"
        );
        exec_file(&dir.path().join("codex.ps1"));
        assert_eq!(
            find_in(&dirs, "codex", ext(".PS1;.CMD").as_deref(), true),
            None,
            "never .ps1, even when PATHEXT lists it"
        );
        exec_file(&dir.path().join("codex.cmd"));
        let shim = Some(dir.path().join("codex.cmd"));
        assert_eq!(find_in(&dirs, "codex", None, true), shim);
        assert_eq!(
            find_in(&dirs, "codex", ext(".PS1;.CMD").as_deref(), true),
            shim
        );

        // Off Windows: the bare name, as before, whatever PATHEXT says.
        let bare = Some(dir.path().join("codex"));
        assert_eq!(find_in(&dirs, "codex", None, false), bare);
        assert_eq!(find_in(&dirs, "codex", ext(".CMD").as_deref(), false), bare);
    }

    #[test]
    fn windows_find_follows_pathext_within_each_dir_in_order() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        exec_file(&first.path().join("x.exe"));
        exec_file(&first.path().join("x.cmd"));
        exec_file(&second.path().join("x.com"));
        let dirs = vec![first.path().to_owned(), second.path().to_owned()];
        let find = |pathext: Option<&str>| find_in(&dirs, "x", pathext.map(OsStr::new), true);
        assert_eq!(
            find(None),
            Some(first.path().join("x.exe")),
            "default order"
        );
        assert_eq!(find(Some(".CMD;.EXE")), Some(first.path().join("x.cmd")));
        assert_eq!(
            find(Some(" .Cmd ; .exe ")),
            Some(first.path().join("x.cmd")),
            "ASCII case and spaces ignored"
        );
        assert_eq!(
            find(Some(".COM")),
            Some(second.path().join("x.com")),
            "an earlier dir without a match is passed over"
        );
        assert_eq!(
            find(Some(".PS1;.VBS")),
            Some(first.path().join("x.exe")),
            "no launchable entry: the default order"
        );
    }

    #[test]
    fn windows_names_keep_an_explicit_extension() {
        assert_eq!(windows_names("codex.cmd", None), ["codex.cmd"]);
        assert_eq!(
            windows_names("codex.EXE", Some(OsStr::new(".CMD"))),
            ["codex.EXE"]
        );
        assert_eq!(
            windows_names("codex.ps1", None),
            [
                "codex.ps1.com",
                "codex.ps1.exe",
                "codex.ps1.bat",
                "codex.ps1.cmd"
            ]
        );
        assert_eq!(
            windows_names("codex", Some(OsStr::new(".CMD;.cmd;.EXE"))),
            ["codex.cmd", "codex.exe"]
        );
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec![dir.path().to_owned()];
        exec_file(&dir.path().join("t.cmd"));
        assert_eq!(
            find_in(&dirs, "t.cmd", None, true),
            Some(dir.path().join("t.cmd"))
        );
        for refused in ["", "..\\t.cmd", "sub/t.cmd"] {
            assert_eq!(find_in(&dirs, refused, None, true), None, "{refused:?}");
        }
    }

    #[test]
    fn agent_child_path_puts_the_cli_folder_first() {
        let base = std::env::temp_dir();
        let search = SearchPaths::new(vec![
            base.join("s1"),
            PathBuf::from("relative"),
            base.join("s2"),
        ]);
        assert_eq!(
            agent_child_path(&base.join("cli").join("codex.cmd"), &search),
            [base.join("cli"), base.join("s1"), base.join("s2")]
        );
    }

    fn blank(entry: &ProviderEntry) -> Detection {
        Detection {
            provider: entry.id.clone(),
            path: None,
            version: None,
            status: Availability::Ready,
            reasons: vec![],
            not_applied: vec![],
            args: vec![],
            model_args_supported: true,
            disabled_features: vec![],
        }
    }
}
