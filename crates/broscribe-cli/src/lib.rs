//! The `broscribe` command line. A library so tests run commands in process
//! with injected dependencies (fake models, temporary folders); `main.rs` only
//! wires the native platform.
//!
//! Output contract: command results go to `out` (human text, or one JSON
//! document under `--json`); progress and notes go to stderr. Errors are a
//! line on `err`, or `{"error":{"code","message"}}` on `out` under `--json`;
//! the exit code is 1 for command errors and 2 for usage errors.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use broscribe_core::config::{Config, Profile};
use broscribe_core::{Error, Library, Result};
use broscribe_models::llm::SearchPaths;
use broscribe_models::{ExecutionContext, ModelProvider, ModelSet, SpeakerEmbedder, Vad};
use broscribe_platform::paths::{ENV_DATA_DIR, ENV_LIBRARY, resolve_paths_from};
use broscribe_platform::{
    AppPaths, DecoderChain, MicCapture, ProcessProbe, ResolvedPaths, SecretStore,
};

mod args;
mod cli;
mod cmd;
pub mod mcp;
mod query;

pub use cmd::update::UpdateDeps;

/// Everything a command needs from the outside world.
pub struct Deps {
    pub paths: Box<dyn AppPaths>,
    pub models: Box<dyn ModelProvider>,
    pub decoder: DecoderChain,
    pub process: Box<dyn ProcessProbe>,
    /// Microphone for `audio devices`, `setup voice --mic` and `setup try`.
    pub mic: Box<dyn MicCapture>,
    /// API keys of enabled providers (remote text embedding).
    pub secrets: std::sync::Arc<dyn SecretStore>,
    /// Explicit integration sign-in only; never used by background refresh.
    pub browser: std::sync::Arc<dyn broscribe_platform::BrowserLauncher>,
    pub autostart: Box<dyn broscribe_platform::Autostart>,
    pub agent_instance: Box<dyn broscribe_platform::AgentInstanceProbe>,
    /// This CLI, the home folder and the agent CLIs (`agents`, `mcp enable`).
    pub agents: AgentEnv,
    /// Process-tree containment and TLS trust of every provider, agent-CLI
    /// probe, model list and model download the commands run.
    pub execution: ExecutionContext,
    /// Native installation verification and injectable release transport.
    pub updates: UpdateDeps,
    /// The wall clock commands read "now" from (relative dates and the
    /// continuations bound to them).
    pub clock: WallClock,
}

/// A per-`Deps` wall clock: the system's, or one a test controls. Never
/// process-global time or environment changes.
#[derive(Clone, Default)]
pub enum WallClock {
    #[default]
    System,
    Custom(std::sync::Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>),
}

impl WallClock {
    /// A clock the caller controls (tests: fixed or stepped instants).
    pub fn custom(
        now: std::sync::Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>,
    ) -> Self {
        WallClock::Custom(now)
    }

    pub fn now(&self) -> chrono::DateTime<chrono::Utc> {
        match self {
            WallClock::System => chrono::Utc::now(),
            WallClock::Custom(now) => now(),
        }
    }
}

/// The system wall clock (production `Deps`).
pub fn system_clock() -> WallClock {
    WallClock::System
}

/// What `agents …` and `mcp enable|disable|status` work with. Injected so
/// tests use a temporary home, fake agent executables and an empty
/// environment, never the user's real registrations.
#[derive(Clone)]
pub struct AgentEnv {
    /// This CLI's canonical path: what the link, the skill and MCP
    /// registrations record.
    pub cli: PathBuf,
    /// Home folder (`~/.local/bin`, `~/.agents/skills`, `~/.claude/skills`);
    /// `None` when unknown (those commands then fail).
    pub home: Option<PathBuf>,
    /// Where `claude`, `codex` and `gemini` are looked for, and the agent
    /// CLIs of `providers list|test|models`.
    pub search: SearchPaths,
    /// Environment offered to agent CLIs; only the allow-listed part
    /// (`llm::process::child_env`) reaches them.
    pub vars: Vec<(OsString, OsString)>,
    /// Facts of the running installation for About and durable CLI, skill
    /// and MCP registration. Tests inject their own installation facts.
    pub about: std::sync::Arc<dyn broscribe_platform::about::PlatformAboutReader>,
    pub uninstall: std::sync::Arc<dyn broscribe_platform::uninstall::DesktopUninstall>,
}

