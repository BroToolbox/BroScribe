//! The agent helper owns capture grants; inference stays in child workers.
use broscribe_detect::SystemClock;
use broscribe_platform::paths::{ENV_DATA_DIR, resolve_paths_from};
use broscribe_platform_native::NativePlatform;
use broscribe_platform_native::ipc;
use broscribed::integrations::{AccountMaintenance, IntegrationMaintenance};
use broscribed::{Daemon, DaemonConfig, WorkerCommand};
use clap::Parser;
use std::{ffi::OsString, path::PathBuf, sync::Arc};
#[derive(Parser)]
struct Args {
    #[arg(long)]
    library: Option<PathBuf>,
    #[arg(long)]
    worker: Option<PathBuf>,
    /// The data folder, exactly as `BROSCRIBE_DATA_DIR` sets it (the cache
    /// moves to `<PATH>/cache`); wins over the environment. For login items
    /// that cannot carry environment variables.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}
fn main() -> broscribe_core::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let paths = NativePlatform::paths();
    let data_flag = args.data_dir.clone().map(OsString::from);
    let paths = resolve_paths_from(paths.as_ref(), |key| {
        if key == ENV_DATA_DIR && data_flag.is_some() {
            data_flag.clone()
        } else {
            std::env::var_os(key)
        }
    })?;
    let data_dir = paths.data_dir.clone();
    broscribed::server::run_admitted(&data_dir, move |admission| async move {
        // Admission precedes endpoint locking and every normal-mode factory.
        let Some(startup) = broscribe_core::lock::DaemonStartupLock::try_acquire(&paths.data_dir)?
        else {
            // Existing startup or a stop command owns the endpoint handoff.
            return Ok(());
        };
        // Before binding: clients connect as soon as the endpoint exists and
        // wait on short budgets. Only the build information is read (a small
        // file), never the full About facts (a code-signature validation on
        // macOS and Windows). Maintenance mode serves right after binding;
        // normal mode still loads the library, detection and integration
        // accounts (the system trust store) before serving.
        let version = env!("CARGO_PKG_VERSION");
        let build = match NativePlatform::about_reader(version).read_build() {
            Ok(build) => broscribed::daemon::agent_build(version, &build),
            Err(error) => {
                tracing::warn!(
                    code = error.code(),
                    "reading the running build's information"
                );
                None
            }
        };
        let listener = ipc::Listener::bind(&paths.data_dir).map_err(|error| {
            broscribe_core::Error::io(
                "binding the BroScribe Agent endpoint",
                std::io::Error::other(error),
            )
        })?;
        drop(startup);
        if admission.mode() == broscribe_core::maintenance::Mode::Maintenance {
            return broscribed::server::serve_control(listener, build)
                .await
                .map_err(Into::into);
        }
        // A broken config.toml or detect.toml never stops normal capture startup.
        let config = broscribed::daemon::config_or_default(&paths.data_dir);
        let library = paths.library(args.library.as_deref(), config.library.as_deref());
        let engine = broscribe_capture::Recorder::new(
            Arc::from(NativePlatform::mic_capture()),
            NativePlatform::system_audio_capture(),
            Arc::from(NativePlatform::system_info()),
            NativePlatform::process_priority(),
        );
        let worker = args
            .worker
            .map(WorkerCommand::new)
            .map(Ok)
            .unwrap_or_else(WorkerCommand::sibling)?;
        let mut cfg = DaemonConfig::new(library, paths.data_dir.clone(), paths.cache_dir, worker);
        cfg.data_dir_overridden = paths.data_dir_overridden;
        cfg.build = build;
        let daemon = Daemon::new(
            cfg,
            Arc::new(engine),
            Arc::from(NativePlatform::process_probe()),
            admission,
        )?;
        daemon.start_detection(
            NativePlatform::mic_usage_monitor(),
            NativePlatform::window_inspector(),
            Box::new(SystemClock),
        )?;
        let accounts = Arc::new(
            AccountMaintenance::new(
                paths.data_dir.clone(),
                Arc::from(NativePlatform::secret_store(&paths.data_dir)),
                Arc::from(NativePlatform::browser()),
            )
            .with_system_calendar(Arc::from(NativePlatform::system_calendar()))
            // Corporate roots too; an unreadable store fails only provider
            // traffic closed, never native calendars or sign-out.
            .with_trust_store(NativePlatform::trust_store()),
        );
        if let Err(error) = accounts.reload() {
            tracing::warn!(
                code = error.code,
                "integration configuration could not be loaded"
            );
        }
        daemon.set_integrations(accounts);
        // The agent is the login item: it shows the menu-bar app when the user wants it at login.
        daemon.launch_at_login(&config);
        broscribed::server::serve(daemon, listener).await?;
        Ok(())
    })
}
