//! Local benchmark commands. Each candidate runs in a fresh child process;
//! hypotheses travel only through its stdout pipe, never through argv or logs.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use broscribe_core::{Error, Result};
use broscribe_pipeline::Env;
use broscribe_pipeline::bench::metrics::Metric;
use broscribe_pipeline::bench::worker::{WorkerOptions, WorkerReport, run_worker};
use broscribe_pipeline::bench::{self, RunOptions, WorkerRequest};
use broscribe_platform_native::NativePlatform;
use clap::{Args, Subcommand};

use crate::args::parse_clock;
use crate::{Ctx, to_value};

#[derive(Debug, Subcommand)]
pub(crate) enum BenchCmd {
    /// Build a local labelled set from meeting excerpts.
    Excerpt {
        #[command(subcommand)]
        action: ExcerptCmd,
    },
    /// Compare candidates (active, default, or model IDs joined by '+').
    Run(RunArgs),
    /// Internal child process: emit exactly one worker JSON report.
    #[command(hide = true)]
    Worker(WorkerArgs),
}

#[derive(Debug, Subcommand)]
pub(crate) enum ExcerptCmd {
    /// Copy this range's transcript as a reference to correct by hand.
    Add {
        meeting: String,
        /// Seconds, mm:ss or hh:mm:ss.
        from: String,
        /// Seconds, mm:ss or hh:mm:ss.
        to: String,
        #[arg(long)]
        set: String,
    },
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    #[arg(required = true, num_args = 1..)]
    candidates: Vec<String>,
    #[arg(long)]
    set: String,
    /// Speaker hints used as references and for optional calibration.
    #[arg(long)]
    manifest: Option<PathBuf>,
    /// Fit thresholds per embedding model; re-resolve only the active model.
    #[arg(long, requires = "manifest")]
    calibrate: bool,
}

#[derive(Debug, Args)]
pub(crate) struct WorkerArgs {
    #[arg(long)]
    candidate: String,
    #[arg(long)]
    set: String,
    #[arg(long)]
    manifest: Option<PathBuf>,
    #[arg(long, requires = "manifest")]
    calibrate_only: bool,
}