impl AgentEnv {
    /// The running process: `cli` canonicalized (on Windows without the
    /// `\\?\` prefix when the plain path means the same file:
    /// [`broscribe_platform::plain_verbatim`]), `HOME`, the usual agent
    /// install folders and this process's environment.
    pub fn from_process(cli: PathBuf) -> Self {
        Self {
            cli: std::fs::canonicalize(&cli)
                .map(|c| broscribe_platform::plain_verbatim(&c))
                .unwrap_or(cli),
            home: std::env::home_dir(),
            search: SearchPaths::from_env(),
            vars: std::env::vars_os().collect(),
            uninstall: std::sync::Arc::new(broscribe_platform_native::uninstall::NativeUninstall),
            about: broscribe_platform_native::NativePlatform::about_reader(env!(
                "CARGO_PKG_VERSION"
            )),
        }
    }

    /// Nothing outside `root`: home `root/home`, CLI `root/bin/broscribe`, no
    /// agent search folders and an empty environment (tests).
    pub fn isolated(root: &Path) -> Self {
        use broscribe_platform::about::{
            FakeAboutReader, InspectedInstallation, Packaging, SealState, SignatureFacts,
            SignatureKind, collect_about,
        };
        Self {
            cli: root.join("bin").join("broscribe"),
            home: Some(root.join("home")),
            search: SearchPaths::default(),
            vars: Vec::new(),
            uninstall: std::sync::Arc::new(
                broscribe_platform::uninstall::FakeDesktopUninstall::default(),
            ),
            about: std::sync::Arc::new(FakeAboutReader::new(collect_about(
                env!("CARGO_PKG_VERSION"),
                InspectedInstallation {
                    packaging: Packaging::Unbundled,
                    resources: None,
                    location_notes: Vec::new(),
                    bundle: None,
                    signature: SignatureFacts::without_notarization(
                        SignatureKind::Unknown {
                            reason: "synthetic fixture".into(),
                        },
                        SealState::NotChecked,
                    ),
                    translated: None,
                },
            ))),
        }
    }
}

/// A model provider built on first use: commands that need no models keep
/// working when the model registry cannot be read (`doctor` reports it),
/// and those that do get the error then. A failed build is retried on the
/// next use.
pub struct LazyModels<P, F> {
    make: F,
    cell: OnceLock<P>,
}

impl<P, F> LazyModels<P, F>
where
    P: ModelProvider,
    F: Fn() -> Result<P> + Send + Sync,
{
    pub fn new(make: F) -> Self {
        Self {
            make,
            cell: OnceLock::new(),
        }
    }

    fn get(&self) -> Result<&P> {
        if let Some(p) = self.cell.get() {
            return Ok(p);
        }
        let p = (self.make)()?;
        Ok(self.cell.get_or_init(|| p))
    }
}

impl<P, F> ModelProvider for LazyModels<P, F>
where
    P: ModelProvider,
    F: Fn() -> Result<P> + Send + Sync,
{
    fn load(&self, profile: &Profile) -> Result<ModelSet> {
        self.get()?.load(profile)
    }

    fn load_vad(&self, model_id: &str) -> Result<Box<dyn Vad + Send>> {
        self.get()?.load_vad(model_id)
    }

    fn load_embedder(&self, model_id: &str) -> Result<Box<dyn SpeakerEmbedder + Send>> {
        self.get()?.load_embedder(model_id)
    }
}

