//! One process per job; sensitive material never travels in arguments.
use broscribe_core::config::{Config, Profile};
use broscribe_core::{Error, Library, Result, queue::JobId};
use broscribe_ipc::{LiveEvent, WorkerProblem};
use broscribe_models::{ExecutionContext, Role, SherpaProvider};
use broscribe_platform::resolve_paths;
use broscribe_platform_native::NativePlatform;
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    library: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    RunOne {
        job_id: String,
    },
    Live {
        session_id: String,
    },
    /// The companion live-speakers worker of a recording (plan D); the
    /// session's start line arrives on stdin.
    LiveSpeakers {
        session_id: String,
    },
    /// Isolated measurement replay (plan D): raw mono little-endian f32 PCM
    /// written into a synthetic journal block by block and analysed by the
    /// live-speakers code with the profile's models; no capture, no daemon.
    /// Prints the worker's events (`--events`) and a summary JSON line
    /// (performance and end-to-end anchor checks; counts only).
    LiveSpeakersReplay {
        #[arg(long)]
        pcm: PathBuf,
        #[arg(long, default_value_t = 16_000)]
        rate: u32,
        /// `system` or `mic`.
        #[arg(long, default_value = "system")]
        track: String,
        /// 0: as fast as possible; 1: real time.
        #[arg(long, default_value_t = 1.0)]
        pace: f64,
        /// Synthetic device clock deviation (ppm).
        #[arg(long, default_value_t = 0.0)]
        drift_ppm: f64,
        /// Insert a pause after this many seconds of audio…
        #[arg(long)]
        pause_at_s: Option<f64>,
        /// …lasting this long.
        #[arg(long, default_value_t = 0.0)]
        pause_s: f64,
        #[arg(long)]
        window_s: Option<f64>,
        #[arg(long)]
        hop_s: Option<f64>,
        /// Folder for the synthetic journal (default: a new temporary one,
        /// removed afterwards).
        #[arg(long)]
        journal_root: Option<PathBuf>,
        #[arg(long)]
        events: bool,
    },
    Retention {
        #[arg(long = "meeting")]
        meetings: Vec<String>,
    },
}
fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("worker failed ({})", e.code());
            ExitCode::FAILURE
        }
    }
}
fn run(args: Args) -> broscribe_core::Result<()> {
    let paths = NativePlatform::paths();
    let paths = resolve_paths(paths.as_ref())?;
    let lease = broscribe_core::maintenance::ModeLease::acquire(
        &paths.data_dir,
        broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
    )?;
    lease.require_normal()?;
    let _ = NativePlatform::process_priority()
        .set_current_process(broscribe_platform::PriorityClass::Utility);
    let config = load_config(&args.command, &paths.data_dir, &mut std::io::stdout())?;
    let root = paths.library(args.library.as_deref(), config.library.as_deref());
    let lib = Library::open(root)?;
    let probe = NativePlatform::process_probe();
    match args.command {
        Command::Live { session_id: _ } => {
            let mut out = std::io::stdout();
            let (provider, live) = live_settings(&mut out, &paths.data_dir, &config.profile)?;
            let (mut vad, asr) = broscribe_worker::load_live_models(&mut out, &live, || {
                Ok((
                    provider.load_vad(&config.profile.vad)?,
                    provider.load_live(&live)?,
                ))
            })
            .map_err(|e| live_failed(&mut out, LiveStage::Load, e))?;
            broscribe_worker::live(
                &mut std::io::stdin().lock(),
                &mut std::io::stdout().lock(),
                &mut vad,
                &asr,
                &live,
            )
        }
        Command::LiveSpeakers { session_id } => {
            let mut input = std::io::BufReader::new(std::io::stdin());
            let start = broscribe_worker::speakers::read_start(&mut input)?;
            if start.session_id != session_id {
                return Err(broscribe_core::Error::InvalidInput(
                    "live speakers start names another session".into(),
                ));
            }
            let (diarizer, embedder) = broscribe_worker::speakers::load_models(
                &mut std::io::stdout(),
                &config.profile.diarization,
                &config.profile.speaker_embedding,
                || {
                    let provider = SherpaProvider::from_data_dir(&paths.data_dir)?;
                    Ok((
                        provider.load_diarizer(
                            &config.profile.diarization,
                            &config.profile.speaker_embedding,
                        )?,
                        provider.load_speaker_embedder(&config.profile.speaker_embedding)?,
                    ))
                },
            )?;
            let mut evidence = broscribe_pipeline::live_identity::LibraryEvidence::new(
                lib.clone(),
                Some(paths.data_dir.clone()),
                config.profile.speaker_embedding.clone(),
            );
            broscribe_worker::speakers::run(
                &start,
                Box::new(input),
                Box::new(std::io::stdout()),
                &broscribe_pipeline::live_identity::SpeakerModels {
                    diarizer: &diarizer,
                    embedder: &embedder,
                },
                (
                    config.profile.diarization.as_str(),
                    config.profile.speaker_embedding.as_str(),
                ),
                &mut evidence,
            )
        }
        Command::LiveSpeakersReplay {
            pcm,
            rate,
            track,
            pace,
            drift_ppm,
            pause_at_s,
            pause_s,
            window_s,
            hop_s,
            journal_root,
            events,
        } => {
            let track = match track.as_str() {
                "system" => broscribe_ipc::Track::System,
                "mic" => broscribe_ipc::Track::Mic,
                _ => {
                    return Err(broscribe_core::Error::InvalidInput(
                        "--track is system or mic".into(),
                    ));
                }
            };
            let provider = SherpaProvider::from_data_dir(&paths.data_dir)?;
            let diarizer = provider.load_diarizer(
                &config.profile.diarization,
                &config.profile.speaker_embedding,
            )?;
            let embedder = provider.load_speaker_embedder(&config.profile.speaker_embedding)?;
            let mut evidence = broscribe_pipeline::live_identity::LibraryEvidence::new(
                lib.clone(),
                Some(paths.data_dir.clone()),
                config.profile.speaker_embedding.clone(),
            );
            broscribe_worker::speakers::replay_file(
                &broscribe_worker::speakers::ReplayFile {
                    pcm,
                    rate,
                    track,
                    pace,
                    drift_ppm,
                    pause: pause_at_s.map(|at| (at, pause_s)),
                    window_s,
                    hop_s,
                    journal_root,
                    events,
                },
                &broscribe_pipeline::live_identity::SpeakerModels {
                    diarizer: &diarizer,
                    embedder: &embedder,
                },
                &mut evidence,
                &mut std::io::stdout().lock(),
            )
        }
        Command::RunOne { job_id } => {
            let provider = SherpaProvider::from_data_dir(&paths.data_dir)?;
            let decoders = NativePlatform::decoder_chain();
            let execution = ExecutionContext::new(
                NativePlatform::process_tree_control(),
                NativePlatform::trust_store(),
            );
            let env = broscribe_pipeline::Env {
                lib: &lib,
                config: &config,
                decoders: &decoders,
                probe: probe.as_ref(),
                execution: &execution,
                progress: None,
            };
            let secrets = std::sync::Arc::from(NativePlatform::secret_store(&paths.data_dir));
            let opts = broscribe_pipeline::worker::WorkerOptions {
                cache_dir: paths.cache_dir,
                once: true,
                encoder: NativePlatform::audio_encoder(),
                calendar_authority: std::sync::Arc::from(NativePlatform::calendar_authority(
                    &paths.data_dir,
                )),
                data_dir: paths.data_dir,
                secrets,
            };
            let result = broscribe_pipeline::worker::work_job(
                &env,
                &provider,
                &opts,
                &JobId::parse(&job_id)?,
                &mut |event| {
                    use std::io::Write;
                    let mut out = std::io::stdout().lock();
                    if serde_json::to_writer(&mut out, event).is_ok() {
                        let _ = writeln!(out);
                        let _ = out.flush();
                    }
                },
            )?;
            // Search vectors for what the job completed, outside the lock,
            // under the consent configured now (best effort: the job's
            // outcome stands; stdout is the event stream, so notes go to
            // stderr).
            if result.batches_resolved || !result.imported.is_empty() {
                let updated = Config::load(&opts.data_dir)
                    .and_then(|current| {
                        broscribe_pipeline::vectors::text_model(
                            &current,
                            &opts.data_dir,
                            opts.secrets.as_ref(),
                            &execution,
                        )
                    })
                    .and_then(|model| match model.embedder() {
                        Some(embedder) => {
                            broscribe_pipeline::vectors::embed_completed(&lib, embedder, &result)
                        }
                        None => Ok(()),
                    });
                if let Err(e) = updated {
                    eprintln!("note: search vectors not updated ({})", e.code());
                }
            }
            if result.failed > 0 {
                return Err(broscribe_core::Error::Internal("job failed".into()));
            }
            Ok(())
        }
        Command::Retention { meetings } => {
            let meetings = if meetings.is_empty() {
                None
            } else {
                Some(
                    meetings
                        .iter()
                        .map(|id| broscribe_core::MeetingId::parse(id))
                        .collect::<broscribe_core::Result<Vec<_>>>()?,
                )
            };
            broscribe_pipeline::retention::apply_current(
                &lib,
                &paths.data_dir,
                probe.as_ref(),
                &broscribe_pipeline::retention::RetentionOptions {
                    meetings,
                    cache_dir: Some(paths.cache_dir),
                    ..Default::default()
                },
            )?;
            Ok(())
        }
    }
}