pub(super) fn dispatch(ctx: &mut Ctx<'_>, action: BenchCmd) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let env = Env {
        lib: &lib,
        config: &config,
        decoders: &ctx.deps.decoder,
        probe: ctx.deps.process.as_ref(),
        progress: Some(ctx.pipeline_progress()),
        execution: &ctx.deps.execution,
    };
    match action {
        BenchCmd::Excerpt {
            action:
                ExcerptCmd::Add {
                    meeting,
                    from,
                    to,
                    set,
                },
        } => {
            let added = bench::add_excerpt(
                &env,
                ctx.data_dir(),
                &set,
                &super::meeting_id(&meeting)?,
                parse_clock(&from)?,
                parse_clock(&to)?,
            )?;
            for note in &added.notes {
                ctx.note(note);
            }
            ctx.emit(to_value(&added)?, |out| {
                writeln!(
                    out,
                    "Added {} to set {} ({:.1}–{:.1} s, {} reference words).",
                    added.excerpt_id, added.set, added.start, added.end, added.reference_words
                )?;
                writeln!(
                    out,
                    "Correct reference_text in set.json, then set reference_checked to true."
                )
            })
        }
        BenchCmd::Run(args) => {
            let executable = std::env::current_exe()
                .map_err(|e| Error::io("locating the benchmark executable", e))?;
            let library = std::path::absolute(ctx.library_path(&config).0)
                .map_err(|error| Error::io("resolving benchmark library", error))?;
            let data = ctx.data_dir().to_path_buf();
            let data_override = ctx.paths.data_dir_overridden.then_some(data.as_path());
            let mut launch = |request: &WorkerRequest| {
                let output = worker_command(&executable, &library, data_override, request)
                    .output()
                    .map_err(|e| Error::io("starting a benchmark worker", e))?;
                // On failure stdout may contain a partial hypothesis: never
                // include it in an error or store it in the results.
                if !output.status.success() {
                    return Err(Error::Internal(format!(
                        "bench worker exited with {}",
                        output.status
                    )));
                }
                decode_report(&output.stdout, &request.candidate)
            };
            let result = bench::run(
                &env,
                &data,
                &RunOptions {
                    set: args.set,
                    candidates: args.candidates,
                    manifest: args.manifest,
                    calibrate: args.calibrate,
                },
                &mut launch,
            )?;
            ctx.emit(to_value(&result)?, |out| {
                writeln!(out, "Candidate\tWER\tDER\tJER\tClusters/speaker\tNaming\tEstimated fixes/meeting\tSeconds\tPeak bytes")?;
                for candidate in &result.results.candidates {
                    let Some(m) = &candidate.metrics else {
                        writeln!(out, "{}\tunavailable: {}", candidate.candidate,
                            candidate.error.as_deref().unwrap_or("worker failed"))?;
                        continue;
                    };
                    writeln!(out, "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        candidate.candidate, figure(&m.wer), figure(&m.der), figure(&m.jer),
                        figure(&m.clusters_per_true_speaker), figure(&m.naming_accuracy),
                        figure(&m.estimated_manual_fixes_per_meeting),
                        figure(&m.time_to_transcript_s), figure(&m.peak_rss_bytes))?;
                    if let Some(source) = &m.diarization_reference {
                        writeln!(out, "  Diarization reference: {source}; collar 0.25 s.")?;
                    }
                }
                writeln!(out, "Results saved locally.")
            })
        }
        BenchCmd::Worker(args) => {
            let usage = NativePlatform::resource_usage();
            let report = run_worker(
                &env,
                ctx.deps.models.as_ref(),
                ctx.data_dir(),
                &WorkerOptions {
                    set: args.set,
                    candidate: args.candidate,
                    manifest: args.manifest,
                    calibrate_only: args.calibrate_only,
                },
                usage.as_ref(),
            )?;
            // A worker always emits JSON, even without the global --json.
            // It is an internal pipe protocol, not human output.
            crate::print_json(ctx.out, &to_value(&report)?)
        }
    }
}

fn worker_command(
    executable: &Path,
    library: &Path,
    data_override: Option<&Path>,
    request: &WorkerRequest,
) -> Command {
    let mut command = Command::new(executable);
    command
        .arg("--json")
        .arg("--library")
        .arg(library)
        .args(["bench", "worker", "--set"])
        .arg(&request.set)
        .arg("--candidate")
        .arg(&request.candidate)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    match data_override {
        Some(data) => command.env(broscribe_platform::paths::ENV_DATA_DIR, data),
        None => command.env_remove(broscribe_platform::paths::ENV_DATA_DIR),
    };
    if let Some(manifest) = &request.manifest {
        command.arg("--manifest").arg(manifest);
    }
    if request.calibrate_only {
        command.arg("--calibrate-only");
    }
    command
}

fn decode_report(bytes: &[u8], candidate: &str) -> Result<WorkerReport> {
    let report: WorkerReport = serde_json::from_slice(bytes)
        // serde's data errors may quote transcript text: report no raw data.
        .map_err(|_| Error::Internal("bench worker returned an invalid JSON report".into()))?;
    report.check(candidate)?;
    Ok(report)
}