/// Runs one command; returns the process exit code. Command output goes to
/// `out`; under `--json`, errors are also printed to `out` as
/// `{"error":{"code","message"}}`, otherwise to `err`. Progress and notices
/// from other threads reach `err` while the command is still running, so it
/// must be shareable with the thread that writes them.
pub fn run(
    args: Vec<OsString>,
    deps: &Deps,
    out: &mut dyn Write,
    err: &mut (dyn Write + Send),
) -> i32 {
    let cli = match cli::parse(args) {
        cli::Parsed::Run(cli) => cli,
        cli::Parsed::Display(text) => {
            let _ = write!(out, "{text}");
            return 0;
        }
        cli::Parsed::Usage(usage) => {
            if usage.json {
                write_error(out, err, true, &Error::InvalidInput(usage.message));
            } else {
                let _ = write!(err, "{}", usage.rendered);
            }
            return 2;
        }
    };
    let json = cli.json;
    // A proposed-policy check names no path, also when it fails before it
    // runs (SET-01).
    let before_command = |error: Error| {
        if cmd::retention::is_policy_preview(&cli.command) {
            cmd::retention::preview_admission_error(error)
        } else {
            error
        }
    };
    let profile_override = std::env::var_os(ENV_DATA_DIR).filter(|p| !p.is_empty());
    let library_override = std::env::var_os(ENV_LIBRARY).filter(|p| !p.is_empty());
    use cli::UpdateCmd::{Install, Recover, Rollback};
    if let cli::Command::Update {
        action: Install { .. } | Rollback { .. } | Recover,
    } = &cli.command
        && let Err(error) = cmd::update::check_path_selectors(
            deps.paths.as_ref(),
            profile_override.as_deref().map(Path::new),
            cli.library.as_deref(),
            library_override.as_deref().map(Path::new),
        )
    {
        write_error(out, err, json, &error);
        return 1;
    }
    let paths = match resolve_paths_from(deps.paths.as_ref(), |name| match name {
        ENV_DATA_DIR => profile_override.clone(),
        ENV_LIBRARY => library_override.clone(),
        _ => std::env::var_os(name),
    }) {
        Ok(paths) => paths,
        Err(error) => {
            write_error(out, err, json, &before_command(error));
            return 1;
        }
    };
    // This owner outlives Ctx's scoped threads and MCP's runtime teardown.
    // Control commands acquire their own observation/exclusive lease.
    let admission = if matches!(
        &cli.command,
        cli::Command::Maintenance { .. } | cli::Command::Uninstall(_)
    ) {
        None
    } else {
        match broscribe_core::maintenance::ModeLease::acquire(
            &paths.data_dir,
            broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
        ) {
            Ok(lease) => {
                if lease.mode() == broscribe_core::maintenance::Mode::Maintenance
                    && !cmd::maintenance::allowed(&cli.command)
                {
                    write_error(out, err, json, &cmd::maintenance::denied());
                    return 1;
                }
                Some(lease)
            }
            Err(error) => {
                write_error(out, err, json, &before_command(error));
                return 1;
            }
        }
    };
    Ctx::scoped(
        deps,
        json,
        cli.library,
        paths,
        admission.as_ref(),
        out,
        err,
        |ctx| match cmd::dispatch(ctx, cli.command) {
            Ok(()) => ctx.exit_code.unwrap_or(0),
            Err(e) => {
                write_error(ctx.out, ctx.err, json, &e);
                1
            }
        },
    )
}

/// Maintenance also inspects the native default when an override selects
/// another profile. Check the raw path before resolving it against the cwd.
fn relative_default_data_dir(paths: &dyn AppPaths) -> Option<String> {
    let path = paths.data_dir();
    let source = paths.default_data_dir_source();
    (!path.is_absolute()).then(|| {
        format!(
            "The default data folder ({}) from {source} is relative. Correct {source} so the default data folder is absolute",
            path.display(),
        )
    })
}

