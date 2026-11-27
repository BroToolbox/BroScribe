//! `broscribe`: wires the native platform into [`broscribe_cli::run`].

use std::process::ExitCode;

use broscribe_cli::{AgentEnv, Deps, LazyModels, UpdateDeps};
use broscribe_models::{ExecutionContext, SherpaProvider};
use broscribe_platform::resolve_paths;
use broscribe_platform_native::NativePlatform;

fn main() -> ExitCode {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(e) => {
            broscribe_cli::write_error(
                &mut std::io::stdout().lock(),
                &mut std::io::stderr().lock(),
                std::env::args_os().any(|arg| arg == "--json"),
                &broscribe_core::Error::io("locating executable", e),
            );
            return ExitCode::FAILURE;
        }
    };
    let paths = NativePlatform::paths();
    let data_dir = match resolve_paths(paths.as_ref()) {
        Ok(paths) => paths.data_dir,
        Err(error) => {
            broscribe_cli::write_error(
                &mut std::io::stdout(),
                &mut std::io::stderr(),
                std::env::args_os().any(|arg| arg == "--json"),
                &error,
            );
            return ExitCode::FAILURE;
        }
    };
    // Secrets are scoped to the data folder (its own namespace per profile).
    let secrets = std::sync::Arc::from(NativePlatform::secret_store(&data_dir));
    // Read the model registry only when a command needs models, so a broken
    // `models.toml` does not block the others (`doctor` reports it).
    let models = LazyModels::new(move || SherpaProvider::from_data_dir(&data_dir));
    let deps = Deps {
        paths,
        models: Box::new(models),
        decoder: NativePlatform::decoder_chain(),
        process: NativePlatform::process_probe(),
        mic: NativePlatform::mic_capture(),
        secrets,
        browser: std::sync::Arc::from(NativePlatform::browser()),
        agent_instance: Box::new(broscribe_platform_native::ipc::NativeAgentInstanceProbe),
        autostart: NativePlatform::autostart(executable.clone()),
        agents: AgentEnv::from_process(executable),
        // OS trust is read on the first HTTPS use, never at start-up.
        execution: ExecutionContext::new(
            NativePlatform::process_tree_control(),
            NativePlatform::trust_store(),
        ),
        updates: UpdateDeps::system(NativePlatform::updater(), NativePlatform::trust_store()),
        clock: broscribe_cli::system_clock(),
    };
    // Unlocked handles: `broscribe mcp` writes through tokio's stdout, which
    // takes the std lock per write, so holding it here would deadlock.
    let code = broscribe_cli::run(
        std::env::args_os().collect(),
        &deps,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