/// Where a live worker failed.
#[derive(Clone, Copy)]
enum LiveStage {
    /// Its settings: the config, the model registry and the profile's live
    /// and VAD models, all checked before any model loads.
    Settings,
    /// Loading the checked models.
    Load,
}

/// The terminal problem of a live worker failure, by stage and error kind;
/// `None`: a crash the daemon restarts (I/O, decoding, inference, …).
fn live_terminal(stage: LiveStage, e: &Error) -> Option<WorkerProblem> {
    match (stage, e) {
        (_, Error::ModelMissing(_)) => Some(WorkerProblem::ModelMissing),
        (
            LiveStage::Settings,
            Error::InvalidInput(_)
            | Error::Unsupported(_)
            | Error::Schema(_)
            | Error::NotConfigured(_),
        ) => Some(WorkerProblem::InvalidSettings),
        _ => None,
    }
}

/// Ends a live worker on `e`. A terminal failure first writes the
/// [`LiveEvent::Unavailable`] line on `out`, so the daemon shows the
/// preview unavailable instead of restarting a worker that cannot run.
fn live_failed(out: &mut dyn Write, stage: LiveStage, e: Error) -> Error {
    if let Some(reason) = live_terminal(stage, &e)
        && let Ok(line) = (LiveEvent::Unavailable { reason }).to_line()
    {
        // Best effort: a daemon that misses it still sees the exit.
        let _ = writeln!(out, "{line}").and_then(|()| out.flush());
    }
    e
}