/// Where the library folder came from (`doctor` reports it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LibrarySource {
    Flag,
    Env,
    Config,
    Default,
}

impl LibrarySource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LibrarySource::Flag => "--library",
            LibrarySource::Env => ENV_LIBRARY,
            LibrarySource::Config => "config",
            LibrarySource::Default => "default",
        }
    }
}

/// One command invocation: dependencies, resolved folders and output streams.
pub(crate) struct Ctx<'a> {
    pub deps: &'a Deps,
    pub json: bool,
    /// A reported partial result can still carry a failing process status.
    pub exit_code: Option<i32>,
    pub library_flag: Option<PathBuf>,
    pub paths: ResolvedPaths,
    pub admission: Option<&'a broscribe_core::maintenance::ModeLease>,
    pub out: &'a mut dyn Write,
    /// The invocation's error stream; writes first deliver the notes queued
    /// before them, so lines keep the order they were produced in.
    pub err: &'a mut dyn Write,
    /// MCP supplies the input a CLI command would otherwise read from stdin.
    pub(crate) request_input: Option<String>,
    /// All tools on a connection operate on its original library.
    pub(crate) request_library: Option<PathBuf>,
    /// Cancellation belongs to a single MCP invocation.
    pub(crate) request_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) request_progress: Option<tokio::sync::watch::Sender<Option<(f64, f64)>>>,
    pub(crate) request_edit_history:
        Option<std::sync::Arc<std::sync::Mutex<cmd::identity_review::EditHistory>>>,
    notes: Notes<'a>,
}

impl Ctx<'_> {
    /// Runs `command` with the invocation's context. Background notes are
    /// written to `err` by a pump thread, started when a command first asks
    /// for them, while the command itself may be blocked; every queued note
    /// is written before this returns.
    #[expect(
        clippy::too_many_arguments,
        reason = "The borrowed admission must outlive the context and its joined threads; control commands deliberately have none."
    )]
    pub(crate) fn scoped<R>(
        deps: &Deps,
        json: bool,
        library_flag: Option<PathBuf>,
        paths: ResolvedPaths,
        admission: Option<&broscribe_core::maintenance::ModeLease>,
        out: &mut dyn Write,
        err: &mut (dyn Write + Send),
        command: impl FnOnce(&mut Ctx<'_>) -> R,
    ) -> R {
        let stream = ErrStream {
            writer: std::sync::Mutex::new(err),
            notes: BackgroundNotes::new(),
        };
        std::thread::scope(|scope| {
            let pump = std::sync::Once::new();
            let start_pump = || pump.call_once(|| stream.start_pump(scope));
            // Dropped after the context, also when the command panics: the
            // pump writes what is queued and ends, so the scope can join it.
            let _close = CloseNotes(&stream);
            let mut err = &stream;
            let mut ctx = Ctx {
                deps,
                json,
                exit_code: None,
                library_flag,
                paths,
                admission,
                out,
                err: &mut err,
                request_input: None,
                request_library: None,
                request_cancel: None,
                request_progress: None,
                request_edit_history: None,
                notes: Notes {
                    queue: &stream.notes,
                    start_pump: &start_pump,
                },
            };
            command(&mut ctx)
        })
    }
}

