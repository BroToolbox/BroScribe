//! Command-line grammar (design §11) and the argv normalisation in front of
//! it ([`parse`], the one entry point `run` uses).
//!
//! Sensitive text the app sends (names, aliases, e-mails, notes, titles,
//! directory queries, calendar IDs and routing identifiers, import, manifest,
//! contacts and export paths, updater feed and keys) never goes in argv. A
//! command carrying such values takes `--spec-stdin`: exactly one JSON
//! object on stdin whose type is
//! named in the flag's doc, unknown fields refused (read with
//! `query::read_stdin`). Commands that already had a stdin transport keep it
//! (`--stdin`, `--query-stdin`, `--fingerprints-stdin`, `--fence-stdin`,
//! `--name-stdin`, `--instructions-stdin`, `--client-secret-stdin`). A
//! stdin flag conflicts with every argv form of the values it carries, so a
//! mix is a usage error, never a silent choice.

use std::ffi::OsString;
use std::path::PathBuf;

use broscribe_core::calendar::CalendarTarget;
use broscribe_core::config::ReadingSize;
use broscribe_core::index::MAX_PAGE_SIZE;
use broscribe_core::queue::PruneCriteria;
use clap::error::ErrorKind;
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use serde::Deserialize;

#[derive(Debug, Parser)]
#[command(name = "broscribe", version, about = "Local-first meeting memory")]
pub(crate) struct Cli {
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,
    /// Library folder (overrides BROSCRIBE_LIBRARY and the config).
    #[arg(long, global = true, value_name = "PATH")]
    pub library: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

/// What [`parse`] made of a command line.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "The common parsed command stays inline; boxing would add an allocation to every invocation."
)]
pub(crate) enum Parsed {
    /// A command to run.
    Run(Cli),
    /// `--help` or `--version`: print this text on stdout and exit 0.
    Display(String),
    /// A command line the grammar refuses (exit 2).
    Usage(UsageError),
}

/// A refused command line, a missing subcommand included. Under `--json`
/// it is reported as the error envelope of
/// `Error::InvalidInput(message)` on stdout (code `invalid_input`), like
/// every other error; otherwise `rendered` goes to stderr.
#[derive(Debug)]
pub(crate) struct UsageError {
    /// `--json` was given as a global option (before any `--`).
    pub json: bool,
    /// One line, for the envelope.
    pub message: String,
    /// clap's full text (the usage line, or the help of a command given
    /// without its subcommand), for stderr.
    pub rendered: String,
}

/// Parses `args` (program name first) after [`normalize`]ing the global
/// options.
pub(crate) fn parse(args: Vec<OsString>) -> Parsed {
    let (args, json) = match normalize(args) {
        Ok(normalized) => normalized,
        Err(usage) => return Parsed::Usage(usage),
    };
    let e = match Cli::try_parse_from(args) {
        Ok(cli) => return Parsed::Run(cli),
        Err(e) => e,
    };
    let rendered = e.to_string();
    let message = match e.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => return Parsed::Display(rendered),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand | ErrorKind::MissingSubcommand => {
            "a subcommand is required (see --help)".to_owned()
        }
        _ => {
            let first = rendered.lines().next().unwrap_or("invalid arguments");
            first.strip_prefix("error: ").unwrap_or(first).to_owned()
        }
    };
    Parsed::Usage(UsageError {
        json,
        message,
        rendered,
    })
}

/// Moves the global options (`--json`, `--library <path>`,
/// `--library=<path>`) before the subcommand, in order and as often as
/// given, so clap still refuses a repeat and a variadic value list that
/// accepts leading hyphens (`tag`, `record tag`) never swallows a trailing
/// global option. For the same reason `record tag`'s own `--session <id>`
/// (or `--session=<id>`) moves before its changes, in order, so a trailing
/// one targets that session (never the active recording with
/// `--session` among the tags) and a repeat is still refused. Stops at the
/// first `--`: what follows is literal, never an option. In `tag` and
/// `record tag` that terminator moves before their values (clap would take
/// a `--` after the first value as a value), so the values before it and
/// everything after it, a later `--` included, are values. An option
/// missing its value (last, or `--` next) is refused here, since a variadic
/// list would otherwise take the bare flag as a value. Returns whether
/// `--json` was given.
fn normalize(args: Vec<OsString>) -> Result<(Vec<OsString>, bool), UsageError> {
    let mut args = args.into_iter();
    let program = args.next();
    let mut globals = Vec::new();
    let mut sessions = Vec::new();
    let mut rest = Vec::with_capacity(args.len());
    let mut json = false;
    while let Some(arg) = args.next() {
        let values = variadic_values(&rest);
        let record_tag = values == Some(2);
        if arg == "--" {
            match values {
                // Help asked before the values stays an option.
                Some(start) => {
                    let at = start
                        + rest[start..]
                            .iter()
                            .take_while(|a| *a == "--help" || *a == "-h")
                            .count();
                    rest.insert(at, arg);
                }
                None => rest.push(arg),
            }
            rest.extend(args.by_ref());
        } else if arg == "--json" {
            json = true;
            globals.push(arg);
        } else if arg == "--library" || (record_tag && arg == "--session") {
            let into = if arg == "--library" {
                &mut globals
            } else {
                &mut sessions
            };
            match args.next() {
                Some(value) if value != "--" => {
                    // Clap still rejects the missing value, but this token
                    // requested a JSON refusal before any literal terminator.
                    json |= value == "--json";
                    into.extend([arg, value]);
                }
                _ => return Err(missing_value(&arg, json)),
            }
        } else if arg.to_str().is_some_and(|a| a.starts_with("--library=")) {
            globals.push(arg);
        } else if record_tag && arg.to_str().is_some_and(|a| a.starts_with("--session=")) {
            sessions.push(arg);
        } else {
            rest.push(arg);
        }
    }
    if !sessions.is_empty() {
        // `--session` is only collected in `record tag`: before its values.
        let values = rest.split_off(2);
        rest.extend(sessions);
        rest.extend(values);
    }
    Ok((
        program.into_iter().chain(globals).chain(rest).collect(),
        json,
    ))
}

/// Where the arguments of a command ending in a variadic value list that
/// accepts leading hyphens start in `rest` (the non-global arguments seen
/// so far): after `record tag`, or after `tag` (its meeting ID, then the
/// changes).
fn variadic_values(rest: &[OsString]) -> Option<usize> {
    match rest {
        [record, tag, ..] if record == "record" && tag == "tag" => Some(2),
        [tag, ..] if tag == "tag" => Some(1),
        _ => None,
    }
}

/// The refusal of `option` given without its value (clap's wording).
fn missing_value(option: &OsString, json: bool) -> UsageError {
    let value = if option == "--library" {
        "PATH"
    } else {
        "SESSION_ID"
    };
    let message = format!(
        "a value is required for '{} <{value}>' but none was supplied",
        option.to_string_lossy()
    );
    UsageError {
        json,
        rendered: format!("error: {message}\n\nFor more information, try '--help'.\n"),
        message,
    }
}

#[derive(Debug, Args)]
pub(crate) struct UninstallArgs {
    /// Show the plan without changing anything.
    #[arg(long, conflicts_with = "yes")]
    pub dry_run: bool,
    /// Apply the plan.
    #[arg(long)]
    pub yes: bool,
    /// Remove BroScribe-created files from discovered data folders. Keep other files.
    #[arg(long)]
    pub remove_data: bool,
    /// Delete the selected profile's library (and those explicitly named below).
    #[arg(long)]
    pub remove_library: bool,
    /// Allow deletion of offline unsaved and processing journals with the data.
    #[arg(long, requires = "remove_data")]
    pub discard_unsaved: bool,
    /// Also inspect this data folder. May be repeated.
    #[arg(long, value_name = "PATH")]
    pub data_dir: Vec<PathBuf>,
    /// Remove credentials in every BroScribe profile namespace.
    #[arg(long)]
    pub all_profiles_secrets: bool,
    /// Skip signing out and removing credentials if this account never stored any.
    #[arg(long, conflicts_with_all = ["all_profiles_secrets", "revoke_at_google", "from_settings"])]
    pub skip_credentials: bool,
    /// Revoke Google grants before removing the local credentials.
    #[arg(long)]
    pub revoke_at_google: bool,
    /// Keep the app installation in place.
    #[arg(long)]
    pub keep_app: bool,
    #[arg(long, hide = true)]
    pub from_settings: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Print the version.
    Version,
    /// Offline maintenance control for this data directory.
    Maintenance {
        #[command(subcommand)]
        action: MaintenanceCmd,
    },
    /// Export counts-only diagnostics without meeting content or identifiers.
    Diagnostics,
    /// Preview or remove BroScribe and its integrations.
    Uninstall(UninstallArgs),
    /// Import a recording, a folder of recordings or a manifest.
    Import(ImportArgs),
    /// Search transcripts; results carry meeting, timestamp and speaker.
    Search(SearchArgs),
    /// Find meetings by title, tags or participants; without words or
    /// filters, the most recent meetings.
    Meetings(MeetingsArgs),
    /// Read or check a saved meeting's review notes.
    Meeting {
        #[command(subcommand)]
        action: MeetingCmd,
    },
    /// Show a meeting's record.
    Show {
        /// Meeting ID (m-…).
        meeting: String,
    },
    /// Print a meeting's transcript, or correct who said given lines.
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Transcript {
        #[command(subcommand)]
        action: Option<TranscriptCmd>,
        #[command(flatten)]
        args: TranscriptArgs,
    },
    /// List people, or show / forget one.
    People {
        #[command(subcommand)]
        action: Option<PeopleCmd>,
    },
    /// Review and name speakers.
    Speakers {
        #[command(subcommand)]
        action: SpeakersCmd,
    },
    /// Add (+tag or tag) and remove (-tag) meeting tags.
    Tag {
        /// Meeting ID (m-…).
        meeting: String,
        /// `+tag` or `tag` adds, `-tag` removes.
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        changes: Vec<String>,
    },
    /// Rename a meeting; the title is the user's and wins over calendar or
    /// window titles. `--stdin` reads it from stdin (never in argv).
    Title {
        /// Meeting ID (m-…).
        meeting: String,
        /// The title; omit with `--stdin`.
        #[arg(required_unless_present = "stdin", conflicts_with = "stdin")]
        title: Option<String>,
        #[arg(long)]
        stdin: bool,
    },
    /// Delete a meeting permanently: transcript, generated results, owned
    /// recording files, voice samples learned from it and caches. People,
    /// referenced originals outside the library and exports stay.
    #[command(group(ArgGroup::new("mode").required(true).args(["dry_run", "yes"])))]
    Delete {
        /// Meeting ID (m-…).
        meeting: String,
        /// Report what would be deleted; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Delete (cannot be undone).
        #[arg(long)]
        yes: bool,
    },
    /// Speaker identity tools.
    Identity {
        #[command(subcommand)]
        action: IdentityCmd,
    },
    /// Model registry: list, pull, use, doctor.
    Models {
        #[command(subcommand)]
        action: ModelsCmd,
    },
    /// The derived search index.
    Index {
        #[command(subcommand)]
        action: IndexCmd,
    },
    /// Setup assistant (interactive on a terminal, flag-driven otherwise),
    /// plus `setup voice` (enrollment) and `setup try` (a test transcription).
    #[command(args_conflicts_with_subcommands = true)]
    Setup {
        #[command(subcommand)]
        action: Option<Box<SetupCmd>>,
        #[command(flatten)]
        args: SetupArgs,
    },
    /// Read or change application preferences.
    Settings {
        #[command(subcommand)]
        action: SettingsCmd,
    },
    /// Check configuration, library, models, voice enrollment and decoders.
    Doctor,
    /// Manage the background recording agent.
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
    /// Control the active recording.
    Record {
        #[command(subcommand)]
        action: RecordCmd,
    },
    /// Meeting-app detection and recording policies.
    Detect {
        #[command(subcommand)]
        action: DetectCmd,
    },
    /// Calendar and directory sign-in.
    Integrations {
        #[command(subcommand)]
        action: IntegrationsCmd,
    },
    /// Play a synthetic signal and measure microphone leakage.
    AudioCheck {
        /// Complete the setup step after a successful check. Saves echo handling
        /// only when delay and leakage were measured; otherwise keeps the
        /// existing policy and reports saved:false, setup_completed:true.
        #[arg(long)]
        save: bool,
        /// Echo handling to save instead of the recommendation.
        #[arg(long, value_enum, requires = "save")]
        echo_handling: Option<EchoHandlingArg>,
    },
    /// Run the MCP server on stdio, or allow / refuse MCP access and
    /// register BroScribe with the detected agent CLIs.
    Mcp {
        #[command(subcommand)]
        action: Option<McpCmd>,
    },
    /// The `broscribe` link and the agent skill for AI agent CLIs.
    Agents {
        #[command(subcommand)]
        action: AgentsCmd,
    },
    /// The desktop app's own settings.
    App {
        #[command(subcommand)]
        action: AppCmd,
    },
    /// The job queue: run queued jobs, or show them.
    Queue {
        #[command(subcommand)]
        action: QueueCmd,
    },
    /// Audio devices.
    Audio {
        #[command(subcommand)]
        action: AudioCmd,
    },
    /// Recording retention: apply the policy, or keep / delete one recording.
    Retention {
        #[command(subcommand)]
        action: RetentionCmd,
    },
    /// Summarise meetings with an enabled LLM provider: summary, topics,
    /// keywords, tags, decisions and action items; `enrich instructions`
    /// shows or sets what summaries should focus on.
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Enrich {
        #[command(subcommand)]
        action: Option<EnrichCmd>,
        #[command(flatten)]
        args: EnrichArgs,
    },
    /// Meeting templates (meeting notes, follow-up e-mail, BANT, …): list,
    /// define, run on a meeting through a consented provider, read results.
    Templates {
        #[command(subcommand)]
        action: TemplatesCmd,
    },
    /// Write a meeting's transcript and generated content as Markdown,
    /// plain text or JSON.
    Export(ExportArgs),
    /// Action items of enriched meetings, or set one's status.
    #[command(args_conflicts_with_subcommands = true)]
    Actions {
        #[command(subcommand)]
        action: Option<ActionsCmd>,
        #[command(flatten)]
        args: ActionsArgs,
    },
    /// Decisions of enriched meetings.
    Decisions(DecisionsArgs),
    /// List, configure and test LLM and remote text-embedding providers.
    Providers {
        #[command(subcommand)]
        action: ProvidersCmd,
    },
    /// Run recognition, diarization and identity again on a meeting's
    /// recording, keeping confirmed names and user-set statuses.
    Reprocess(ReprocessArgs),
    /// Compare models on a local labelled set of meeting excerpts.
    Bench {
        #[command(subcommand)]
        action: crate::cmd::bench::BenchCmd,
    },
    /// Meeting-app readers: a redacted accessibility structure dump for
    /// writing reader rules (no names, captions or titles).
    Readers {
        #[command(subcommand)]
        action: crate::cmd::readers::ReadersCmd,
    },
    /// Library integrity: `check` reports what is pending or unreadable;
    /// `repair` fixes only the scopes asked for.
    Library {
        #[command(subcommand)]
        action: LibraryCmd,
    },
    /// Signed updates from GitHub Releases by default. Checks are on by
    /// default and can be turned off in Settings. Installation requires confirmation.
    ///
    /// Publisher commands: `release-key new`, `sign-manifest --key-file`,
    /// and `verify-manifest`.
    Update {
        #[command(subcommand)]
        action: UpdateCmd,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum MaintenanceCmd {
    /// Show whether this profile is in maintenance mode.
    Status,
    /// Request offline maintenance for this profile.
    Enable,
    /// Clear the offline maintenance request for this profile.
    Disable,
}

#[derive(Debug, Subcommand)]
pub(crate) enum LibraryCmd {
    /// Switch to another library after recording and processing are idle.
    Switch {
        path: PathBuf,
        /// Seconds to wait for active work and restart confirmation.
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
    },
    /// List tags and their meeting counts and apps used in the library.
    Filters,
    /// Measure library size and free space on its disk.
    DiskUsage,
    /// Read-only integrity check: index repairs pending, unfinished
    /// deletions and discards, unpublished meeting folders, unusable voice
    /// samples and unreadable files. Identifiers and counts only, never
    /// names, titles or paths.
    Check,
    /// Repair what `library check` reports, in the named scopes only.
    #[command(group(ArgGroup::new("mode").required(true).args(["dry_run", "yes"])))]
    Repair {
        /// Scopes, comma-separated (required: nothing is repaired implicitly).
        #[arg(
            long,
            value_enum,
            value_delimiter = ',',
            value_name = "SCOPES",
            required = true
        )]
        only: Vec<RepairScopeArg>,
        /// Report what would be repaired; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Repair.
        #[arg(long)]
        yes: bool,
    },
}