fn figure(metric: &Metric) -> String {
    match metric {
        Metric::Value { value, .. } => format!("{value:.3}"),
        Metric::Unavailable { unavailable } => format!("unavailable ({unavailable})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Deps;
    use crate::cli::Cli;
    use broscribe_core::schema::{self, Meeting, Segment, Source};
    use broscribe_core::{Library, MeetingId};
    use broscribe_models::fakes::FakeModelProvider;
    use broscribe_platform::fakes::{FakeMicCapture, FakePaths, FakeProcessProbe, FakeSecretStore};
    use broscribe_platform::{DecoderChain, resolve_paths};
    use clap::Parser;
    use std::ffi::OsString;

    #[test]
    fn bench_grammar_requires_set_candidates_and_calibration_manifest() {
        for args in [
            vec!["broscribe", "bench", "run", "active", "--set", "synthetic"],
            vec![
                "broscribe",
                "bench",
                "run",
                "active",
                "nemo-titanet-small",
                "--set",
                "synthetic",
                "--manifest",
                "hints.json",
                "--calibrate",
            ],
            vec![
                "broscribe",
                "bench",
                "excerpt",
                "add",
                "m-000001",
                "00:01",
                "00:03",
                "--set",
                "synthetic",
            ],
        ] {
            Cli::try_parse_from(args).unwrap();
        }
        for args in [
            vec!["broscribe", "bench", "run", "--set", "synthetic"],
            vec!["broscribe", "bench", "run", "active"],
            vec![
                "broscribe",
                "bench",
                "run",
                "active",
                "--set",
                "synthetic",
                "--calibrate",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn invalid_worker_report_does_not_disclose_transcript() {
        let error =
            decode_report(br#"{"text":"synthetic confidential phrase"}"#, "active").unwrap_err();
        assert!(!error.to_string().contains("confidential"));
    }

    #[test]
    fn bench_excerpt_json_and_worker_protocol_through_cli() {
        let temp = tempfile::tempdir().unwrap();
        let deps = Deps {
            paths: Box::new(FakePaths::new(temp.path())),
            updates: crate::UpdateDeps::with_transport(
                std::sync::Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                    std::path::Path::new(&temp.path()).join("install"),
                    "synthetic-test-target",
                )),
                std::sync::Arc::new(broscribe_updates::FakeTransport::default()),
            ),
            models: Box::new(FakeModelProvider::default()),
            decoder: DecoderChain::default(),
            process: Box::new(FakeProcessProbe::new(std::process::id())),
            mic: Box::new(FakeMicCapture::new(16_000)),
            secrets: std::sync::Arc::new(FakeSecretStore::default()),
            browser: std::sync::Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
            agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
            autostart: Box::new(broscribe_platform::fakes::FakeAutostart::default()),
            agents: crate::AgentEnv::isolated(temp.path()),
            execution: broscribe_models::ExecutionContext::new(
                std::sync::Arc::new(broscribe_platform::fakes::FakeProcessTreeControl::default()),
                std::sync::Arc::new(broscribe_platform::fakes::FakeTrustStore::default()),
            ),
            clock: crate::WallClock::System,
        };
        let paths = resolve_paths(deps.paths.as_ref()).unwrap();
        let lib = Library::open(&paths.default_library_dir).unwrap();
        let mut meeting = Meeting::new(
            MeetingId::parse("m-000001").unwrap(),
            Source::Import,
            schema::now(),
        );
        meeting.duration_s = 5.0;
        meeting.segments.push(Segment {
            speech_overlap: Vec::new(),
            id: 0,
            track: "t0".into(),
            speaker: None,
            start: 0.0,
            end: 2.0,
            text: "synthetic reference".into(),
            words: vec![],
        });
        lib.write(|tx| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)
        })
        .unwrap();
        let invoke = |args: &[&str]| {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let argv = std::iter::once("broscribe")
                .chain(args.iter().copied())
                .map(OsString::from)
                .collect();
            let code = crate::run(argv, &deps, &mut out, &mut err);
            assert_eq!(
                code,
                0,
                "{}{}",
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&err)
            );
            out
        };
        let bytes = invoke(&[
            "--json",
            "bench",
            "excerpt",
            "add",
            "m-000001",
            "0",
            "3",
            "--set",
            "synthetic",
        ]);
        let added: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(added["reference_words"], 2);
        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic reference"));
        let set = bench::load_set(&paths.data_dir, "synthetic").unwrap();
        assert_eq!(
            set.excerpts[0].reference_text.as_deref(),
            Some("synthetic reference")
        );
        assert!(!set.excerpts[0].reference_checked);
        // No recording: the worker still returns a valid report, with an
        // unavailable excerpt instead of fabricating a zero score.
        let bytes = invoke(&[
            "bench",
            "worker",
            "--candidate",
            "active",
            "--set",
            "synthetic",
        ]);
        let report = decode_report(&bytes, "active").unwrap();
        assert!(report.excerpts[0].error.is_some());
        assert!(report.peak_rss_bytes.is_some_and(|b| b > 0));
    }
}