impl Ctx<'_> {
    pub(crate) fn check_request_cancelled(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        if self
            .request_cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            return Err(Error::Service {
                code: "cancelled",
                message: "the client cancelled the request".into(),
            });
        }
        Ok(())
    }

    pub(crate) fn input_json<T: serde::de::DeserializeOwned>(&self, command: &str) -> Result<T> {
        match self.request_input.as_deref() {
            Some(input) => serde_json::from_str(input)
                .map_err(|e| Error::InvalidInput(format!("{command} input: {e}"))),
            None if self.request_cancel.is_some() => {
                Err(Error::Internal(format!("missing MCP input for {command}")))
            }
            None => query::read_stdin(command),
        }
    }

    pub(crate) fn input_text(&self, command: &str) -> Result<String> {
        match self.request_input.as_deref() {
            Some(input) => query::checked_instructions(input.to_owned()),
            None if self.request_cancel.is_some() => {
                Err(Error::Internal(format!("missing MCP input for {command}")))
            }
            None => query::read_stdin_text(command),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.paths.data_dir
    }

    pub fn config(&self) -> Result<Config> {
        Config::load(self.data_dir())
    }

    /// Library folder by precedence: `--library`, `BROSCRIBE_LIBRARY`, config
    /// `library`, platform default.
    pub fn library_path(&self, config: &Config) -> (PathBuf, LibrarySource) {
        let (path, source) = self.configured_library_path(config);
        match &self.request_library {
            Some(root) if root != &path => (root.clone(), LibrarySource::Flag),
            _ => (path, source),
        }
    }

    pub(crate) fn configured_library_path(&self, config: &Config) -> (PathBuf, LibrarySource) {
        if let Some(p) = &self.library_flag {
            (p.clone(), LibrarySource::Flag)
        } else if let Some(p) = &self.paths.library_from_env {
            (p.clone(), LibrarySource::Env)
        } else if let Some(p) = &config.library {
            (p.clone(), LibrarySource::Config)
        } else {
            (
                self.paths.default_library_dir.clone(),
                LibrarySource::Default,
            )
        }
    }

    /// Explicit initialization paths (import and setup) may create the library.
    pub fn create_library(&self, config: &Config) -> Result<Library> {
        Library::open(self.library_path(config).0)
    }

    /// Commands on an existing library never create a folder on a wrong path.
    /// Missing libraries report `not_found`, including for write operations.
    pub fn library_existing(&self, config: &Config) -> Result<Library> {
        Library::open_existing(self.library_path(config).0)
    }

    /// Prints `value` under `--json`, else runs `human` on the output stream.
    pub fn emit(
        &mut self,
        value: serde_json::Value,
        human: impl FnOnce(&mut dyn Write) -> std::io::Result<()>,
    ) -> Result<()> {
        if self.json {
            print_json(self.out, &value)
        } else {
            human(self.out).map_err(|e| Error::io("writing output", e))
        }
    }

    /// Background callbacks cannot borrow the invocation's output stream;
    /// what they push is written to it while the command runs.
    pub(crate) fn background_notes(&self) -> BackgroundNotes {
        self.notes.progress().clone()
    }

    /// Pipeline notices share the per-invocation error-stream queue.
    pub(crate) fn pipeline_progress(&self) -> &dyn broscribe_pipeline::ProgressSink {
        self.notes.progress()
    }

    /// One line of progress or notes on the error stream.
    pub fn note(&mut self, line: impl std::fmt::Display) {
        let _ = writeln!(self.err, "{line}");
    }
}

/// The invocation's background notes and the pump that writes them.
pub(crate) struct Notes<'a> {
    queue: &'a BackgroundNotes,
    start_pump: &'a (dyn Fn() + 'a),
}

impl<'a> Notes<'a> {
    /// The queue, with its pump running: it outlives borrows of the context,
    /// so commands can hold it beside `ctx.out`/`ctx.err`.
    pub(crate) fn progress(&self) -> &'a BackgroundNotes {
        (self.start_pump)();
        self.queue
    }
}

/// Notes waiting for the error stream. Producers wait while it is full and
/// the pump runs (backpressure, never a dropped line).
const QUEUED_NOTES: usize = 64;

/// Per-invocation notes from any thread, written to the injected error
/// stream while the command runs. Callbacks never write to process-global
/// stdout/stderr. Notes pushed after the invocation ended have no stream to
/// reach and are discarded.
#[derive(Clone)]
pub(crate) struct BackgroundNotes(std::sync::Arc<NoteQueue>);