/// One `library repair --only` scope and the core repair it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum RepairScopeArg {
    /// Re-index what the index lists as pending; rebuild it when it is
    /// required, missing, incomplete or of another layout
    /// (`Index::repair_pending`).
    Index,
    /// Remove owned atomic-write leftovers of writers that are gone, in
    /// library, meeting, people, live, index, queue, deleting, discard and
    /// user-template folders (`lifecycle::sweep_orphan_temps`).
    Temps,
    /// Finish deletions whose marker remains
    /// (`lifecycle::finish_pending_deletions`).
    Deletions,
    /// Remove stored voice samples that do not decode (`ops::repair_identity`).
    Identity,
}

#[derive(Debug, Subcommand)]
pub(crate) enum UpdateCmd {
    /// The updater's state: off, idle, a staged download, an install
    /// awaiting confirmation, or a replacement to recover.
    Status,
    /// Fetch and verify the signed release envelope: offers a newer build
    /// for this installation; downloads nothing.
    Check,
    /// Download, verify and stage the offered build; nothing is replaced.
    Download,
    /// Delete the staged download.
    Discard,
    /// Replace the installation with the staged build. Refused while
    /// anything records or may start recording; the agent admits no capture
    /// until `confirm` or `rollback`.
    #[command(group(ArgGroup::new("install_consent").required(true).args(["yes", "expectation_stdin"])))]
    Install {
        /// The app and the agent restart on the new build (or give
        /// `--expectation-stdin`; exactly one is required).
        #[arg(long)]
        yes: bool,
        /// Read the confirmed update from stdin, one `UpdateExpectation`
        /// (`{"from", "to", "signer"}`, the app's path); refused unless the
        /// staged update still matches it.
        #[arg(long)]
        expectation_stdin: bool,
    },
    /// The new build started and works: remove the previous copy.
    Confirm,
    /// Restore the previous installation `install` kept.
    #[command(group(ArgGroup::new("rollback_consent").required(true).args(["yes", "expectation_stdin"])))]
    Rollback {
        /// The app and the agent restart on the previous build (or give
        /// `--expectation-stdin`; exactly one is required).
        #[arg(long)]
        yes: bool,
        /// Read the confirmed rollback from stdin, one `UpdateExpectation`
        /// (`{"from", "to", "signer"}`, the app's path); refused unless the
        /// kept installation still matches it.
        #[arg(long)]
        expectation_stdin: bool,
    },
    /// The running agent's install admission, held across a manual
    /// replacement of the installation.
    Admission {
        #[command(subcommand)]
        action: AdmissionCmd,
    },
    /// Finish or undo a replacement an interrupted process left behind.
    Recover,
    /// The `[updates]` settings.
    Settings {
        #[command(subcommand)]
        action: UpdateSettingsCmd,
    },
    /// Manage release signing keys. Commands print public key data only.
    ReleaseKey {
        #[command(subcommand)]
        action: ReleaseKeyCmd,
    },
    /// Sign a release manifest read whole from stdin (its exact bytes, never
    /// trimmed or re-serialised) and print the signed envelope and its signers.
    #[command(group(ArgGroup::new("signing_source").required(true).args(["keys", "key_file"])))]
    SignManifest {
        /// Label of a stored release key (repeatable).
        #[arg(long = "key", value_name = "LABEL")]
        keys: Vec<String>,
        /// Raw 32-byte Ed25519 seed in a mode-0600 POSIX file.
        #[arg(long, value_name = "PATH")]
        key_file: Option<PathBuf>,
        /// Read the manifest from stdin (required).
        #[arg(long, required = true)]
        manifest_stdin: bool,
    },
    /// Verify an envelope from stdin with the compiled release public keys.
    VerifyManifest {
        #[arg(long, required = true)]
        envelope_stdin: bool,
        /// Accept an expired, authentic release envelope as a renewal baseline.
        #[arg(long)]
        allow_expired: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum AdmissionCmd {
    /// Take the running agent's install admission (refused while anything
    /// records); prints the fence that releases it.
    Acquire,
    /// Give back the install admission `acquire` took.
    Release {
        /// The fence `acquire` printed.
        #[arg(long)]
        fence: u64,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ReleaseKeyCmd {
    /// Create a raw 32-byte release seed in a new mode-0600 POSIX file.
    New {
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
    },
    /// Generate a release key under LABEL (1 to 32 of a-z, 0-9, `-`) and
    /// print its public key; refused when one exists unless
    /// `--replace-existing`.
    Generate {
        #[arg(long, value_name = "LABEL")]
        label: String,
        #[arg(long)]
        replace_existing: bool,
    },
    /// The public key and key ID stored under LABEL.
    Show {
        #[arg(long, value_name = "LABEL")]
        label: String,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum UpdateSettingsCmd {
    /// Print the `[updates]` settings and the configuration revision.
    Show,
    /// Change the check preference or replace the whole `[updates]` table.
    #[command(group(ArgGroup::new("settings_change").required(true).args(["spec_stdin", "enabled"])))]
    Set {
        /// Read the validated settings from stdin.
        #[arg(long)]
        spec_stdin: bool,
        /// Change only automatic checks, preserving source and trust overrides.
        #[arg(long, value_name = "BOOL")]
        enabled: Option<bool>,
        /// Save only while the configuration is still at this revision (as
        /// `show` returned it); `config_stale` otherwise.
        #[arg(long, value_name = "REVISION")]
        expect_revision: Option<u64>,
    },
}

#[derive(Debug, Default, Args)]
pub(crate) struct EnrichArgs {
    /// Meeting ID (m-…).
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub meeting: Option<String>,
    /// Every transcribed meeting without a summary (or with one that
    /// predates changes to the transcript).
    #[arg(long)]
    pub all: bool,
    /// Use only this provider (default: the profile's llm list).
    #[arg(long, value_name = "ID")]
    pub provider: Option<String>,
    /// Model for this run only (never saved; needs --provider): one the
    /// provider lists (`providers models <id>`) or any name it accepts.
    #[arg(long, value_name = "MODEL", requires = "provider")]
    pub model: Option<String>,
    /// Queue the enrichment for the background worker and return its job
    /// ID instead of waiting.
    #[arg(long, conflicts_with_all = ["all", "yes"])]
    pub queue: bool,
    /// Confirm `--all` without asking (required under --json or without a terminal).
    #[arg(long)]
    pub yes: bool,
    /// Instructions for this run only, read as text from stdin (never
    /// argv); empty text means none. Without it the saved default applies.
    #[arg(long)]
    pub instructions_stdin: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum EnrichCmd {
    /// Restore the predecessor of the displayed summary.
    Restore {
        meeting: String,
        /// Creation time of the displayed result (RFC 3339).
        #[arg(long)]
        expect: chrono::DateTime<chrono::FixedOffset>,
    },
    /// The saved default instructions for summaries.
    Instructions {
        #[command(subcommand)]
        action: InstructionsCmd,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum InstructionsCmd {
    /// Print the saved instructions.
    Show,
    /// Save the instructions read as text from stdin.
    Set {
        /// Read the text from stdin (required: instructions never go in argv).
        #[arg(long, required = true)]
        stdin: bool,
    },
    /// Remove the saved instructions.
    Clear,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TemplatesCmd {
    /// Restore the predecessor of the displayed template result.
    Restore {
        meeting: String,
        id: String,
        /// Creation time of the displayed result (RFC 3339).
        #[arg(long)]
        expect: chrono::DateTime<chrono::FixedOffset>,
    },
    /// Built-in and user templates, with whether each runs automatically;
    /// with `--meeting`, also that meeting's stored results.
    List {
        /// Also list this meeting's results (m-…).
        #[arg(long, value_name = "MEETING")]
        meeting: Option<String>,
    },
    /// One template with its instructions.
    Show {
        /// Template ID.
        id: String,
    },
    /// Define a template from `{"name", "description", "body"}` on stdin.
    New {
        /// Template ID: lowercase letters, digits and hyphens.
        id: String,
        /// Read the definition from stdin (required).
        #[arg(long, required = true)]
        stdin: bool,
    },
    /// Change a template from `{"name", "description", "body"}` on stdin
    /// (fields left out keep their value); editing a built-in saves an
    /// override.
    Edit {
        /// Template ID.
        id: String,
        /// Read the changes from stdin (required).
        #[arg(long, required = true)]
        stdin: bool,
    },
    /// Delete a user template (results already generated stay).
    Delete {
        /// Template ID.
        id: String,
    },
    /// Restore a built-in template's own text.
    Reset {
        /// Template ID.
        id: String,
    },
    /// Templates that run after each meeting's summary.
    Auto {
        #[command(subcommand)]
        action: TemplatesAutoCmd,
    },
    /// Run a template on a meeting through a consented provider.
    Run {
        /// Meeting ID (m-…).
        meeting: String,
        /// Template ID.
        id: String,
        /// Use only this provider (default: the profile's llm list).
        #[arg(long, value_name = "ID")]
        provider: Option<String>,
        /// Model for this run only (needs --provider).
        #[arg(long, value_name = "MODEL", requires = "provider")]
        model: Option<String>,
        /// Queue the run for the background worker and return its job ID.
        #[arg(long)]
        queue: bool,
        /// Extra instructions for this run, read as text from stdin.
        #[arg(long)]
        instructions_stdin: bool,
    },
    /// A meeting's result for one template, names filled in.
    Result {
        /// Meeting ID (m-…).
        meeting: String,
        /// Template ID.
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum TemplatesAutoCmd {
    /// Run this template automatically after each meeting's summary.
    Add {
        /// Template ID.
        id: String,
    },
    /// Stop running this template automatically.
    Remove {
        /// Template ID.
        id: String,
    },
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, ValueEnum, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExportFormatArg {
    Md,
    Txt,
    Json,
}

#[derive(Debug, Args)]
pub(crate) struct ExportArgs {
    /// Meeting ID (m-…).
    pub meeting: String,
    /// Sections, comma-separated: transcript, summary, decisions, actions, notes,
    /// template:<id> (default: every one the meeting has).
    #[arg(long, value_delimiter = ',', value_name = "SECTIONS")]
    pub include: Vec<String>,
    /// md, txt or json.
    #[arg(long, value_enum, default_value = "md")]
    pub format: ExportFormatArg,
    /// Leave timestamps out of the transcript and items.
    #[arg(long)]
    pub no_timestamps: bool,
    /// Write to a file outside the library (mode 0600). Use `-` for stdout.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Read the destination from stdin instead, as an [`ExportSpec`], so the
    /// path never appears in argv (the app's path).
    #[arg(long, conflicts_with = "out")]
    pub spec_stdin: bool,
}

/// `export --spec-stdin` input.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExportSpec {
    /// As `--out`: a file written with mode 0600, or `-` for stdout.
    pub out: PathBuf,
}

/// Keyset paging of `meetings`, `search` and `actions`, the same pages MCP
/// and the app read (`Index::search_meetings_page`, `Index::search_page`,
/// `Index::list_actions_page`). With `--page-size` the output is one page:
/// the rows, `next_cursor` (absent on the last page) and the index
/// `revision` it was read at. `--cursor` continues after the page that
/// returned it; a cursor of another query or command is `cursor_mismatch`,
/// one read after any index change `cursor_stale`, one that does not decode
/// `invalid_cursor`: start again from the first page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Args)]
pub(crate) struct PageArgs {
    /// Rows per page, 1 to 500; the output becomes one page.
    #[arg(long, value_name = "N", value_parser = page_size, conflicts_with = "limit")]
    pub page_size: Option<usize>,
    /// Continue after the page that returned this cursor.
    #[arg(long, value_name = "CURSOR", requires = "page_size")]
    pub cursor: Option<String>,
}

fn page_size(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(n) if (1..=MAX_PAGE_SIZE).contains(&n) => Ok(n),
        _ => Err(format!("expected 1 to {MAX_PAGE_SIZE}")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ActionStatusArg {
    Open,
    Done,
    Dropped,
}

#[derive(Debug, Args)]
pub(crate) struct ActionsArgs {
    /// Only actions owned by `me` or by this person (ID, name, alias or e-mail).
    #[arg(long)]
    pub owner: Option<String>,
    /// Only actions with this status.
    #[arg(long, value_enum)]
    pub status: Option<ActionStatusArg>,
    /// Meetings since: 30d, 12h, 2w, YYYY-MM-DD or RFC 3339.
    #[arg(long)]
    pub since: Option<String>,
    /// Meetings before: same forms as --since (a date includes that day).
    #[arg(long)]
    pub until: Option<String>,
    /// Maximum number of results.
    #[arg(long)]
    pub limit: Option<usize>,
    #[command(flatten)]
    pub page: PageArgs,
    /// Only this meeting's actions (m-…).
    #[arg(long, value_name = "MEETING")]
    pub meeting: Option<String>,
    /// Words the action text must contain.
    #[arg(long)]
    pub query: Option<String>,
    /// Read the query and the owner from stdin instead, as one JSON object
    /// `{"query": "…", "owner": "…"}` (both optional), so they never appear
    /// in argv.
    #[arg(long, conflicts_with_all = ["query", "owner"])]
    pub query_stdin: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ActionsCmd {
    /// Correct an action's owner or due date.
    Set {
        meeting: String,
        action: String,
        /// Local person ID.
        #[arg(long, conflicts_with = "clear_owner")]
        owner: Option<broscribe_core::PersonId>,
        #[arg(long)]
        clear_owner: bool,
        /// New date (YYYY-MM-DD).
        #[arg(long, conflicts_with = "clear_due")]
        due: Option<chrono::NaiveDate>,
        #[arg(long)]
        clear_due: bool,
    },
    /// Set an action item's status; re-enrichment keeps it.
    SetStatus {
        /// Meeting ID (m-…).
        meeting: String,
        /// Action ID (as `actions` lists it).
        action: String,
        #[arg(value_enum)]
        status: ActionStatusArg,
    },
}

#[derive(Debug, Args)]
pub(crate) struct DecisionsArgs {
    /// Only meetings with a topic containing this.
    #[arg(long)]
    pub topic: Option<String>,
    /// Meetings since: 30d, 12h, 2w, YYYY-MM-DD or RFC 3339.
    #[arg(long)]
    pub since: Option<String>,
    /// Meetings before: same forms as --since (a date includes that day).
    #[arg(long)]
    pub until: Option<String>,
    /// Maximum number of results.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Only this meeting's decisions (m-…).
    #[arg(long, value_name = "MEETING")]
    pub meeting: Option<String>,
    /// Words the decision text must contain.
    #[arg(long)]
    pub query: Option<String>,
    /// Read the query from stdin instead, as one JSON object
    /// `{"query": "…"}`, so it never appears in argv.
    #[arg(long, conflicts_with = "query")]
    pub query_stdin: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum McpCmd {
    /// Allow MCP access and (re)register BroScribe with every detected agent
    /// CLI (claude, codex, gemini).
    Enable,
    /// Refuse MCP access and remove the registrations from detected agent CLIs.
    Disable,
    /// Whether MCP access is allowed, and the detected agent CLIs.
    Status,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AgentsCmd {
    /// The `~/.local/bin/broscribe` link, the skill files, detected agent
    /// CLIs and whether MCP access is allowed.
    Status,
    /// Link `~/.local/bin/broscribe` and write the BroScribe skill for agent
    /// CLIs; never replaces files BroScribe did not write.
    Install,
    /// Remove BroScribe's link and skill files (only those it wrote).
    Uninstall,
}

#[derive(Debug, Copy, Clone, ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AppPermissionArg {
    Notifications,
    Accessibility,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AppCmd {
    /// Read the running app's notification and Accessibility grants.
    Permissions {
        /// Ask the app to request this permission from the operating system.
        #[arg(long, value_enum)]
        request: Option<AppPermissionArg>,
    },
    /// Show the running app where it was left.
    Show,
    /// Start BroScribe at login. The window opens on desktops without a tray host.
    OpenAtLogin {
        #[arg(value_enum)]
        state: SwitchArg,
    },
    /// Show BroScribe in the macOS menu bar.
    StatusItem {
        #[arg(value_enum)]
        state: SwitchArg,
    },
    /// Show recording controls in compact mode.
    PanelCompact {
        #[arg(value_enum)]
        state: SwitchArg,
    },
    /// Hide Recording controls from supported screen sharing and screenshots.
    HideControlsFromScreenSharing {
        #[arg(value_enum)]
        state: SwitchArg,
    },
    /// Version, build number, architecture, signing state and licences of
    /// this installation.
    About,
    /// Report or set the text size used by BroScribe windows. Alias of settings text-size.
    ReadingSize {
        #[arg(value_enum)]
        size: Option<ReadingSizeArg>,
    },
    /// Live speaker identification while recording (provisional voices and
    /// suggested names). Off never stops capture or the live transcript.
    LiveIdentity {
        #[arg(value_enum)]
        state: SwitchArg,
    },
    /// Explicit opt-in to suggest your own person live while the speaker
    /// model has no calibrated thresholds. Suggestions only: nothing is named
    /// automatically, enrolled or recalibrated.
    UncalibratedMeSuggestions {
        #[arg(value_enum)]
        state: SwitchArg,
    },
}

/// `config::ReadingSize` steps, smallest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ReadingSizeArg {
    Small,
    Standard,
    Large,
    Larger,
    Largest,
}

impl From<ReadingSizeArg> for ReadingSize {
    fn from(size: ReadingSizeArg) -> Self {
        match size {
            ReadingSizeArg::Small => ReadingSize::Small,
            ReadingSizeArg::Standard => ReadingSize::Standard,
            ReadingSizeArg::Large => ReadingSize::Large,
            ReadingSizeArg::Larger => ReadingSize::Larger,
            ReadingSizeArg::Largest => ReadingSize::Largest,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum SwitchArg {
    On,
    Off,
    Status,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ProvidersCmd {
    /// Registered providers with their state and what was detected.
    List,
    /// Set an HTTP provider’s address and model without turning it on.
    Configure {
        id: String,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        model: String,
    },
    /// Put an enabled summary provider first in the active profile.
    Prefer { id: String },
    /// Enable a provider after showing what it receives.
    Enable {
        id: String,
        /// Accept the consent text (required under --json or without a terminal).
        #[arg(long)]
        accept: bool,
    },
    /// Disable a provider and withdraw its consent.
    Disable { id: String },
    /// Send one tiny invented prompt to check the provider works.
    Test { id: String },
    /// Store an HTTP provider's API key, read from stdin (pipe or redirect
    /// it; never an argument), in the system secret store.
    SetKey { id: String },
    /// Remove an HTTP provider's stored API key.
    DeleteKey { id: String },
    /// The provider's models, listed live by the provider itself (no
    /// meeting data is sent; needs the provider enabled with consent).
    Models { id: String },
}

#[derive(Debug, Args)]
pub(crate) struct ReprocessArgs {
    /// Meeting ID (m-…).
    pub meeting: String,
    /// Model profile for this run (default: the active one).
    #[arg(long)]
    pub profile: Option<String>,
    /// Recognise the text again; keep speakers and diarization.
    #[arg(long)]
    pub asr_only: bool,
    /// Queue the reprocessing for the background worker and return its job
    /// ID instead of running it here; a queued or running reprocessing of
    /// the meeting is `library_busy` either way.
    #[arg(long)]
    pub queue: bool,
}

fn parse_retention_policy(
    value: &str,
) -> std::result::Result<broscribe_core::config::Retention, String> {
    broscribe_core::config::Retention::parse_cli(value).map_err(|e| e.to_string())
}

#[derive(Debug, Subcommand)]
pub(crate) enum RetentionCmd {
    /// Delete the recordings the retention policy marks as due.
    Apply {
        /// Report what would be deleted; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Check what a proposed retention policy would delete without saving
        /// it (`keep`, `processed`, `processed-and-named`, or `days:<N>` with
        /// `N` from 1 to 3,650). Requires `--dry-run`.
        #[arg(
            long,
            requires = "dry_run",
            value_name = "POLICY",
            value_parser = parse_retention_policy
        )]
        policy: Option<broscribe_core::config::Retention>,
    },
    /// Save the library's recording retention policy (`keep`, `processed`,
    /// `processed-and-named`, or `days:<N>` with `N` from 1 to 3,650).
    /// A stricter policy previews deletions and requires --yes to save.
    Policy {
        /// Retention policy value.
        #[arg(value_name = "POLICY", value_parser = parse_retention_policy)]
        policy: broscribe_core::config::Retention,
        /// Preview deletions without changing the saved policy.
        #[arg(long, conflicts_with = "yes")]
        dry_run: bool,
        /// Save the policy after checking what it would delete.
        #[arg(long)]
        yes: bool,
    },
    /// Keep this meeting's recording whatever the policy says.
    Keep {
        /// Meeting ID (m-…).
        meeting: String,
    },
    /// Delete this meeting's recording now (transcript and names stay).
    /// `--dry-run` reports what would go, including a moved original that is
    /// the only copy of the recording; `--yes` deletes it.
    #[command(group(ArgGroup::new("mode").required(true).args(["dry_run", "yes"])))]
    DeleteNow {
        /// Meeting ID (m-…).
        meeting: String,
        /// Report what would be deleted; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Delete (cannot be undone).
        #[arg(long)]
        yes: bool,
    },
    /// Why each recording is kept or due (`retention::explain`): verdict,
    /// due date, the shown speakers holding it back and the queue jobs using
    /// it. Identifiers and reasons only; reads, never deletes.
    Explain {
        /// Only these meetings (m-…); default: every meeting.
        meetings: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum SearchModeArg {
    Auto,
    Keyword,
    Semantic,
    Hybrid,
}

/// What `search --in` looks through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum SearchInArg {
    /// What was said.
    Transcript,
    /// Template results (keyword only).
    Template,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, ValueEnum, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HandlingArg {
    /// Copy into the library.
    Copy,
    /// Keep in place.
    Reference,
    /// Move into the library.
    Move,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SettingsCmd {
    /// Report or set the text size used by BroScribe windows.
    TextSize { value: Option<TextSizeArg> },
    /// Report or set whether BroScribe's windows are light, dark or follow the system (the default).
    Theme { value: Option<ThemeArg> },
    /// Report or set the grouping thresholds for the active speaker model.
    SpeakerGrouping {
        #[command(subcommand)]
        preset: Option<SpeakerGroupingCmd>,
    },
    /// Report or change the library folder used on the next launch.
    LibraryFolder { path: Option<PathBuf> },
    /// Report or set the default meeting language. Use `auto` for detection.
    DefaultLanguage { value: Option<String> },
    /// Read the saved recognition words. Add words with `setup --words-file`.
    WordList,
    /// Report the last Settings pane and saved window position.
    Window,
    /// Default storage handling for imported recordings.
    ImportHandling { value: Option<HandlingArg> },
    /// Show a consent reminder when recording.
    ConsentReminder { value: Option<OnOff> },
    /// Summarise single imported recordings automatically when a provider is available.
    AutomaticSummaries { value: Option<OnOff> },
}

#[derive(Debug, Subcommand)]
pub(crate) enum SpeakerGroupingCmd {
    /// Suggest at 50% and merge automatically at 80%.
    Standard,
    /// Suggest at about 54% and merge automatically at 90%.
    Conservative,
    /// Set percentages from 0 to 100. An omitted threshold disables its action.
    Custom {
        #[arg(long, value_name = "PERCENT")]
        suggest_at: Option<f32>,
        #[arg(long, value_name = "PERCENT")]
        auto_merge_at: Option<f32>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TextSizeArg {
    Small,
    Standard,
    Large,
    Larger,
    Largest,
}

/// `config::ThemeChoice`.
#[derive(Clone, Copy, Debug, ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThemeArg {
    /// Follow the system's light or dark appearance.
    System,
    Light,
    Dark,
}

impl From<ThemeArg> for broscribe_core::config::ThemeChoice {
    fn from(theme: ThemeArg) -> Self {
        match theme {
            ThemeArg::System => Self::System,
            ThemeArg::Light => Self::Light,
            ThemeArg::Dark => Self::Dark,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OnOff {
    On,
    Off,
}

/// Echo handling names as saved in the config (`broscribe_dsp::EchoHandling`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, ValueEnum, serde::Deserialize, schemars::JsonSchema,
)]
#[value(rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub(crate) enum EchoHandlingArg {
    None,
    ResidueGating,
    FullAec,
}

impl EchoHandlingArg {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ResidueGating => "residue_gating",
            Self::FullAec => "full_aec",
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct ImportArgs {
    /// Inspect the file, folder or manifest without importing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Audio or video file, or a folder of them.
    #[arg(
        required_unless_present_any = ["manifest", "spec_stdin"],
        conflicts_with_all = ["manifest", "spec_stdin"]
    )]
    pub path: Option<PathBuf>,
    /// Import manifest (design §6.5).
    #[arg(long, value_name = "FILE")]
    pub manifest: Option<PathBuf>,
    /// Inline manifest supplied by MCP.
    #[arg(skip)]
    pub manifest_content: Option<serde_json::Value>,
    /// How the recording is stored (default from the config).
    #[arg(long, value_enum)]
    pub handling: Option<HandlingArg>,
    /// Language (BCP 47) stored on the meeting.
    #[arg(long, conflicts_with = "manifest")]
    pub lang: Option<String>,
    /// Detect the language instead of using the configured default.
    #[arg(long, conflicts_with_all = ["manifest", "lang"])]
    pub auto_language: bool,
    /// Meeting title (default: the file name).
    #[arg(long, conflicts_with_all = ["manifest", "spec_stdin"])]
    pub title: Option<String>,
    /// Start time: YYYY-MM-DD, "YYYY-MM-DD HH:MM" (local) or RFC 3339.
    #[arg(long, conflicts_with = "manifest")]
    pub date: Option<String>,
    /// A participant, "Name <email>", "Name" or an e-mail (repeatable).
    #[arg(
        long = "participant",
        value_name = "NAME <EMAIL>",
        conflicts_with_all = ["manifest", "spec_stdin"]
    )]
    pub participants: Vec<String>,
    /// Read the recording (path, title, participants) or the manifest from
    /// stdin instead, as an [`ImportSpec`], so no path appears in argv (the
    /// app's path).
    #[arg(long, conflicts_with = "manifest")]
    pub spec_stdin: bool,
    /// Enrich the imported meetings (needs an LLM provider enabled with
    /// consent; without the flag, single files follow `auto_enrich`).
    #[arg(long)]
    pub enrich: bool,
}

/// `import --spec-stdin` input: the values of the argv forms it replaces,
/// read from one JSON object with exactly one of `path` (with its optional
/// `title` and `participants`) and `manifest` (alone: a manifest carries its
/// own titles and participants, and `--lang` and `--date` are refused beside
/// it as in argv). A relative path resolves against the working directory,
/// as in argv.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "ImportSpecFields")]
pub(crate) enum ImportSpec {
    /// As the PATH argument (a file or a folder), `--title` and
    /// `--participant`.
    Recording {
        path: PathBuf,
        title: Option<String>,
        participants: Vec<String>,
    },
    /// As `--manifest`.
    Manifest(PathBuf),
}

/// The JSON fields of an [`ImportSpec`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportSpecFields {
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    manifest: Option<PathBuf>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    participants: Vec<String>,
}

impl TryFrom<ImportSpecFields> for ImportSpec {
    type Error = &'static str;

    fn try_from(f: ImportSpecFields) -> Result<Self, Self::Error> {
        match (f.path, f.manifest) {
            (Some(path), None) => Ok(Self::Recording {
                path,
                title: f.title,
                participants: f.participants,
            }),
            (None, Some(manifest)) if f.title.is_none() && f.participants.is_empty() => {
                Ok(Self::Manifest(manifest))
            }
            (None, Some(_)) => {
                Err("a manifest carries its own titles and participants; give only \"manifest\"")
            }
            _ => Err("give exactly one of \"path\" and \"manifest\""),
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct SearchArgs {
    /// Words to find (all must match in keyword mode). Optional: the
    /// filters alone list matching utterances.
    pub query: Option<String>,
    /// Search mode: auto, keyword, semantic or hybrid. Auto picks hybrid when
    /// text vectors are available and keyword otherwise (the default).
    #[arg(long, value_enum)]
    pub mode: Option<SearchModeArg>,
    /// transcript (default) or template: template results, keyword only,
    /// by words alone (no other filter).
    #[arg(long = "in", value_enum, default_value = "transcript")]
    pub scope: SearchInArg,
    /// Only utterances by this person (ID, name, alias or e-mail).
    #[arg(long)]
    pub person: Option<String>,
    /// Read sensitive query fields from stdin as one JSON object with
    /// optional query, person, tag and topic strings.
    #[arg(long, conflicts_with_all = ["query", "person"])]
    pub query_stdin: bool,
    /// Only meetings with this tag.
    #[arg(long)]
    pub tag: Option<String>,
    /// Only meetings with an enrichment topic containing this.
    #[arg(long)]
    pub topic: Option<String>,
    /// Only meetings from this detector app ID.
    #[arg(long)]
    pub app: Option<String>,
    /// Only recorded or imported meetings.
    #[arg(long, value_enum)]
    pub source: Option<MeetingSourceArg>,
    /// Meetings since: 30d, 12h, 2w, YYYY-MM-DD or RFC 3339.
    #[arg(long)]
    pub since: Option<String>,
    /// Meetings before: same forms as --since (a date includes that day).
    #[arg(long)]
    pub until: Option<String>,
    /// Only utterances of this meeting (m-…).
    #[arg(long, value_name = "MEETING")]
    pub meeting: Option<String>,
    /// Maximum number of results.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[command(flatten)]
    pub page: PageArgs,
}

#[derive(Debug, Subcommand)]
pub(crate) enum MeetingCmd {
    /// Check recording files and reconcile the saved availability.
    CheckRecording { meeting: String },
    /// Queue preparation of the recording's playback cache.
    PrepareAudio { meeting: String },
    /// Save a time range as a WAV clip outside the library.
    Clip {
        meeting: String,
        /// Start (seconds or HH:MM:SS).
        #[arg(long)]
        from: String,
        /// End (seconds or HH:MM:SS).
        #[arg(long)]
        to: String,
        /// Destination WAV file. An existing file is refused.
        #[arg(long)]
        out: PathBuf,
    },
    /// List the bookmarks and notes taken while recording, in recording-time order.
    Timeline { meeting: String },
    /// List review notes and their revisions, or check one.
    ReviewNotes {
        meeting: String,
        /// One-based position of the note to mark checked.
        #[arg(long)]
        dismiss: Option<usize>,
        /// Revision returned when the note was displayed.
        #[arg(long, requires = "dismiss")]
        expect: Option<String>,
    },
}

#[derive(Debug, Args)]
pub(crate) struct MeetingsArgs {
    /// Follow metadata hits with the best transcript match per remaining meeting.
    #[arg(long, conflicts_with = "page_size")]
    pub include_transcripts: bool,
    /// Date order when no text ranks the results.
    #[arg(long, value_enum)]
    pub order: Option<MeetingOrderArg>,
    /// Words to find in titles, tags and participants. Optional: without
    /// words or filters, the most recent meetings.
    pub query: Option<String>,
    /// Only meetings with this person (ID, name, alias or e-mail).
    #[arg(long)]
    pub person: Option<String>,
    /// Read sensitive query fields from stdin as one JSON object with
    /// optional query, person, tag and topic strings.
    #[arg(long, conflicts_with_all = ["query", "person"])]
    pub query_stdin: bool,
    /// Only meetings with this tag.
    #[arg(long)]
    pub tag: Option<String>,
    /// Only meetings with an enrichment topic containing this.
    #[arg(long)]
    pub topic: Option<String>,
    /// Only meetings from this detector app ID.
    #[arg(long)]
    pub app: Option<String>,
    /// Only recorded or imported meetings.
    #[arg(long, value_enum)]
    pub source: Option<MeetingSourceArg>,
    /// Meetings since: 30d, 12h, 2w, YYYY-MM-DD or RFC 3339.
    #[arg(long)]
    pub since: Option<String>,
    /// Meetings before: same forms as --since (a date includes that day).
    #[arg(long)]
    pub until: Option<String>,
    /// Maximum number of results.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[command(flatten)]
    pub page: PageArgs,
}

#[derive(Clone, Copy, Debug, ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MeetingOrderArg {
    Newest,
    Oldest,
}

impl From<MeetingOrderArg> for broscribe_core::index::MeetingOrder {
    fn from(order: MeetingOrderArg) -> Self {
        match order {
            MeetingOrderArg::Newest => Self::Newest,
            MeetingOrderArg::Oldest => Self::Oldest,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MeetingSourceArg {
    Recorded,
    Imported,
}

impl From<MeetingSourceArg> for broscribe_core::schema::Source {
    fn from(source: MeetingSourceArg) -> Self {
        match source {
            MeetingSourceArg::Recorded => Self::Recording,
            MeetingSourceArg::Imported => Self::Import,
        }
    }
}

#[derive(Debug, Default, Args)]
pub(crate) struct TranscriptArgs {
    /// Meeting ID (m-…).
    #[arg(required = true)]
    pub meeting: Option<String>,
    /// Only this speaker: S1, S2, … or a person.
    #[arg(long)]
    pub speaker: Option<String>,
    /// From this time (hh:mm:ss, mm:ss or seconds).
    #[arg(long)]
    pub from: Option<String>,
    /// Up to this time.
    #[arg(long)]
    pub to: Option<String>,
    /// Find literal text or speaker labels in saved lines. Read the query from stdin.
    #[arg(long)]
    pub find_stdin: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TranscriptCmd {
    /// Say who spoke these transcript segments, for these lines only (the
    /// speaker and its voice are unchanged), or clear that correction.
    #[command(group(ArgGroup::new("lines").required(true).args(["segments", "fingerprints_stdin"])))]
    Attribute {
        /// Meeting ID (m-…).
        meeting: String,
        /// Segment IDs (as `transcript --json` lists them).
        #[arg(long = "segment", value_name = "ID", num_args = 1..)]
        segments: Vec<u32>,
        /// Read the segments (and the person, so no name is in argv) from
        /// stdin instead, as `{"fingerprints":[{"track","start","end",
        /// "text_sha256"},…], "person": "…"}` (`person` omitted with
        /// --clear); refused when the transcript no longer has them.
        #[arg(long)]
        fingerprints_stdin: bool,
        /// Person ID, e-mail or name (a new name creates the person).
        #[arg(
            long,
            conflicts_with_all = ["clear", "fingerprints_stdin"],
            required_unless_present_any = ["clear", "fingerprints_stdin"]
        )]
        person: Option<String>,
        /// Show the speaker's name again on these lines.
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum PeopleCmd {
    /// Check confirmed identifications against independent voice samples.
    Recheck { person: Option<String> },
    /// Keep a name for the exact voice evidence returned by people recheck.
    KeepIdentification {
        #[arg(long, required = true)]
        spec_stdin: bool,
    },
    /// Review recurring voices and possible duplicate profiles.
    Review,
    /// Confirm a reviewed set of voice occurrences from a JSON spec.
    Identify {
        #[arg(long, required = true)]
        spec_stdin: bool,
    },
    /// Apply a reviewed profile merge from a JSON spec.
    Merge {
        #[arg(long, required_unless_present = "resume", conflicts_with = "resume")]
        spec_stdin: bool,
        /// Resume the merge already confirmed for this library.
        #[arg(long)]
        resume: bool,
    },
    /// Remember that two profiles are separate people (IDs, names, aliases or e-mails).
    NotSame { a: String, b: String },

    /// Create a person without voice data from name, optional emails and notes.
    Add {
        /// Read the JSON person fields from stdin.
        #[arg(long, required = true)]
        spec_stdin: bool,
    },
    /// List people (the default).
    List {
        /// Match a name, alias, e-mail or ID.
        query: Option<String>,
    },
    /// List the sources and playback ranges of a person's voice samples.
    Samples { person: String },
    /// Export a listed voice sample as a WAV clip outside the library.
    Clip {
        person: String,
        /// Sample ID from `people samples`.
        sample: String,
        /// Destination WAV file outside the library. An existing file is refused.
        #[arg(long)]
        out: PathBuf,
    },
    /// Show one person.
    Show { person: String },
    /// Search an enabled integration's directory.
    Lookup {
        /// Words to find; omit with `--query-stdin`.
        #[arg(
            required_unless_present = "query_stdin",
            conflicts_with = "query_stdin"
        )]
        query: Option<String>,
        /// Read the query from stdin instead, as a [`LookupSpec`], so it never
        /// appears in argv (the app's path).
        #[arg(long)]
        query_stdin: bool,
        #[arg(long, value_enum)]
        integration: Option<IntegrationArg>,
        /// Exact corporate account as PROVIDER/ACCOUNT_ID.
        #[arg(long)]
        account: Option<String>,
    },
    /// Cache a directory photo for an existing local person.
    Photo {
        person: String,
        /// Required for Microsoft photos: PROVIDER/ACCOUNT_ID.
        #[arg(long)]
        account: Option<String>,
    },
    /// Forget a person and remove their names from meetings. Requires --yes to write.
    Forget(ForgetArgs),
    /// Forget a person's voice samples and readings. Requires --yes to write.
    ForgetVoice(ForgetArgs),
    /// Edit a person's name, aliases, e-mails and notes.
    Edit(PeopleEditArgs),
    /// Undo one `people edit` by the `undo_id` it returned (only while
    /// nothing it changed was changed again). Forgetting a person or a voice
    /// has no undo.
    UndoEdit {
        /// Person ID, e-mail, name or alias.
        person: String,
        /// The edit's `undo_id` (pu-…).
        #[arg(long, value_name = "UNDO_ID")]
        undo_id: String,
    },
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("mode").required(true).args(["dry_run", "yes"])))]
pub(crate) struct ForgetArgs {
    /// Person ID, name, alias or e-mail.
    pub person: String,
    /// Show what goes, what changes and what stays.
    #[arg(long)]
    pub dry_run: bool,
    /// Confirm the permanent deletion.
    #[arg(long)]
    pub yes: bool,
}

/// `people lookup --query-stdin` input.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LookupSpec {
    pub query: String,
}

#[derive(Debug, Args)]
pub(crate) struct PeopleEditArgs {
    /// Person ID, e-mail, name or alias.
    pub person: String,
    /// New display name.
    #[arg(long)]
    pub name: Option<String>,
    /// `+alias` or `alias` adds, `-alias` removes (repeatable).
    #[arg(long = "alias", value_name = "[+|-]ALIAS", allow_hyphen_values = true)]
    pub aliases: Vec<String>,
    /// `+email` or `email` adds, `-email` removes (repeatable).
    #[arg(long = "email", value_name = "[+|-]EMAIL", allow_hyphen_values = true)]
    pub emails: Vec<String>,
    /// Replace the notes with this file's text (an empty file clears them).
    #[arg(long, value_name = "FILE")]
    pub notes_file: Option<PathBuf>,
    /// Read the whole edit from stdin instead, as `ops::PersonEditSpec`
    /// (name, alias and e-mail changes, notes and the optional `expected`
    /// revision the caller saw), so nothing typed appears in argv (the app's
    /// path). The result carries the new `revision` and an `undo_id`.
    #[arg(long, conflicts_with_all = ["name", "aliases", "emails", "notes_file"])]
    pub spec_stdin: bool,
    /// Commit only while the directory account remains at this generation.
    /// Google: google:N. Microsoft: PROVIDER/ACCOUNT_ID:N.
    /// A sign-out or replacement waits for the commit or prevents it.
    #[arg(long, value_name = "ACCOUNT:GENERATION")]
    pub expect_generation: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SpeakersCmd {
    /// Speakers waiting for a decision, most talk time first; speakers
    /// dismissed with "can't tell" are left out and counted.
    Review {
        #[arg(long)]
        limit: Option<usize>,
        /// List only the speakers dismissed from review (restore with
        /// `speakers restore`).
        #[arg(long)]
        dismissed: bool,
    },
    /// The lines of a speaker worth playing to recognise the voice, best
    /// first (the app plays the same clips).
    Samples {
        meeting: String,
        /// S1, S2, … (a merged member's ID means its group).
        speaker: String,
    },
    /// "Can't tell": take a speaker out of the review queue without naming
    /// or rejecting anyone; nothing else changes.
    Dismiss {
        meeting: String,
        /// S1, S2, … (a merged member's ID dismisses its group).
        speaker: String,
        /// As for `merge`.
        #[arg(long)]
        fence_stdin: bool,
    },
    /// Undo `speakers dismiss`: the speaker returns to the review queue.
    Restore {
        meeting: String,
        /// S1, S2, …
        speaker: String,
        /// As for `merge`.
        #[arg(long)]
        fence_stdin: bool,
    },
    /// Name a speaker. Saved at once; other speakers (this meeting first,
    /// then the rest of the library) are rechecked in the background by
    /// the agent's queue (`identity status` shows the result).
    Name {
        meeting: String,
        /// S1, S2, …
        speaker: String,
        /// Person ID, e-mail or name (a new name creates the person).
        person: String,
        /// Refuse if the speaker revision from `show --json` has changed.
        #[arg(long, value_name = "REVISION")]
        expect: Option<String>,
    },
    /// Reject a suggestion (default: the current suggestion or auto-name).
    Reject {
        meeting: String,
        speaker: String,
        person: Option<String>,
    },
    /// People whose stored voice is closest to a speaker's, best first
    /// (raw similarity, not a probability).
    Matches {
        meeting: String,
        /// S1, S2, …
        speaker: String,
        /// Matches to list, 1 to 10.
        #[arg(long, default_value_t = 3)]
        limit: usize,
    },
    /// Show over-split speakers of one meeting as one voice (reversible
    /// with `speakers unmerge`): `<meeting> <kept> <member>…`. The kept
    /// speaker follows the rule (a confirmed or caption-proven member with
    /// the most talk time, else the most talk time), whatever was named.
    /// `--auto (--all | <meeting>…)` groups the speakers whose voices are at
    /// least the calibrated `auto_merge` similar.
    Merge {
        /// `<meeting> <kept> <member>…`; with `--auto`, the meetings.
        targets: Vec<String>,
        /// Group automatically by the `auto_merge` threshold.
        #[arg(long)]
        auto: bool,
        /// With `--auto`: every meeting of the library.
        #[arg(long, requires = "auto")]
        all: bool,
        /// Refuse unless the meeting still has the speakers the caller saw:
        /// `{"speaker_namespace", "grouping_revision"}` on stdin (the app's
        /// path).
        #[arg(long, conflicts_with = "auto")]
        fence_stdin: bool,
    },
    /// Undo a merge: the group's speakers show separately again, and are
    /// never merged automatically again.
    Unmerge {
        meeting: String,
        /// G2, … (`show` lists the groups).
        group: String,
        /// As for `merge`.
        #[arg(long)]
        fence_stdin: bool,
    },
    /// Remember that two speakers of a meeting are different people: never
    /// merged automatically nor suggested as duplicates again.
    NotSame {
        meeting: String,
        a: String,
        b: String,
        /// As for `merge`.
        #[arg(long)]
        fence_stdin: bool,
    },
    /// Speakers of one meeting that may be the same voice, best first
    /// (similarity between the calibrated `merge` and `auto_merge`).
    Duplicates { meeting: String },
    /// Live decisions awaiting review (partial, conflicting or unmapped),
    /// newest meeting first, as the app's review queue lists them
    /// (`live_identity::live_reviews`): `{"meetings": [...]}`. Read only;
    /// correct a line with `transcript attribute` or dismiss an item with
    /// `speakers live-dismiss`.
    LiveReview {
        /// Only this meeting (m-…).
        #[arg(long, value_name = "MEETING")]
        meeting: Option<String>,
    },
    /// Undo a speaker name carried over from the recording and remove its voice samples.
    UndoLiveName { meeting: String, speaker: String },
    /// Dismiss one live-review item as `live-review` showed it (its
    /// decision and `review_key`); nothing is named, corrected or taught.
    /// Refused with `stale_review` once the item changed.
    LiveDismiss {
        /// Meeting ID (m-…).
        meeting: String,
        #[arg(long)]
        decision: u64,
        #[arg(long, value_name = "KEY")]
        review_key: String,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum IdentityCmd {
    /// Apply, undo and redo identity edits in one stdin session (one JSON request per line).
    EditSession,
    /// Fit naming thresholds for the active embedding model from a manifest.
    Calibrate {
        #[arg(long, value_name = "FILE")]
        manifest: PathBuf,
    },
    /// Re-resolve every unconfirmed speaker of every finished meeting with
    /// the current naming thresholds (confirmations, rejections and review
    /// dismissals stay); meetings a job works on are skipped.
    ReResolve {
        /// Print what would change, writing nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// The background speaker recheck that naming, correcting or
    /// forgetting someone requests: progress and the latest committed
    /// counts per meeting (read only).
    Status,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ModelsCmd {
    /// Registered models with install state and active roles.
    List,
    /// Download and verify a model.
    Pull {
        id: String,
        #[arg(long)]
        allow_unverified_license: bool,
        /// Reinstall even when installed.
        #[arg(long)]
        force: bool,
        /// Cancel the download as soon as stdin reaches end of file (the
        /// app's path: it keeps stdin open as a pipe and closes it to
        /// cancel). The pull stops before its next read, removes what it
        /// staged, keeps an earlier install and fails with `cancelled`.
        #[arg(long)]
        cancel_on_stdin_eof: bool,
    },
    /// Make a model active for a role (vad, asr, diarization,
    /// speaker_embedding, text_embedding).
    Use {
        role: String,
        id: String,
        #[arg(long)]
        allow_unverified_license: bool,
    },
    /// Verify installed models (re-hashes their files); with `--live`, also
    /// load the profile's VAD and live model and run them over a synthetic
    /// fixture (`Registry::live_check`): nothing is downloaded and no
    /// recording is used.
    Doctor {
        #[arg(long)]
        live: bool,
        /// Profile whose live models `--live` checks (default: the active one).
        #[arg(long, requires = "live")]
        profile: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum IndexCmd {
    /// Rebuild the index from the library files.
    Rebuild,
}

#[derive(Debug, Subcommand)]
pub(crate) enum QueueCmd {
    /// Cancel a queued or running generation without changing its result.
    Cancel {
        job: broscribe_core::queue::JobId,
        /// The run token from the queue receipt. Defaults to the current run.
        #[arg(long)]
        run: Option<String>,
    },
    /// Run queued jobs of every kind until none is left, then exit.
    Work {
        /// One JSON object per line on stdout:
        /// {"event","job_id","kind","fraction","message"}.
        #[arg(long)]
        json_progress: bool,
        /// Run at most one job.
        #[arg(long)]
        once: bool,
    },
    /// Jobs in the queue with their state and attempts.
    Status,
    /// Remove setup Try-it imports (job and staged audio) left behind by a
    /// cancelled or killed setup step. Never touches `move` imports.
    DiscardSetup {
        /// Only the imports of this setup session (after its child was
        /// killed); without it, every import whose session ended.
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// Remove finished job folders (`Queue::prune`), oldest first. Jobs a
    /// holder may still use, ones holding the only copy of a moved original,
    /// failed jobs that will be retried or keep other files and unreadable
    /// records are kept and reported with their reason.
    Prune(PruneArgs),
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("mode").required(true).args(["dry_run", "yes"])))]
pub(crate) struct PruneArgs {
    /// Report what would be removed; change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Remove.
    #[arg(long)]
    pub yes: bool,
    /// Finished states to remove (repeatable; default: done). `failed` only
    /// takes jobs failed for good.
    #[arg(long = "state", value_enum, default_values_t = [PruneStateArg::Done])]
    pub states: Vec<PruneStateArg>,
    /// Only jobs last updated at least this long ago: 30d, 12h or 2w.
    #[arg(long, value_name = "AGE", value_parser = age)]
    pub older_than: Option<std::time::Duration>,
    /// Only jobs of this kind (repeatable; default: every kind).
    #[arg(long = "kind", value_name = "KIND")]
    pub kinds: Vec<String>,
    /// Move unreadable job records whole into the queue's quarantine folder
    /// (never deleted).
    #[arg(long)]
    pub quarantine_invalid: bool,
    /// At most this many folders per run; the rest is reported as
    /// `remaining`.
    #[arg(long, default_value_t = PruneCriteria::DEFAULT_LIMIT, value_parser = positive)]
    pub limit: usize,
}

impl PruneArgs {
    pub(crate) fn criteria(&self) -> PruneCriteria {
        PruneCriteria {
            done: self.states.contains(&PruneStateArg::Done),
            failed: self.states.contains(&PruneStateArg::Failed),
            min_age: self.older_than.unwrap_or_default(),
            kinds: self.kinds.clone(),
            quarantine_invalid: self.quarantine_invalid,
            limit: self.limit,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum PruneStateArg {
    Done,
    Failed,
}

fn age(s: &str) -> Result<std::time::Duration, String> {
    crate::args::parse_age(s).map_err(|e| e.to_string())
}

fn positive(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err("expected a positive number".to_owned()),
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum AudioCmd {
    /// Show the configured microphone pin and whether it is connected.
    PinStatus,
    /// Read the saved audio checks.
    CheckStatus,
    /// Audio input devices.
    Devices,
    /// Record with this microphone (an `audio devices` ID) while it is
    /// connected; `--clear` goes back to the system default.
    Pin {
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        device: Option<String>,
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum SetupCmd {
    /// Check the selected microphone's level with a two-second capture.
    MicTest,
    /// Show the count and size of kept voice readings.
    VoiceStatus,
    /// Estimate download size and report available disk space and memory.
    ModelCheck,
    /// Open the app without automatically running the setup assistant.
    Defer,
    /// Clear the saved deferral without changing completed setup steps.
    Resume,
    /// Save recording access after the agent's audio permissions have answers.
    RecordingAccess {
        /// Ask the recording agent to request one native audio permission.
        #[arg(long, value_enum)]
        request: Option<SetupAudioPermission>,
    },
    /// Run the setup audio check and save its result.
    AudioCheck {
        #[arg(long, value_enum)]
        echo_handling: Option<EchoHandlingArg>,
    },
    /// Continue with the current summary settings and provider choices.
    Summaries,
    /// Continue with the current meeting-app policies, optionally changing one.
    MeetingApps {
        #[arg(requires = "policy")]
        app: Option<String>,
        #[arg(value_enum, requires = "app")]
        policy: Option<DetectPolicyArg>,
    },
    /// Finish the assistant with the progress recorded so far.
    Finish,
    /// Enroll your voice: read the supplied text aloud for about 30 s (repeat
    /// it if you finish early), then make a separate confirmation reading
    /// with at least 8 s of detected speech. --mic captures the confirmation
    /// for 25 s; keep reading until it stops. Confirmation is optional with
    /// --from-file. Or pick yourself in a meeting (--from … --speaker …).
    /// --check-recording only checks the first recording.
    Voice(VoiceArgs),
    /// Transcribe a short clip with the active models to check they work.
    Try(TryArgs),
}

#[derive(Clone, Copy, Debug, ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SetupAudioPermission {
    Microphone,
    SystemAudio,
}

#[derive(Debug, Default, Args)]
#[command(group(
    ArgGroup::new("voice_source")
        .required(true)
        .args(["mic", "from", "from_file", "delete_enrollment", "show_passage"])
))]
pub(crate) struct VoiceArgs {
    /// Record the passage from a microphone.
    #[arg(long)]
    pub mic: bool,
    /// A meeting where you speak (with --speaker).
    #[arg(long, value_name = "MEETING", requires = "speaker")]
    pub from: Option<String>,
    /// Your speaker in that meeting: S1, S2, …
    #[arg(long, value_name = "S#", requires = "from")]
    pub speaker: Option<String>,
    /// A recording of you reading the passage. The original is kept.
    #[arg(long, value_name = "AUDIO")]
    pub from_file: Option<PathBuf>,
    /// Delete the kept passage recordings (voiceprints stay).
    #[arg(long)]
    pub delete_enrollment: bool,
    /// Print the passage and confirmation reading for --lang.
    #[arg(long)]
    pub show_passage: bool,
    /// Input device ID (--mic); with --from-file, recorded on the voiceprints.
    #[arg(long, value_name = "ID")]
    pub device: Option<String>,
    /// Seconds to record with --mic (at most 45).
    #[arg(long, default_value_t = 30.0)]
    pub seconds: f64,
    /// Only this part of --from-file: start-end seconds.
    #[arg(long, requires = "from_file")]
    pub range: Option<String>,
    /// A separate confirmation reading with at least 8 s of detected speech
    /// (silence does not count). The original is kept.
    #[arg(long, value_name = "AUDIO", requires = "from_file")]
    pub confirm_file: Option<PathBuf>,
    /// With --confirm-file: when the confirmation cannot confirm the reading
    /// (below the voice threshold, insufficient detected speech, or unusable
    /// by the voice model), exit 0 with a report (`committed: false`,
    /// `reason: confirmation_unconfirmed`) instead of an error. Exit 0 then
    /// means a report, not an enrollment: nothing is saved. To save the first
    /// recording without the confirmation reading, run again without --confirm-file.
    /// A first recording that is not usable stays an error.
    /// A matching confirmation reading still enrolls normally.
    #[arg(
        long,
        requires_all = ["from_file", "confirm_file"],
        conflicts_with_all = [
            "check_recording",
            "mic",
            "from",
            "speaker",
            "show_passage",
            "delete_enrollment",
        ]
    )]
    pub report_unconfirmed: bool,
    /// Commit only while the config's `setup.voice_session` equals this ID.
    /// With --check-recording it only places the consumed file under that
    /// session's cache folder; the config is not read for it.
    #[arg(long, value_name = "ID", requires = "from_file")]
    pub session: Option<String>,
    /// Only check whether the first recording (--from-file) has enough usable
    /// speech to enroll: loads only the voice-activity model and saves
    /// nothing. Not ready is a result, not an error.
    #[arg(
        long,
        requires = "from_file",
        conflicts_with_all = [
            "mic",
            "from",
            "speaker",
            "show_passage",
            "delete_enrollment",
            "confirm_file",
            "keep_as_enrollment",
            "device",
            "seconds",
            "name",
            "emails",
            "spec_stdin",
        ]
    )]
    pub check_recording: bool,
    /// Keep the passage as `people/<me>/enrollment/<sha256>.wav`.
    #[arg(long)]
    pub keep_as_enrollment: bool,
    /// Passage language (BCP 47; default: the configured language).
    #[arg(long)]
    pub lang: Option<String>,
    /// Your name (creates or updates the Me person).
    #[arg(long)]
    pub name: Option<String>,
    /// Your e-mail addresses (repeatable).
    #[arg(long = "email")]
    pub emails: Vec<String>,
    /// Read the name and e-mail addresses from stdin instead, as an
    /// [`IdentitySpec`], so they never appear in argv (the app's path).
    #[arg(long, conflicts_with_all = ["name", "emails"])]
    pub spec_stdin: bool,
}

/// `setup voice --spec-stdin` input: who "Me" is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IdentitySpec {
    /// As `--name`.
    #[serde(default)]
    pub name: Option<String>,
    /// As `--email`, one entry each.
    #[serde(default)]
    pub emails: Vec<String>,
}

/// `setup --spec-stdin` input: who "Me" is and the contacts file to seed
/// people from (as `--name`, `--email` and `--people-file`). Every field is
/// optional; an absent one changes nothing. A relative `people_file`
/// resolves against the working directory, as in argv.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetupSpec {
    /// As `--name`.
    #[serde(default)]
    pub name: Option<String>,
    /// As `--email`, one entry each.
    #[serde(default)]
    pub emails: Vec<String>,
    /// As `--people-file`.
    #[serde(default)]
    pub people_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct TryArgs {
    /// Transcribe the installed speech model's own example without saving it.
    #[arg(long, conflicts_with_all = ["from_file", "device", "seconds", "save_as_meeting"])]
    pub model_example: bool,
    /// Seconds to record from the microphone (at most 25).
    #[arg(long, default_value_t = 20.0, conflicts_with = "from_file")]
    pub seconds: f64,
    /// Transcribe this file instead of recording. The original is kept.
    #[arg(long, value_name = "AUDIO")]
    pub from_file: Option<PathBuf>,
    /// Input device ID.
    #[arg(long, value_name = "ID", conflicts_with = "from_file")]
    pub device: Option<String>,
    /// Language (BCP 47) stored when saved as a meeting.
    #[arg(long)]
    pub lang: Option<String>,
    /// Save a copy of the clip as a meeting.
    #[arg(long)]
    pub save_as_meeting: bool,
    /// Setup cancellation session owned by the app.
    #[arg(long, hide = true)]
    pub session: Option<String>,
}

/// A contact to seed through setup.
#[derive(Debug, Default, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetupContact {
    /// Display name. Omit for an email-only contact.
    pub name: Option<String>,
    /// Addresses for this person.
    #[serde(default)]
    pub emails: Vec<String>,
}

#[derive(Debug, Default, Args)]
pub(crate) struct SetupArgs {
    /// Main meeting language (BCP 47).
    #[arg(long)]
    pub lang: Option<String>,
    /// Detect the language of each new recording or import.
    #[arg(long, conflicts_with = "lang")]
    pub auto_language: bool,
    /// Your name (creates or updates the Me person).
    #[arg(long)]
    pub name: Option<String>,
    /// Your e-mail addresses (repeatable).
    #[arg(long = "email")]
    pub emails: Vec<String>,
    /// Read the name, e-mail addresses and contacts file from stdin instead,
    /// as a [`SetupSpec`], so none appears in argv (the app's path; stdin
    /// then holds no answers, hence `--non-interactive`).
    #[arg(
        long,
        conflicts_with_all = ["name", "emails", "people_file"],
        requires = "non_interactive"
    )]
    pub spec_stdin: bool,
    /// Model profile.
    #[arg(long)]
    pub profile: Option<String>,
    /// Recognition hint words, one per line.
    #[arg(long, value_name = "FILE")]
    pub words_file: Option<PathBuf>,
    /// Contacts to seed people from (CSV or vCard; names and e-mails only).
    #[arg(long, value_name = "FILE")]
    pub people_file: Option<PathBuf>,
    /// Inline recognition hints supplied by MCP.
    #[arg(skip)]
    pub words: Option<Vec<String>>,
    /// Inline contacts supplied by MCP.
    #[arg(skip)]
    pub contacts: Option<Vec<SetupContact>>,
    /// Enroll your voice from a meeting ID or an audio file.
    #[arg(long, value_name = "MEETING|FILE", requires = "range")]
    pub enroll_me: Option<String>,
    /// Seconds where only you speak: start-end (e.g. 12.5-40).
    #[arg(long, requires = "enroll_me")]
    pub range: Option<String>,
    /// Download the profile's models.
    #[arg(long)]
    pub pull_models: bool,
    /// Acknowledge downloading models whose licence has not been verified.
    #[arg(long, requires = "pull_models")]
    pub allow_unverified_license: bool,
    /// Cancel model downloads when stdin closes.
    #[arg(long, requires = "pull_models")]
    pub cancel_on_stdin_eof: bool,
    /// Never prompt.
    #[arg(long)]
    pub non_interactive: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum DaemonCmd {
    /// Follow state, recording levels, job progress and notices as JSON lines.
    Watch,
    /// Answer or dismiss an agent notice using an action ID from the event stream.
    Notice {
        /// Notice ID from `daemon watch`.
        id: String,
        /// One of the notice's action IDs.
        action: String,
    },
    /// Show the recording agent, stopped recordings, prompts and permissions.
    Status,
    /// Start the recording agent and report its status.
    Start,
    /// Stop the recording agent when it is safe to do so.
    Stop,
    /// Register the recording agent to start at login.
    Install,
    /// Remove the recording agent’s login registration.
    Uninstall {
        /// For manual upgrades, require this durable install admission on a live login agent.
        #[arg(long)]
        admission_fence: Option<u64>,
        /// For manual rollback, require this durable install admission on disk and a proven idle registration.
        #[arg(long, conflicts_with = "admission_fence")]
        admission_fence_if_idle: Option<u64>,
    },
}

/// The recording a recording command acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
pub(crate) struct SessionArg {
    /// The session (default: the recording active when the command
    /// starts). The agent refuses it (`stale_session`) once another session
    /// replaced it, so a command never lands on a newer recording. Given
    /// once; a repeat is an error.
    #[arg(long = "session", value_name = "SESSION_ID")]
    pub session: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RecordCmd {
    /// Start a new recording.
    Start {
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Audio setup override; otherwise detected from the default devices.
        #[arg(long)]
        setup: Option<broscribe_dsp::Setup>,
    },
    /// Stop and save: the recording is queued for final processing.
    Stop {
        #[command(flatten)]
        session: SessionArg,
    },
    /// Show the active recording. Use daemon status for stopped recordings and prompts.
    Status,
    /// List meeting prompts that can still be answered.
    Prompts,
    /// Answer one pending meeting prompt, by its ID from record prompts.
    Answer {
        id: String,
        #[arg(value_enum)]
        action: RecordingAnswer,
    },
    /// Retry saving one stopped recording whose final processing failed.
    Retry {
        /// Exact stopped session; never defaults to the active recording.
        #[arg(long)]
        session: String,
    },
    /// Save a legacy recording in the open library or discard its journal.
    #[command(group(ArgGroup::new("journal_confirmation").required(true).args(["yes", "dry_run"])))]
    ResolveJournal {
        #[arg(value_enum)]
        resolution: JournalResolutionArg,
        /// The journal's session ID from daemon status.
        #[arg(long)]
        session: String,
        /// Confirm the requested change.
        #[arg(long)]
        yes: bool,
        /// Show the action without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Pause the recording in this session.
    Pause {
        #[command(flatten)]
        session: SessionArg,
    },
    /// Resume the paused recording.
    Resume {
        #[command(flatten)]
        session: SessionArg,
    },
    /// Set the language of the active recording.
    Lang {
        #[command(flatten)]
        session: SessionArg,
        language: String,
    },
    /// Bookmark the current position of the active recording.
    Bookmark {
        #[command(flatten)]
        session: SessionArg,
        label: Option<String>,
    },
    /// Add a note at the current recording position.
    Note {
        #[command(flatten)]
        session: SessionArg,
        /// The note; after `--`, a text starting with `-` is kept literally.
        text: String,
    },
    /// `+tag` or `tag` adds, `-tag` removes (`+-x` adds the tag `-x`).
    /// `--session` may come before or after the changes (never taken as a
    /// change); after `--` every value is a change (`-- --json` is the
    /// change `--json`, `-- --session` the change `--session`).
    Tag {
        #[command(flatten)]
        session: SessionArg,
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        changes: Vec<String>,
    },
    /// Confirm or remove a participant: a person ID, a registered e-mail, or
    /// a name (a new name creates a name-only person; no voice is learned).
    Participant {
        #[arg(value_enum)]
        action: ParticipantArg,
        person: String,
        /// The session (default: the active recording).
        #[arg(long)]
        session: Option<String>,
    },
    /// Rename the recording (active, or stopped and not yet saved). The
    /// title is read from stdin with `--stdin`; after saving, the answer is
    /// `already_published` with the meeting to rename through `title`.
    Title {
        /// The title; omit with `--stdin`.
        #[arg(required_unless_present = "stdin", conflicts_with = "stdin")]
        title: Option<String>,
        /// Read the title from stdin (the app's path: never in argv).
        #[arg(long)]
        stdin: bool,
        /// The session (default: the active recording).
        #[arg(long)]
        session: Option<String>,
    },
    /// Discard a recording without saving it: capture stops, nothing is
    /// processed, and its audio and provisional results are deleted. Accepts
    /// the active session or a stopped one not yet saved (`already_saved`
    /// otherwise: use `delete`).
    Discard {
        #[arg(long)]
        session: String,
        /// Required: discarding cannot be undone.
        #[arg(long, required = true)]
        yes: bool,
    },
    /// Transcribe the recording so far while capture continues.
    ProcessNow {
        #[command(flatten)]
        session: SessionArg,
    },
    /// Turn automatic stop on or off for the recording, or link it to a
    /// detected call (`--app`, a detection app ID from `record status`'s
    /// `auto_stop.candidates`). Automatic stop always saves; it never
    /// discards.
    #[command(group(ArgGroup::new("auto_stop_mode").required(true).args(["on", "off"])))]
    AutoStop {
        /// The session (default: the active recording).
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        on: bool,
        #[arg(long)]
        off: bool,
        /// Link the recording to this freshly detected call; only with `--on`.
        #[arg(long, value_name = "APP_ID", conflicts_with = "off")]
        app: Option<String>,
    },
    /// The live preview's speaker turns (provisional; suggestions are not
    /// identities), as `record.live` returns them. With `--follow`, NDJSON
    /// on stdout for that one session: the snapshot first
    /// (`{"type":"snapshot",…}`), then each live change the agent publishes
    /// for it (`turn`, `identity`, `transcript`), a fresh `snapshot` after
    /// the agent dropped events for a slow reader, and last
    /// `{"type":"end","reason":"session_ended"|"stale_session"|"max_events"|"max_seconds"|"agent_unavailable"}`.
    /// Lines are written as they come, never queued without bound; another
    /// session's events are never written.
    Live {
        /// The session (default: the recording active when the command
        /// starts; a follow never moves to a later one).
        #[arg(long)]
        session: Option<String>,
        /// Keep writing live changes until the session ends.
        #[arg(long)]
        follow: bool,
        /// With `--follow`: stop after this many lines after the snapshot.
        #[arg(long, value_name = "N", requires = "follow", value_parser = positive)]
        max_events: Option<usize>,
        /// With `--follow`: stop after this many seconds.
        #[arg(long, value_name = "SECONDS", requires = "follow", value_parser = clap::value_parser!(u64).range(1..))]
        max_seconds: Option<u64>,
    },
    /// Name, confirm or clear the speaker of exactly one live turn, as it was
    /// shown (its revision, epoch and evidence). Never names later speech and
    /// never teaches a voice.
    #[command(group(
        ArgGroup::new("live_action")
            .required(true)
            .args(["name", "name_stdin", "confirm", "clear"])
    ))]
    LiveAttribute {
        /// The session (default: the active recording).
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        turn: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        epoch: u64,
        #[arg(long)]
        evidence: String,
        /// A person ID, a registered e-mail, or a new name (a name-only
        /// person).
        #[arg(long)]
        name: Option<String>,
        /// Read the `--name` value from stdin (the app's path: never in argv).
        #[arg(long)]
        name_stdin: bool,
        /// Accept the turn's suggestion: the suggested person's ID.
        #[arg(long, value_name = "PERSON_ID")]
        confirm: Option<String>,
        /// Remove your name from the turn: it shows the suggestion again.
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum JournalResolutionArg {
    Adopt,
    Discard,
}

impl From<JournalResolutionArg> for broscribe_ipc::JournalResolution {
    fn from(value: JournalResolutionArg) -> Self {
        match value {
            JournalResolutionArg::Adopt => Self::Adopt,
            JournalResolutionArg::Discard => Self::Discard,
        }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, ValueEnum, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordingAnswer {
    Record,
    Ignore,
}

#[derive(Clone, Copy, Debug, ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ParticipantArg {
    Confirm,
    Remove,
}

#[derive(Debug, Subcommand)]
pub(crate) enum DetectCmd {
    /// Show the agent’s meeting detection status and app policies.
    Status,
    /// Set whether this meeting app records automatically, asks, or is ignored.
    Policy {
        app: String,
        #[arg(value_enum)]
        policy: DetectPolicyArg,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DetectPolicyArg {
    Auto,
    Ask,
    Off,
}

#[derive(Clone, Copy, Debug, ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IntegrationArg {
    /// Calendars already synced by macOS, read by BroScribe Agent.
    SystemCalendar,
    MicrosoftGraph,
    Google,
}

#[derive(Debug, Subcommand)]
pub(crate) enum IntegrationsCmd {
    /// List corporate accounts without starting authentication.
    Accounts { integration: Option<IntegrationArg> },
    /// Sign in and verify a corporate identity (User.Read); selects no calendars.
    AddAccount {
        integration: IntegrationArg,
        #[command(flatten)]
        attempt: AttemptArg,
    },
    /// Reconnect an existing corporate account and verify its identity.
    ReconnectAccount {
        integration: IntegrationArg,
        #[arg(long)]
        account: String,
        #[arg(long)]
        generation: u64,
        #[command(flatten)]
        attempt: AttemptArg,
    },
    /// Disconnect a corporate account and remove its local integration data.
    RemoveAccount {
        integration: IntegrationArg,
        #[arg(long)]
        account: String,
        #[arg(long)]
        generation: u64,
    },
    /// Discard only the provider's quarantined, unidentified old sign-in and
    /// its token/calendar/photo cache; registered accounts and library stay.
    DiscardLegacy {
        integration: IntegrationArg,
        #[arg(long)]
        generation: u64,
        /// Required explicit confirmation of this limited deletion.
        #[arg(long)]
        yes: bool,
    },
    /// Discover readable calendars; discovery does not select or sync them.
    /// microsoft-graph needs `--account`; google has one provider-wide
    /// account and refuses it. The catalog's `generation` and
    /// `selection_generation` fence `select-calendars`.
    Calendars {
        integration: IntegrationArg,
        #[arg(long)]
        account: Option<String>,
    },
    /// Replace the whole selection with a [`CalendarSelectionSpec`] read from
    /// stdin (calendar IDs and mailboxes never go in argv); an empty
    /// selection explicitly reads nothing. microsoft-graph needs `--account`;
    /// google refuses it. Both are fenced by the catalog's two generations.
    SelectCalendars {
        integration: IntegrationArg,
        #[arg(long)]
        account: Option<String>,
        /// The catalog's `generation`.
        #[arg(long)]
        generation: u64,
        /// The catalog's `selection_generation`.
        #[arg(long)]
        selection_generation: u64,
        /// Read the selection from stdin (required).
        #[arg(long, required = true)]
        spec_stdin: bool,
    },
    /// Stop reading one selected microsoft-graph calendar, keeping the other
    /// routes and their cached events: a [`CalendarRemovalSpec`] on stdin.
    RemoveCalendar {
        integration: IntegrationArg,
        #[arg(long)]
        account: String,
        #[arg(long)]
        generation: u64,
        #[arg(long)]
        selection_generation: u64,
        /// Read the target from stdin (required).
        #[arg(long, required = true)]
        spec_stdin: bool,
    },
    /// Sync the selected calendars of one corporate account.
    SyncAccount {
        integration: IntegrationArg,
        #[arg(long)]
        account: String,
    },
    /// Explicit read-only consent; microsoft365_groups requests Group.Read.All
    /// across the tenant and may require administrator approval.
    GrantAccount {
        integration: IntegrationArg,
        #[arg(long)]
        account: String,
        #[arg(long)]
        generation: u64,
        #[arg(long)]
        permission: String,
        #[command(flatten)]
        attempt: AttemptArg,
    },
    /// Configure a calendar integration or its OAuth client.
    Setup {
        integration: IntegrationArg,
        #[arg(long)]
        client_id: Option<String>,
        /// Read the OAuth client secret from stdin, never argv or config.
        #[arg(long)]
        client_secret_stdin: bool,
        /// system-calendar only: read exactly these
        /// calendars (repeat the flag); replaces the selection. Without it
        /// nothing is selected or changed.
        #[arg(long = "calendar-id", value_name = "ID")]
        calendar_ids: Vec<String>,
        /// system-calendar only: explicitly stop reading
        /// every selected calendar.
        #[arg(long, conflicts_with_all = ["calendar_ids", "client_id", "client_secret_stdin"])]
        clear_calendars: bool,
        /// system-calendar only: replace the selection with a
        /// [`SystemCalendarSelectionSpec`] read from stdin instead (calendar
        /// IDs never go in argv; the app's path); an empty list explicitly
        /// stops reading every selected calendar.
        #[arg(
            long,
            conflicts_with_all = ["calendar_ids", "clear_calendars", "client_id", "client_secret_stdin"]
        )]
        spec_stdin: bool,
        /// google only: remove the stored OAuth client secret (only while
        /// signed out; sign-out keeps it). Combine with `--client-id` to
        /// replace the client.
        #[arg(
            long,
            conflicts_with_all = ["client_secret_stdin", "calendar_ids", "clear_calendars", "spec_stdin"]
        )]
        clear_client_secret: bool,
    },
    /// Sign in; for system-calendar, connect (macOS may ask for calendar access).
    Login {
        integration: IntegrationArg,
        #[command(flatten)]
        attempt: AttemptArg,
    },
    /// Cancel the sign-in started with `--attempt`, durably and from any
    /// process: once `cancelled` nothing of it is ever saved; a sign-in that
    /// already finished keeps its result, which is returned as it is.
    /// Killing the signing-in process does not cancel it.
    CancelLogin {
        integration: IntegrationArg,
        #[arg(long, value_name = "ID")]
        attempt: String,
    },
    /// The state of the sign-in started with `--attempt`: running,
    /// cancelled, succeeded, failed, interrupted or not found.
    LoginAttempt {
        integration: IntegrationArg,
        #[arg(long, value_name = "ID")]
        attempt: String,
    },
    /// Show calendar integration status.
    Status { integration: Option<IntegrationArg> },
    /// Sign out; for native calendars, disconnect and clear its local copy.
    /// Sign-out is local: the grant stays live at the provider.
    Logout {
        integration: IntegrationArg,
        /// google only: also ask Google to revoke the signed-out grant. Google
        /// revokes every permission granted to the whole OAuth project, for
        /// every data folder, Mac or tool signed in with it, and may take a
        /// while to apply it.
        #[arg(long)]
        revoke_at_google: bool,
    },
    /// Refresh calendar data for connected integrations.
    Sync { integration: Option<IntegrationArg> },
}

/// The sign-in attempt a browser sign-in runs as (google and
/// microsoft-graph).
#[derive(Clone, Debug, Default, PartialEq, Eq, Args)]
pub(crate) struct AttemptArg {
    /// Caller-chosen ID of this sign-in (1 to 64 letters, digits or `-`),
    /// for `cancel-login` and `login-attempt`; default: a new random one,
    /// reported in the result. Single use.
    #[arg(long, value_name = "ID")]
    pub attempt: Option<String>,
}

/// `integrations select-calendars --spec-stdin` input. Only the field of
/// the command's integration may be non-empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CalendarSelectionSpec {
    /// microsoft-graph: routing-qualified targets as `calendars` lists them.
    #[serde(default)]
    pub targets: Vec<CalendarTarget>,
    /// google: calendar IDs of the catalog.
    #[serde(default)]
    pub calendar_ids: Vec<String>,
}

/// `integrations setup system-calendar --spec-stdin` input: exactly the
/// calendars to read, as `--calendar-id` (required; empty as
/// `--clear-calendars`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemCalendarSelectionSpec {
    pub calendar_ids: Vec<String>,
}

/// `integrations remove-calendar --spec-stdin` input.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CalendarRemovalSpec {
    pub target: CalendarTarget,
}

#[cfg(test)]
mod recording_args_tests {
    use super::*;

    #[test]
    fn parity_setup_batch_flags_require_a_download_and_example_is_read_only() {
        assert!(
            Cli::try_parse_from([
                "broscribe",
                "setup",
                "--pull-models",
                "--allow-unverified-license",
                "--cancel-on-stdin-eof"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["broscribe", "setup", "--allow-unverified-license"]).is_err());
        assert!(Cli::try_parse_from(["broscribe", "setup", "--cancel-on-stdin-eof"]).is_err());
        assert!(
            Cli::try_parse_from([
                "broscribe",
                "setup",
                "try",
                "--model-example",
                "--save-as-meeting"
            ])
            .is_err()
        );
    }

    #[test]
    fn every_command_has_help() {
        use clap::CommandFactory as _;

        fn check(command: &clap::Command, path: String) {
            assert!(
                command
                    .get_about()
                    .is_some_and(|help| !help.to_string().trim().is_empty()),
                "Missing help for {path}"
            );
            for child in command.get_subcommands() {
                check(child, format!("{path} {}", child.get_name()));
            }
        }

        check(&Cli::command(), "broscribe".into());
        for (args, phrase) in [
            (
                &["app", "open-at-login", "--help"][..],
                "Start BroScribe at login",
            ),
            (
                &["app", "reading-size", "--help"][..],
                "Alias of settings text-size",
            ),
            (
                &["search", "--help"][..],
                "auto, keyword, semantic or hybrid",
            ),
        ] {
            let Parsed::Display(help) = run_parse(args) else {
                panic!("{args:?}")
            };
            assert!(help.contains(phrase), "{help}");
        }
    }

    #[test]
    fn every_agent_command_accepts_json() {
        for args in [
            vec!["daemon", "status"],
            vec!["daemon", "start"],
            vec!["daemon", "stop"],
            vec!["daemon", "install"],
            vec!["daemon", "uninstall"],
            vec![
                "record",
                "start",
                "--app",
                "synthetic.app",
                "--title",
                "Synthetic",
            ],
            vec!["record", "stop"],
            vec!["record", "status"],
            vec!["record", "pause"],
            vec!["record", "resume"],
            vec!["record", "lang", "pt-BR"],
            vec!["record", "bookmark"],
            vec!["record", "note", "Synthetic note"],
            vec!["record", "tag", "+a", "-b"],
            vec!["record", "tag", "--session", "s-synthetic", "+-literal"],
            vec!["record", "participant", "confirm", "p-synthetic"],
            vec!["record", "participant", "remove", "p-synthetic"],
            vec![
                "record",
                "participant",
                "confirm",
                "Synthetic Name",
                "--session",
                "s-synthetic",
            ],
            vec!["record", "title", "--stdin"],
            vec!["record", "title", "--stdin", "--session", "s-synthetic"],
            vec!["record", "title", "Synthetic title"],
            vec!["record", "discard", "--session", "s-synthetic", "--yes"],
            vec!["record", "process-now"],
            vec!["title", "m-synthetic", "--stdin"],
            vec!["title", "m-synthetic", "Synthetic title"],
            vec!["delete", "m-synthetic", "--dry-run"],
            vec!["delete", "m-synthetic", "--yes"],
            vec!["record", "live"],
            vec!["record", "auto-stop", "--on"],
            vec!["record", "auto-stop", "--off"],
            vec!["record", "auto-stop", "--session", "s-synthetic", "--off"],
            vec!["record", "auto-stop", "--on", "--app", "teams"],
            vec!["record", "live", "--session", "s-synthetic"],
            vec![
                "record",
                "live-attribute",
                "--turn",
                "system-16000",
                "--revision",
                "3",
                "--epoch",
                "1",
                "--evidence",
                "ev-1",
                "--name-stdin",
            ],
            vec![
                "record",
                "live-attribute",
                "--session",
                "s-synthetic",
                "--turn",
                "system-16000",
                "--revision",
                "3",
                "--epoch",
                "1",
                "--evidence",
                "ev-1",
                "--confirm",
                "p-synthetic",
            ],
            vec![
                "record",
                "live-attribute",
                "--turn",
                "mic-0",
                "--revision",
                "1",
                "--epoch",
                "0",
                "--evidence",
                "ev-1",
                "--clear",
            ],
            vec!["detect", "status"],
            vec!["detect", "policy", "teams", "off"],
            vec!["audio-check"],
            vec!["audio-check", "--save"],
            vec!["audio-check", "--save", "--echo-handling", "residue_gating"],
            vec!["integrations", "setup", "google"],
            vec!["integrations", "login", "microsoft-graph"],
            vec!["integrations", "accounts", "microsoft-graph"],
            vec!["integrations", "add-account", "microsoft-graph"],
            vec![
                "integrations",
                "reconnect-account",
                "microsoft-graph",
                "--account",
                "11111111111111111111111111111111",
                "--generation",
                "3",
            ],
            vec![
                "integrations",
                "remove-account",
                "microsoft-graph",
                "--account",
                "11111111111111111111111111111111",
                "--generation",
                "3",
            ],
            vec![
                "integrations",
                "discard-legacy",
                "microsoft-graph",
                "--generation",
                "3",
                "--yes",
            ],
            vec![
                "integrations",
                "calendars",
                "microsoft-graph",
                "--account",
                "11111111111111111111111111111111",
            ],
            vec![
                "integrations",
                "select-calendars",
                "microsoft-graph",
                "--account",
                "11111111111111111111111111111111",
                "--generation",
                "3",
                "--selection-generation",
                "2",
                "--spec-stdin",
            ],
            vec![
                "integrations",
                "sync-account",
                "microsoft-graph",
                "--account",
                "11111111111111111111111111111111",
            ],
            vec![
                "integrations",
                "grant-account",
                "microsoft-graph",
                "--account",
                "11111111111111111111111111111111",
                "--generation",
                "3",
                "--permission",
                "directory",
            ],
            vec!["integrations", "status"],
            vec!["integrations", "status", "google"],
            vec!["integrations", "logout", "google"],
            vec!["integrations", "sync"],
            vec!["integrations", "sync", "google"],
            vec!["integrations", "setup", "system-calendar"],
            vec![
                "integrations",
                "setup",
                "system-calendar",
                "--calendar-id",
                "cal-a",
                "--calendar-id",
                "cal-b",
            ],
            vec!["integrations", "login", "system-calendar"],
            vec!["integrations", "status", "system-calendar"],
            vec!["integrations", "logout", "system-calendar"],
            vec!["integrations", "sync", "system-calendar"],
            vec![
                "integrations",
                "setup",
                "system-calendar",
                "--clear-calendars",
            ],
            vec!["integrations", "logout", "google", "--revoke-at-google"],
            vec!["integrations", "setup", "google", "--clear-client-secret"],
            vec![
                "integrations",
                "setup",
                "google",
                "--clear-client-secret",
                "--client-id",
                "synthetic-client",
            ],
        ] {
            let argv = ["broscribe", "--json"]
                .into_iter()
                .chain(args.iter().copied());
            let parsed = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert!(parsed.json);
        }
    }

    #[test]
    fn destructive_and_title_commands_need_their_confirmation_or_input() {
        for args in [
            vec!["record", "discard", "--session", "s-synthetic"],
            vec!["record", "discard", "--yes"],
            vec!["delete", "m-synthetic"],
            vec!["delete", "m-synthetic", "--dry-run", "--yes"],
            vec!["title", "m-synthetic"],
            vec!["title", "m-synthetic", "Synthetic", "--stdin"],
            vec!["record", "title"],
            vec!["record", "auto-stop"],
            vec!["record", "auto-stop", "--on", "--off"],
            vec!["record", "auto-stop", "--off", "--app", "teams"],
            vec!["record", "auto-stop", "--app", "teams"],
            vec![
                "record",
                "live-attribute",
                "--turn",
                "mic-0",
                "--revision",
                "1",
                "--epoch",
                "0",
                "--evidence",
                "ev-1",
            ],
            vec![
                "record",
                "live-attribute",
                "--turn",
                "mic-0",
                "--revision",
                "1",
                "--epoch",
                "0",
                "--evidence",
                "ev-1",
                "--clear",
                "--confirm",
                "p-synthetic",
            ],
            vec![
                "integrations",
                "setup",
                "system-calendar",
                "--clear-calendars",
                "--calendar-id",
                "synthetic-calendar",
            ],
            vec![
                "integrations",
                "setup",
                "google",
                "--clear-client-secret",
                "--client-secret-stdin",
            ],
            vec![
                "integrations",
                "setup",
                "google",
                "--clear-client-secret",
                "--spec-stdin",
            ],
            vec![
                "integrations",
                "setup",
                "system-calendar",
                "--clear-client-secret",
                "--clear-calendars",
            ],
            vec![
                "integrations",
                "setup",
                "system-calendar",
                "--clear-client-secret",
                "--calendar-id",
                "synthetic-calendar",
            ],
        ] {
            let argv = ["broscribe", "--json"]
                .into_iter()
                .chain(args.iter().copied());
            assert!(Cli::try_parse_from(argv).is_err(), "{args:?} parsed");
        }
        let parsed = Cli::try_parse_from(["broscribe", "record", "tag", "-literal"])
            .unwrap_or_else(|e| {
                panic!("{e}");
            });
        let Command::Record {
            action: RecordCmd::Tag { changes, session },
        } = parsed.command
        else {
            panic!("not record tag");
        };
        assert_eq!(
            (changes, session.session),
            (vec!["-literal".to_owned()], None)
        );
    }

    fn record(args: &[&str]) -> Result<RecordCmd, clap::Error> {
        let argv = ["broscribe", "record"]
            .into_iter()
            .chain(args.iter().copied());
        Cli::try_parse_from(argv).map(|parsed| match parsed.command {
            Command::Record { action } => action,
            other => panic!("not a record command: {other:?}"),
        })
    }

    /// CL01 (session): every recording command takes one explicit
    /// `--session`; a repeat is refused rather than silently picking one, and
    /// after `--` values are literal (never options or globals).
    #[test]
    fn recording_commands_take_one_explicit_session_and_literal_values() {
        let s = |id: &str| SessionArg {
            session: Some(id.to_owned()),
        };
        assert_eq!(
            format!("{:?}", record(&["stop", "--session", "s-a"]).unwrap()),
            format!("{:?}", RecordCmd::Stop { session: s("s-a") })
        );
        for args in [
            vec!["pause", "--session", "s-a"],
            vec!["resume", "--session", "s-a"],
            vec!["process-now", "--session", "s-a"],
            vec!["lang", "--session", "s-a", "pt-BR"],
            vec!["bookmark", "--session", "s-a"],
            vec!["note", "--session", "s-a", "synthetic note"],
            vec!["tag", "--session", "s-a", "+x"],
        ] {
            let parsed = record(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let session = match parsed {
                RecordCmd::Pause { session }
                | RecordCmd::Resume { session }
                | RecordCmd::ProcessNow { session }
                | RecordCmd::Lang { session, .. }
                | RecordCmd::Bookmark { session, .. }
                | RecordCmd::Note { session, .. }
                | RecordCmd::Tag { session, .. } => session,
                other => panic!("{other:?}"),
            };
            assert_eq!(session, s("s-a"), "{args:?}");
        }
        // Defaults resolve the active recording later, never a stale guess.
        assert!(matches!(
            record(&["stop"]).unwrap(),
            RecordCmd::Stop { session } if session.session.is_none()
        ));
        // One session only.
        for args in [
            vec!["stop", "--session", "s-a", "--session", "s-b"],
            vec!["tag", "--session", "s-a", "--session", "s-b", "+x"],
        ] {
            assert!(record(&args).is_err(), "{args:?} parsed");
        }
        // After `--` values are literal: a tag or note that looks like an
        // option (even the global `--json`) is data.
        let RecordCmd::Tag { session, changes } =
            record(&["tag", "--session", "s-a", "--", "--json", "-old", "+new"]).unwrap()
        else {
            panic!("not record tag");
        };
        assert_eq!(session, s("s-a"));
        assert_eq!(changes, ["--json", "-old", "+new"]);
        let RecordCmd::Note { text, .. } = record(&["note", "--", "--session"]).unwrap() else {
            panic!("not record note");
        };
        assert_eq!(text, "--session");
    }

    #[test]
    fn removed_calendar_auth_providers_and_options_are_rejected() {
        for args in [
            vec!["integrations", "login", "teams-session"],
            vec!["integrations", "login", "outlook-calendar"],
            vec![
                "integrations",
                "setup",
                "microsoft-graph",
                "--clear-client-id",
            ],
            vec![
                "integrations",
                "add-account",
                "microsoft-graph",
                "--i-understand-experimental",
            ],
        ] {
            let argv = ["broscribe", "--json"]
                .into_iter()
                .chain(args.iter().copied());
            assert!(Cli::try_parse_from(argv).is_err(), "{args:?} parsed");
        }
    }

    #[test]
    fn calendar_ids_repeat_in_order() {
        let parsed = Cli::try_parse_from([
            "broscribe",
            "integrations",
            "setup",
            "system-calendar",
            "--calendar-id",
            "cal-b",
            "--calendar-id",
            "cal-a",
        ])
        .unwrap();
        let Command::Integrations {
            action: IntegrationsCmd::Setup { calendar_ids, .. },
        } = parsed.command
        else {
            panic!("not integrations setup");
        };
        assert_eq!(calendar_ids, ["cal-b", "cal-a"]);
    }

    #[test]
    fn audio_check_echo_handling_requires_save() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(
                ["broscribe", "audio-check"]
                    .into_iter()
                    .chain(args.iter().copied()),
            )
        };
        assert!(parse(&["--echo-handling", "full_aec"]).is_err());
        let parsed = parse(&["--save", "--echo-handling", "full_aec"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::AudioCheck {
                save: true,
                echo_handling: Some(EchoHandlingArg::FullAec)
            }
        ));
    }

    #[test]
    fn templates_export_and_instructions_parse() {
        for args in [
            vec!["enrich", "m-0000a1", "--instructions-stdin"],
            vec!["enrich", "instructions", "show"],
            vec!["enrich", "instructions", "set", "--stdin"],
            vec!["enrich", "instructions", "clear"],
            vec!["templates", "list"],
            vec!["templates", "list", "--meeting", "m-0000a1"],
            vec!["templates", "show", "bant"],
            vec!["templates", "new", "my-recap", "--stdin"],
            vec!["templates", "edit", "bant", "--stdin"],
            vec!["templates", "delete", "my-recap"],
            vec!["templates", "reset", "bant"],
            vec!["templates", "auto", "add", "bant"],
            vec!["templates", "auto", "remove", "bant"],
            vec!["templates", "run", "m-0000a1", "bant", "--queue"],
            vec![
                "templates",
                "run",
                "m-0000a1",
                "bant",
                "--provider",
                "fake",
                "--model",
                "m",
                "--instructions-stdin",
            ],
            vec!["templates", "result", "m-0000a1", "bant"],
            vec!["search", "budget", "--in", "template"],
            vec!["export", "m-0000a1"],
            vec![
                "export",
                "m-0000a1",
                "--include",
                "transcript,template:bant",
                "--format",
                "txt",
                "--no-timestamps",
                "--out",
                "-",
            ],
        ] {
            let argv = ["broscribe", "--json"]
                .into_iter()
                .chain(args.iter().copied());
            Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
        for bad in [
            vec!["enrich", "instructions", "set"],
            vec!["enrich", "instructions", "set", "Focus on risks"],
            vec!["templates", "new", "my-recap"],
            vec!["templates", "run", "m-0000a1", "bant", "--model", "m"],
            vec!["search", "x", "--in", "summary"],
            vec!["export", "m-0000a1", "--format", "pdf"],
        ] {
            let argv = ["broscribe"].into_iter().chain(bad.iter().copied());
            assert!(Cli::try_parse_from(argv).is_err(), "{bad:?}");
        }
        let parsed = Cli::try_parse_from([
            "broscribe",
            "export",
            "m-0000a1",
            "--include",
            "summary,template:bant",
        ])
        .unwrap();
        let Command::Export(a) = parsed.command else {
            panic!("not export");
        };
        assert_eq!(a.include, ["summary", "template:bant"]);
        assert_eq!(a.format, ExportFormatArg::Md);
    }

    fn run_parse(args: &[&str]) -> Parsed {
        parse(
            std::iter::once("broscribe")
                .chain(args.iter().copied())
                .map(OsString::from)
                .collect(),
        )
    }

    fn parsed(args: &[&str]) -> Cli {
        match run_parse(args) {
            Parsed::Run(cli) => cli,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    fn usage(args: &[&str]) -> UsageError {
        match run_parse(args) {
            Parsed::Usage(e) => e,
            other => panic!("{args:?} parsed: {other:?}"),
        }
    }

    /// CL01: global options anywhere before `--` are global, each as often
    /// as given (a repeat is refused); after `--` everything is literal.
    #[test]
    fn globals_are_hoisted_before_the_terminator_only() {
        let cli = parsed(&["tag", "m-0000a1", "+a", "--json", "--library", "/lib"]);
        assert!(cli.json);
        assert_eq!(cli.library, Some(PathBuf::from("/lib")));
        let Command::Tag { changes, .. } = cli.command else {
            panic!("not tag");
        };
        assert_eq!(changes, ["+a"]);

        let cli = parsed(&[
            "record",
            "tag",
            "--session",
            "s-a",
            "--",
            "--json",
            "--library",
            "/elsewhere",
        ]);
        assert!(!cli.json);
        assert_eq!(cli.library, None);
        let Command::Record {
            action: RecordCmd::Tag { changes, .. },
        } = cli.command
        else {
            panic!("not record tag");
        };
        assert_eq!(changes, ["--json", "--library", "/elsewhere"]);

        let cli = parsed(&["version", "--library=/a=b"]);
        assert_eq!(cli.library, Some(PathBuf::from("/a=b")));

        // A repeat is refused, never collapsed into one.
        assert!(usage(&["--json", "version", "--json"]).json);
        assert!(!usage(&["--library", "/a", "version", "--library", "/b"]).json);
        // An option with no value before the terminator is refused, never
        // left for a variadic list to take as a value, and the `--json`
        // after `--` stays literal.
        let e = usage(&["record", "note", "--library", "--", "--json"]);
        assert!(!e.json);
        assert!(e.message.contains("--library"), "{}", e.message);
        assert!(
            usage(&["tag", "m-0000a1", "+a", "--library"])
                .message
                .contains("--library")
        );
    }

    /// CL01 (C1): `record tag`'s `--session` anywhere before `--` targets
    /// that session, so a trailing one is never swallowed as a change of
    /// the active recording; a repeat or a missing value is refused, and
    /// after `--` it is a literal change.
    #[test]
    fn record_tag_session_is_an_option_wherever_it_comes_before_the_terminator() {
        let tag = |args: &[&str]| match parsed(args).command {
            Command::Record {
                action: RecordCmd::Tag { session, changes },
            } => (session.session, changes),
            other => panic!("{args:?}: {other:?}"),
        };
        let s = |id: &str| Some(id.to_owned());
        assert_eq!(
            tag(&["record", "tag", "+a", "--session", "s-b"]),
            (s("s-b"), vec!["+a".to_owned()])
        );
        assert_eq!(
            tag(&["record", "tag", "+a", "--session=s-b", "-c", "--json"]),
            (s("s-b"), vec!["+a".to_owned(), "-c".to_owned()])
        );
        assert_eq!(
            tag(&["record", "tag", "+a", "--", "--session", "s-b"]),
            (
                None,
                vec!["+a".to_owned(), "--session".to_owned(), "s-b".to_owned()]
            )
        );
        // Only the first `--` is the terminator, wherever the values start;
        // a later one is a value, and an explicit session before it holds.
        assert_eq!(
            tag(&[
                "record",
                "tag",
                "+a",
                "--session",
                "s-b",
                "--",
                "-c",
                "--",
                "--json"
            ]),
            (
                s("s-b"),
                vec![
                    "+a".to_owned(),
                    "-c".to_owned(),
                    "--".to_owned(),
                    "--json".to_owned()
                ]
            )
        );
        let cli = parsed(&["tag", "m-0000a1", "+a", "--", "-b", "--library", "/x"]);
        assert!(cli.library.is_none());
        let Command::Tag { meeting, changes } = cli.command else {
            panic!("not tag");
        };
        assert_eq!(meeting, "m-0000a1");
        assert_eq!(changes, ["+a", "-b", "--library", "/x"]);
        assert!(matches!(
            run_parse(&["record", "tag", "--help", "--", "+a"]),
            Parsed::Display(_)
        ));
        for args in [
            vec![
                "record",
                "tag",
                "+a",
                "--session",
                "s-a",
                "--session",
                "s-b",
            ],
            vec!["record", "tag", "--session", "s-a", "+a", "--session=s-b"],
            vec!["--json", "record", "tag", "+a", "--session"],
            vec!["--json", "record", "tag", "+a", "--session", "--", "+b"],
        ] {
            let e = usage(&args);
            assert!(e.message.contains("--session"), "{args:?}: {}", e.message);
            assert_eq!(e.json, args[0] == "--json", "{args:?}");
        }
    }

    /// CL11: every refused command line under `--json` is an envelope,
    /// a missing subcommand included; help and version are output.
    #[test]
    fn usage_errors_keep_the_json_flag() {
        for args in [
            vec!["--json", "queue"],
            vec!["queue", "--json"],
            vec!["--json", "update", "settings"],
            vec!["--json"],
        ] {
            let e = usage(&args);
            assert!(e.json, "{args:?}");
            assert_eq!(
                e.message, "a subcommand is required (see --help)",
                "{args:?}"
            );
        }
        let e = usage(&["--json", "nope"]);
        assert!(e.json && !e.message.is_empty() && !e.message.starts_with("error:"));
        assert!(!usage(&["queue"]).json);
        assert!(matches!(run_parse(&["--help"]), Parsed::Display(_)));
        assert!(matches!(
            run_parse(&["--json", "--version"]),
            Parsed::Display(_)
        ));
    }

    #[test]
    fn update_help_describes_the_default_source_consent_and_publisher_commands() {
        let Parsed::Display(help) = run_parse(&["update", "--help"]) else {
            panic!("expected updater help");
        };
        for text in [
            "GitHub Releases",
            "default and can be turned off",
            "requires confirmation",
            "release-key new",
            "sign-manifest --key-file",
            "verify-manifest",
        ] {
            assert!(help.contains(text), "{help}");
        }
        for action in ["install", "rollback", "recover"] {
            let Parsed::Display(help) = run_parse(&["update", action, "--help"]) else {
                panic!("expected update {action} help");
            };
            assert!(!help.contains("--data-dir"), "{help}");
        }
    }

    #[test]
    fn feature_commands_parse() {
        for args in [
            vec!["reprocess", "m-0000a1", "--queue"],
            vec!["queue", "prune", "--dry-run"],
            vec![
                "queue",
                "prune",
                "--yes",
                "--state",
                "done",
                "--state",
                "failed",
                "--older-than",
                "30d",
                "--kind",
                "import",
                "--quarantine-invalid",
                "--limit",
                "10",
            ],
            vec!["retention", "apply", "--dry-run"],
            vec!["retention", "apply", "--dry-run", "--policy", "keep"],
            vec!["retention", "apply", "--dry-run", "--policy", "processed"],
            vec![
                "retention",
                "apply",
                "--dry-run",
                "--policy",
                "processed-and-named",
            ],
            vec!["retention", "apply", "--dry-run", "--policy", "days:30"],
            vec!["retention", "policy", "keep"],
            vec!["retention", "policy", "processed"],
            vec!["retention", "policy", "processed-and-named"],
            vec!["retention", "policy", "days:14"],
            vec!["retention", "delete-now", "m-0000a1", "--dry-run"],
            vec!["retention", "delete-now", "m-0000a1", "--yes"],
            vec!["retention", "explain"],
            vec!["retention", "explain", "m-0000a1", "m-0000a2"],
            vec!["library", "check"],
            vec![
                "library",
                "repair",
                "--only",
                "index,deletions",
                "--dry-run",
            ],
            vec!["library", "repair", "--only", "identity", "--yes"],
            vec!["library", "repair", "--only", "temps", "--dry-run"],
            vec!["models", "doctor"],
            vec!["models", "doctor", "--live", "--profile", "balanced"],
            vec!["models", "pull", "synthetic-model", "--cancel-on-stdin-eof"],
            vec!["record", "live", "--follow"],
            vec![
                "record",
                "live",
                "--session",
                "s-a",
                "--follow",
                "--max-events",
                "10",
                "--max-seconds",
                "60",
            ],
            vec!["meetings", "--page-size", "50"],
            vec!["meetings", "--page-size", "50", "--cursor", "opaque"],
            vec!["search", "--query-stdin", "--page-size", "5"],
            vec!["actions", "--page-size", "500"],
            vec!["people", "edit", "p-synthetic", "--spec-stdin"],
            vec![
                "people",
                "edit",
                "p-synthetic",
                "--spec-stdin",
                "--expect-generation",
                "google:3",
            ],
            vec!["people", "undo-edit", "p-synthetic", "--undo-id", "pu-1"],
            vec!["people", "lookup", "--query-stdin"],
            vec!["people", "lookup", "--", "-literal"],
            vec!["speakers", "live-review"],
            vec!["speakers", "live-review", "--meeting", "m-0000a1"],
            vec![
                "speakers",
                "live-dismiss",
                "m-0000a1",
                "--decision",
                "3",
                "--review-key",
                "ab12",
            ],
            vec!["integrations", "calendars", "google"],
            vec![
                "integrations",
                "select-calendars",
                "google",
                "--generation",
                "3",
                "--selection-generation",
                "5",
                "--spec-stdin",
            ],
            vec!["integrations", "setup", "system-calendar", "--spec-stdin"],
            vec!["import", "--spec-stdin", "--handling", "copy", "--enrich"],
            vec!["setup", "--non-interactive", "--spec-stdin", "--lang", "en"],
            vec![
                "integrations",
                "remove-calendar",
                "microsoft-graph",
                "--account",
                "11111111111111111111111111111111",
                "--generation",
                "3",
                "--selection-generation",
                "2",
                "--spec-stdin",
            ],
            vec!["integrations", "login", "google", "--attempt", "a-1"],
            vec![
                "integrations",
                "add-account",
                "microsoft-graph",
                "--attempt",
                "a-1",
            ],
            vec!["integrations", "cancel-login", "google", "--attempt", "a-1"],
            vec![
                "integrations",
                "login-attempt",
                "microsoft-graph",
                "--attempt",
                "a-1",
            ],
            vec!["app", "about"],
            vec!["app", "reading-size"],
            vec!["app", "reading-size", "larger"],
            vec!["app", "live-identity", "off"],
            vec!["app", "uncalibrated-me-suggestions", "status"],
            vec!["update", "status"],
            vec!["update", "check"],
            vec!["update", "download"],
            vec!["update", "discard"],
            vec!["update", "install", "--yes"],
            vec!["update", "install", "--expectation-stdin"],
            vec!["update", "confirm"],
            vec!["update", "rollback", "--yes"],
            vec!["update", "rollback", "--expectation-stdin"],
            vec!["update", "admission", "acquire"],
            vec!["update", "admission", "release", "--fence", "7"],
            vec!["update", "recover"],
            vec!["update", "settings", "show"],
            vec![
                "update",
                "settings",
                "set",
                "--spec-stdin",
                "--expect-revision",
                "4",
            ],
            vec!["update", "release-key", "generate", "--label", "release-1"],
            vec![
                "update",
                "release-key",
                "generate",
                "--label",
                "release-1",
                "--replace-existing",
            ],
            vec!["update", "release-key", "show", "--label", "release-1"],
            vec![
                "update",
                "sign-manifest",
                "--key",
                "release-1",
                "--key",
                "release-2",
                "--manifest-stdin",
            ],
            vec!["import", "--spec-stdin"],
            vec![
                "import",
                "--spec-stdin",
                "--date",
                "2026-11-10",
                "--lang",
                "en",
            ],
            vec!["export", "m-0000a1", "--spec-stdin", "--format", "txt"],
            vec!["setup", "--non-interactive", "--spec-stdin"],
            vec![
                "setup",
                "voice",
                "--from-file",
                "synthetic.wav",
                "--spec-stdin",
            ],
        ] {
            let mut argv = vec!["--json"];
            argv.extend(&args);
            assert!(parsed(&argv).json, "{args:?}");
        }
    }

    /// Destructive commands need their explicit mode, paging and follow
    /// bounds their own flags, and a stdin spec never mixes with the argv
    /// form of the values it carries (CL02, CL09).
    #[test]
    fn feature_commands_refuse_ambiguity() {
        for args in [
            vec!["queue", "prune"],
            vec!["queue", "prune", "--dry-run", "--yes"],
            vec!["queue", "prune", "--dry-run", "--limit", "0"],
            vec!["queue", "prune", "--dry-run", "--older-than", "soon"],
            vec!["queue", "prune", "--dry-run", "--state", "running"],
            vec!["retention", "apply", "--policy", "processed"],
            vec!["retention", "apply", "--dry-run", "--policy", "days:0"],
            vec!["retention", "apply", "--dry-run", "--policy", "forever"],
            vec!["retention", "policy"],
            vec!["retention", "policy", "days:0"],
            vec!["retention", "policy", "days:-1"],
            vec!["retention", "policy", "days:abc"],
            vec!["retention", "policy", "forever"],
            vec!["retention", "delete-now", "m-0000a1"],
            vec!["retention", "delete-now", "m-0000a1", "--dry-run", "--yes"],
            vec!["library", "repair", "--yes"],
            vec!["library", "repair", "--only", "index"],
            vec!["library", "repair", "--only", "everything", "--yes"],
            vec!["models", "doctor", "--profile", "balanced"],
            vec!["record", "live", "--max-events", "3"],
            vec!["record", "live", "--follow", "--max-events", "0"],
            vec!["record", "live", "--follow", "--max-seconds", "0"],
            vec!["meetings", "--cursor", "opaque"],
            vec!["meetings", "--page-size", "0"],
            vec!["meetings", "--page-size", "501"],
            vec!["meetings", "--page-size", "5", "--limit", "5"],
            vec![
                "people",
                "edit",
                "p-synthetic",
                "--spec-stdin",
                "--name",
                "X",
            ],
            vec![
                "people",
                "edit",
                "p-synthetic",
                "--spec-stdin",
                "--alias=+x",
            ],
            vec![
                "people",
                "edit",
                "p-synthetic",
                "--spec-stdin",
                "--email",
                "e",
            ],
            vec![
                "people",
                "edit",
                "p-synthetic",
                "--spec-stdin",
                "--notes-file",
                "n",
            ],
            vec!["people", "lookup"],
            vec!["people", "lookup", "Synthetic", "--query-stdin"],
            vec!["people", "undo-edit", "p-synthetic"],
            vec!["speakers", "live-dismiss", "m-0000a1", "--decision", "3"],
            vec![
                "speakers",
                "live-dismiss",
                "m-0000a1",
                "--review-key",
                "ab12",
            ],
            vec![
                "integrations",
                "select-calendars",
                "microsoft-graph",
                "--account",
                "11111111111111111111111111111111",
                "--generation",
                "3",
                "--selection-generation",
                "2",
            ],
            vec![
                "integrations",
                "select-calendars",
                "google",
                "--generation",
                "3",
                "--spec-stdin",
                "--target",
                r#"{"kind":"own","calendar_id":"synthetic"}"#,
            ],
            vec!["integrations", "cancel-login", "google"],
            vec!["update", "install"],
            vec!["update", "rollback"],
            vec!["update", "install", "--yes", "--expectation-stdin"],
            vec!["update", "rollback", "--yes", "--expectation-stdin"],
            vec!["update", "admission"],
            vec!["update", "admission", "release"],
            vec!["update", "admission", "release", "--fence", "-1"],
            vec!["update", "settings", "set"],
            vec!["update", "release-key", "generate"],
            vec!["update", "sign-manifest", "--manifest-stdin"],
            vec!["update", "sign-manifest", "--key", "release-1"],
            vec!["import", "--spec-stdin", "synthetic.m4a"],
            vec!["import", "--spec-stdin", "--title", "Synthetic"],
            vec!["import", "--spec-stdin", "--participant", "Synthetic"],
            vec!["import", "--spec-stdin", "--manifest", "synthetic.json"],
            vec![
                "integrations",
                "select-calendars",
                "google",
                "--generation",
                "3",
                "--spec-stdin",
            ],
            vec![
                "integrations",
                "setup",
                "system-calendar",
                "--spec-stdin",
                "--calendar-id",
                "synthetic@example.invalid",
            ],
            vec![
                "integrations",
                "setup",
                "system-calendar",
                "--spec-stdin",
                "--clear-calendars",
            ],
            vec![
                "integrations",
                "setup",
                "google",
                "--spec-stdin",
                "--client-secret-stdin",
            ],
            vec![
                "export",
                "m-0000a1",
                "--spec-stdin",
                "--out",
                "synthetic.md",
            ],
            vec!["setup", "--spec-stdin"],
            vec!["setup", "--non-interactive", "--spec-stdin", "--name", "X"],
            vec![
                "setup",
                "--non-interactive",
                "--spec-stdin",
                "--people-file",
                "synthetic.csv",
            ],
            vec![
                "setup",
                "voice",
                "--from-file",
                "synthetic.wav",
                "--spec-stdin",
                "--email",
                "e",
            ],
            vec!["app", "reading-size", "huge"],
        ] {
            let mut argv = vec!["--json"];
            argv.extend(&args);
            assert!(usage(&argv).json, "{args:?}");
        }
    }

    #[test]
    fn prune_flags_become_core_criteria() {
        let prune = |args: &[&str]| match parsed(args).command {
            Command::Queue {
                action: QueueCmd::Prune(a),
            } => a.criteria(),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            prune(&["queue", "prune", "--dry-run"]),
            PruneCriteria::default()
        );
        assert_eq!(
            prune(&[
                "queue",
                "prune",
                "--yes",
                "--state",
                "failed",
                "--older-than",
                "2w",
                "--kind",
                "import",
                "--quarantine-invalid",
                "--limit",
                "7",
            ]),
            PruneCriteria {
                done: false,
                failed: true,
                min_age: std::time::Duration::from_secs(14 * 86_400),
                kinds: vec!["import".to_owned()],
                quarantine_invalid: true,
                limit: 7,
            }
        );
    }

    /// The stdin specs: exact fields, unknown ones refused.
    #[test]
    fn stdin_specs_refuse_unknown_fields() {
        let import: ImportSpec = serde_json::from_str(
            r#"{"path":"/synthetic/a.m4a","title":"Synthetic","participants":["Ana <ana@example.invalid>"]}"#,
        )
        .unwrap();
        assert_eq!(
            import,
            ImportSpec::Recording {
                path: "/synthetic/a.m4a".into(),
                title: Some("Synthetic".into()),
                participants: vec!["Ana <ana@example.invalid>".into()],
            }
        );
        assert_eq!(
            serde_json::from_str::<ImportSpec>(r#"{"manifest":"relative/manifest.json"}"#).unwrap(),
            ImportSpec::Manifest("relative/manifest.json".into())
        );
        for refused in [
            r#"{"title":"Synthetic"}"#,
            r#"{"path":"a","date":"x"}"#,
            r#"{}"#,
            r#"{"path":"a","manifest":"m.json"}"#,
            r#"{"manifest":"m.json","title":"Synthetic"}"#,
            r#"{"manifest":"m.json","participants":["Ana"]}"#,
            r#"{"manifest":"m.json","lang":"en"}"#,
        ] {
            assert!(
                serde_json::from_str::<ImportSpec>(refused).is_err(),
                "{refused}"
            );
        }
        assert_eq!(
            serde_json::from_str::<SetupSpec>(r#"{"people_file":"/synthetic/people.csv"}"#)
                .unwrap(),
            SetupSpec {
                people_file: Some("/synthetic/people.csv".into()),
                ..SetupSpec::default()
            }
        );
        assert!(serde_json::from_str::<SetupSpec>(r#"{"words_file":"w.txt"}"#).is_err());
        assert!(
            serde_json::from_str::<IdentitySpec>(r#"{"people_file":"/synthetic/p.csv"}"#).is_err()
        );
        assert_eq!(
            serde_json::from_str::<SystemCalendarSelectionSpec>(
                r#"{"calendar_ids":["synthetic@example.invalid"]}"#
            )
            .unwrap()
            .calendar_ids,
            ["synthetic@example.invalid"]
        );
        assert!(serde_json::from_str::<SystemCalendarSelectionSpec>("{}").is_err());
        assert!(
            serde_json::from_str::<SystemCalendarSelectionSpec>(
                r#"{"calendar_ids":[],"targets":[]}"#
            )
            .is_err()
        );
        assert_eq!(
            serde_json::from_str::<ExportSpec>(r#"{"out":"-"}"#).unwrap(),
            ExportSpec { out: "-".into() }
        );
        assert!(serde_json::from_str::<ExportSpec>(r#"{"out":"-","format":"md"}"#).is_err());
        assert_eq!(
            serde_json::from_str::<IdentitySpec>("{}").unwrap(),
            IdentitySpec::default()
        );
        assert!(serde_json::from_str::<IdentitySpec>(r#"{"name":"X","email":"e"}"#).is_err());
        assert!(serde_json::from_str::<LookupSpec>(r#"{"query":"x","person":"y"}"#).is_err());
        let selection: CalendarSelectionSpec = serde_json::from_str(
            r#"{"targets":[{"kind":"delegated","mailbox":"synthetic@example.invalid","calendar_id":null}]}"#,
        )
        .unwrap();
        assert_eq!(selection.targets.len(), 1);
        assert!(selection.calendar_ids.is_empty());
        assert!(serde_json::from_str::<CalendarSelectionSpec>(r#"{"target":[]}"#).is_err());
        assert!(serde_json::from_str::<CalendarRemovalSpec>(r#"{"targets":[]}"#).is_err());
    }
}