/// The config. A live worker whose config cannot be used ends as
/// [`live_failed`] does at its settings stage.
fn load_config(command: &Command, data_dir: &Path, out: &mut dyn Write) -> Result<Config> {
    Config::load(data_dir).map_err(|e| match command {
        Command::Live { .. } => live_failed(out, LiveStage::Settings, e),
        _ => e,
    })
}

/// The live worker's settings stage, before any model loads: the model
/// registry (with the user's `models.toml`), the profile's declared live
/// model (never another one picked because it happens to be installed) and
/// its VAD model, checked for the role and runtime loading needs. Returns
/// the provider and the live model's ID; a failure ends the worker as
/// [`live_failed`] does.
fn live_settings(
    out: &mut dyn Write,
    data_dir: &Path,
    profile: &Profile,
) -> Result<(SherpaProvider, String)> {
    resolve_live(data_dir, profile).map_err(|e| live_failed(out, LiveStage::Settings, e))
}

fn resolve_live(data_dir: &Path, profile: &Profile) -> Result<(SherpaProvider, String)> {
    let provider = SherpaProvider::from_data_dir(data_dir)?;
    let registry = provider.registry();
    let vad = registry.require(&profile.vad)?;
    if !vad.has_role(Role::Vad) {
        return Err(Error::InvalidInput(format!(
            "model {} does not have the vad role",
            vad.id
        )));
    }
    if vad.runtime != broscribe_models::sherpa::RUNTIME {
        return Err(Error::Unsupported(format!(
            "model {} runs on `{}`, not `{}`",
            vad.id,
            vad.runtime,
            broscribe_models::sherpa::RUNTIME
        )));
    }
    let live = registry.live_model(profile)?.id.clone();
    Ok((provider, live))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(out: &[u8]) -> Vec<LiveEvent> {
        std::str::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| LiveEvent::parse(l).unwrap())
            .collect()
    }

    fn unavailable(reason: WorkerProblem) -> Option<LiveEvent> {
        Some(LiveEvent::Unavailable { reason })
    }

    /// The settings stage over a synthetic data folder and `profile`: the
    /// error code and the last line the worker wrote.
    fn settings(data_dir: &Path, profile: &Profile) -> (&'static str, Option<LiveEvent>) {
        let mut out = Vec::new();
        let err = live_settings(&mut out, data_dir, profile)
            .map(|_| ())
            .unwrap_err();
        (err.code(), events(&out).pop())
    }

    #[test]
    fn a_missing_live_model_ends_with_the_terminal_unavailable_line() {
        let mut out = Vec::new();
        let err = broscribe_worker::load_live_models(&mut out, "live-m", || -> Result<()> {
            Err(Error::ModelMissing("live-m is not installed".into()))
        })
        .map_err(|e| live_failed(&mut out, LiveStage::Load, e))
        .unwrap_err();
        assert_eq!(err.code(), "model_missing");
        assert_eq!(events(&out).pop(), unavailable(WorkerProblem::ModelMissing));
    }

    /// Loading failures other than a missing model, and settings that
    /// could not be read for a transient reason, are crashes the daemon
    /// retries: no terminal line.
    #[test]
    fn other_failures_stay_crashes() {
        for (stage, e) in [
            (LiveStage::Load, Error::Inference("synthetic".into())),
            (LiveStage::Load, Error::InvalidInput("synthetic".into())),
            (
                LiveStage::Settings,
                Error::io("reading", std::io::Error::other("synthetic")),
            ),
        ] {
            let mut out = Vec::new();
            let code = e.code();
            assert_eq!(live_failed(&mut out, stage, e).code(), code);
            assert!(events(&out).is_empty(), "{code}");
        }
    }

    /// A hand-edited profile whose live or VAD setting cannot run ends the
    /// worker as invalid settings; an unlisted ASR model as missing.
    #[test]
    fn hand_edited_profiles_end_the_worker_with_their_terminal_problem() {
        let tmp = tempfile::tempdir().unwrap();
        let default = Config::default().profile;
        let vad_as_asr = Profile {
            asr: default.vad.clone(),
            ..default.clone()
        };
        assert_eq!(
            settings(tmp.path(), &vad_as_asr),
            ("invalid_input", unavailable(WorkerProblem::InvalidSettings))
        );
        let asr_as_vad = Profile {
            vad: default.asr.clone(),
            ..default.clone()
        };
        assert_eq!(
            settings(tmp.path(), &asr_as_vad),
            ("invalid_input", unavailable(WorkerProblem::InvalidSettings))
        );
        let unlisted = Profile {
            asr: "not-a-model".into(),
            ..default.clone()
        };
        assert_eq!(
            settings(tmp.path(), &unlisted),
            ("model_missing", unavailable(WorkerProblem::ModelMissing))
        );
        let mut out = Vec::new();
        let (_, live) = live_settings(&mut out, tmp.path(), &default).unwrap();
        assert!(!live.is_empty());
        assert!(out.is_empty());
    }

    /// An unreadable config or user model registry ends the live worker as
    /// invalid settings; other commands only fail.
    #[test]
    fn unusable_config_files_end_the_live_worker_as_invalid_settings() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(Config::path(tmp.path()), "profile = [[[").unwrap();
        let live = Command::Live {
            session_id: "synthetic".into(),
        };
        let mut out = Vec::new();
        let err = load_config(&live, tmp.path(), &mut out).unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        assert_eq!(
            events(&out).pop(),
            unavailable(WorkerProblem::InvalidSettings)
        );
        let mut out = Vec::new();
        let retention = Command::Retention {
            meetings: Vec::new(),
        };
        load_config(&retention, tmp.path(), &mut out).unwrap_err();
        assert!(out.is_empty());

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("models.toml"), "[[[").unwrap();
        assert_eq!(
            settings(tmp.path(), &Config::default().profile),
            ("invalid_input", unavailable(WorkerProblem::InvalidSettings))
        );
    }
}