struct NoteQueue {
    state: std::sync::Mutex<PendingNotes>,
    /// Signalled whenever lines are added or taken, or the queue closes.
    changed: std::sync::Condvar,
}

struct PendingNotes {
    lines: std::collections::VecDeque<String>,
    /// A pump drains the queue, so a full queue makes producers wait.
    pumped: bool,
    closed: bool,
}

impl BackgroundNotes {
    fn new() -> Self {
        Self(std::sync::Arc::new(NoteQueue {
            state: std::sync::Mutex::new(PendingNotes {
                lines: std::collections::VecDeque::new(),
                pumped: false,
                closed: false,
            }),
            changed: std::sync::Condvar::new(),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, PendingNotes> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn push(&self, line: String) {
        let mut notes = self.state();
        while notes.pumped && !notes.closed && notes.lines.len() >= QUEUED_NOTES {
            notes = self
                .0
                .changed
                .wait(notes)
                .unwrap_or_else(|e| e.into_inner());
        }
        if notes.closed {
            return;
        }
        notes.lines.push_back(line);
        self.0.changed.notify_all();
    }

    /// Every queued line, oldest first; `close` also refuses later ones.
    fn take(&self, close: bool) -> std::collections::VecDeque<String> {
        let mut notes = self.state();
        notes.closed |= close;
        let lines = std::mem::take(&mut notes.lines);
        self.0.changed.notify_all();
        lines
    }

    /// Waits for lines; `false` once the queue is closed and empty.
    fn wait_for_lines(&self) -> bool {
        let mut notes = self.state();
        while notes.lines.is_empty() && !notes.closed {
            notes = self
                .0
                .changed
                .wait(notes)
                .unwrap_or_else(|e| e.into_inner());
        }
        !notes.lines.is_empty()
    }

    fn set_pumped(&self) {
        self.state().pumped = true;
    }
}

impl broscribe_pipeline::ProgressSink for BackgroundNotes {
    fn say(&self, line: std::fmt::Arguments<'_>) {
        self.push(line.to_string());
    }
}

/// The invocation's error stream, shared by the command's thread and the
/// notes pump. Lock order: `writer`, then the note queue.
struct ErrStream<'w> {
    writer: std::sync::Mutex<&'w mut (dyn Write + Send)>,
    notes: BackgroundNotes,
}

impl<'w> ErrStream<'w> {
    /// The writer, after the notes queued before this call. A failed write
    /// (a closed stream) is not the producer's failure: the note is dropped
    /// and the work goes on.
    fn locked(&self, close: bool) -> std::sync::MutexGuard<'_, &'w mut (dyn Write + Send)> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        for line in self.notes.take(close) {
            let _ = writeln!(writer, "{line}");
        }
        writer
    }

    /// Writes notes as they come, until the queue is closed and empty.
    fn pump(&self) {
        while self.notes.wait_for_lines() {
            drop(self.locked(false));
        }
    }

    /// Start before exposing the queue to any callback. Like other required
    /// scoped workers, failure to create the thread aborts this invocation;
    /// it must not silently turn a bounded live stream into an unbounded log.
    fn start_pump<'scope>(&'scope self, scope: &'scope std::thread::Scope<'scope, '_>) {
        scope.spawn(|| self.pump());
        self.notes.set_pumped();
    }
}

impl Write for &ErrStream<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.locked(false).write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.locked(false).write_all(buf)
    }

    /// One formatted write (a `writeln!` line) is never split by a note.
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> std::io::Result<()> {
        self.locked(false).write_fmt(args)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.locked(false).flush()
    }
}

/// Closes the notes when the invocation ends: what is queued is written,
/// later notes are refused and the pump ends.
struct CloseNotes<'s, 'w>(&'s ErrStream<'w>);

impl Drop for CloseNotes<'_, '_> {
    fn drop(&mut self) {
        let _ = self.0.locked(true).flush();
    }
}

/// Writes `value` as one JSON document and a newline.
pub fn print_json(out: &mut dyn Write, value: &serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(|e| Error::Internal(e.to_string()))?;
    writeln!(out, "{text}").map_err(|e| Error::io("writing output", e))
}

/// Stable error code/message; `account_exists` also names the existing account handle.
pub fn error_envelope(e: &Error) -> serde_json::Value {
    let mut value = serde_json::json!({ "error": { "code": e.code(), "message": e.to_string() } });
    if let Error::AccountExists { account_id } = e {
        value["error"]["account_id"] = serde_json::json!(account_id);
    }
    value
}

/// Reports `e`: the JSON envelope on `out` under `--json`, else a line on `err`.
pub fn write_error(out: &mut dyn Write, err: &mut dyn Write, json: bool, e: &Error) {
    if json {
        let _ = print_json(out, &error_envelope(e));
    } else {
        let _ = writeln!(err, "error: {e}");
    }
}

/// Serialises a result type for `--json`.
pub(crate) fn to_value<T: serde::Serialize>(v: &T) -> Result<serde_json::Value> {
    serde_json::to_value(v).map_err(|e| Error::Internal(format!("serialising output: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_models::fakes::FakeModelProvider;
    use broscribe_platform::fakes::{
        FakeMicCapture, FakePaths, FakeProcessProbe, FakeProcessTreeControl, FakeSecretStore,
        FakeTrustStore,
    };

    fn deps() -> Deps {
        Deps {
            paths: Box::new(FakePaths::new("/nonexistent/broscribe-test")),
            updates: crate::UpdateDeps::with_transport(
                std::sync::Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                    std::path::Path::new(&"/nonexistent/broscribe-test").join("install"),
                    "synthetic-test-target",
                )),
                std::sync::Arc::new(broscribe_updates::FakeTransport::default()),
            ),
            models: Box::new(FakeModelProvider::default()),
            decoder: DecoderChain::default(),
            process: Box::new(FakeProcessProbe::new(1)),
            mic: Box::new(FakeMicCapture::new(16_000)),
            secrets: std::sync::Arc::new(FakeSecretStore::default()),
            browser: std::sync::Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
            agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
            autostart: Box::new(broscribe_platform::fakes::FakeAutostart::default()),
            agents: AgentEnv::isolated(Path::new("/nonexistent/broscribe-test")),
            execution: ExecutionContext::new(
                std::sync::Arc::new(FakeProcessTreeControl::default()),
                std::sync::Arc::new(FakeTrustStore::default()),
            ),
            clock: WallClock::System,
        }
    }

    fn run_args(args: &[&str]) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = std::iter::once("broscribe")
            .chain(args.iter().copied())
            .map(OsString::from)
            .collect();
        let root = tempfile::tempdir().unwrap();
        let mut deps = deps();
        deps.paths = Box::new(FakePaths::new(root.path()));
        let code = run(args, &deps, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn version_plain_and_json() {
        let (code, out, _) = run_args(&["version"]);
        assert_eq!(code, 0);
        assert_eq!(
            out.trim(),
            format!("broscribe {}", env!("CARGO_PKG_VERSION"))
        );
        let (code, out, _) = run_args(&["--json", "version"]);
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn usage_errors() {
        let (code, out, err) = run_args(&["nope"]);
        assert_eq!(code, 2);
        assert!(out.is_empty() && !err.is_empty());
        let (code, out, _) = run_args(&["--json", "nope"]);
        assert_eq!(code, 2);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], "invalid_input");
    }

    #[test]
    fn envelope_uses_stable_code() {
        let v = error_envelope(&Error::LibraryBusy("/lib".into()));
        assert_eq!(v["error"]["code"], "library_busy");
        assert!(v["error"]["message"].as_str().unwrap().contains("/lib"));
    }
}
