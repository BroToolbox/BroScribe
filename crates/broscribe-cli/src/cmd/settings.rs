use broscribe_core::Result;
use broscribe_core::config::{AutoEnrich, Config, ReadingSize, SpeakerGrouping};
use broscribe_core::schema::Handling;
use serde_json::{Value, json};

use crate::Ctx;
use crate::cli::{HandlingArg, OnOff, SettingsCmd, SpeakerGroupingCmd, TextSizeArg};

pub(super) fn run(ctx: &mut Ctx<'_>, action: SettingsCmd) -> Result<()> {
    match action {
        SettingsCmd::TextSize { value } => {
            let config = if let Some(value) = value {
                Config::update(ctx.data_dir(), |config| {
                    ctx.check_request_cancelled()?;
                    config.appearance.reading_size = match value {
                        TextSizeArg::Small => ReadingSize::Small,
                        TextSizeArg::Standard => ReadingSize::Standard,
                        TextSizeArg::Large => ReadingSize::Large,
                        TextSizeArg::Larger => ReadingSize::Larger,
                        TextSizeArg::Largest => ReadingSize::Largest,
                    };
                    Ok(())
                })?
                .0
            } else {
                ctx.config()?
            };
            let reading_size = config.appearance.reading_size;
            let scale = reading_size.scale();
            ctx.emit(
                json!({"reading_size": reading_size, "scale": scale}),
                |out| writeln!(out, "Text size: {reading_size:?} ({}%)", scale * 100.),
            )
        }
        SettingsCmd::Theme { value } => {
            let config = if let Some(value) = value {
                Config::update(ctx.data_dir(), |config| {
                    ctx.check_request_cancelled()?;
                    config.appearance.theme = value.into();
                    Ok(())
                })?
                .0
            } else {
                ctx.config()?
            };
            let theme = config.appearance.theme.name();
            ctx.emit(json!({ "theme": theme }), |out| {
                writeln!(out, "Appearance: {theme}")
            })
        }
        SettingsCmd::SpeakerGrouping { preset } => speaker_grouping(ctx, preset),
        SettingsCmd::LibraryFolder { path } => {
            let path = if let Some(path) = path {
                if ctx.paths.library_from_env.is_some() {
                    return Err(broscribe_core::Error::InvalidInput(
                        "Remove BROSCRIBE_LIBRARY before choosing a different folder.".into(),
                    ));
                }
                let path = std::path::absolute(path)
                    .map_err(|e| broscribe_core::Error::io("resolving the library folder", e))?;
                let library = broscribe_core::Library::open(path)?;
                let path = library.root().to_owned();
                Config::update(ctx.data_dir(), |config| {
                    ctx.check_request_cancelled()?;
                    config.library = Some(path.clone());
                    Ok(())
                })?;
                path
            } else {
                ctx.library_path(&ctx.config()?).0
            };
            ctx.emit(json!({"library_folder": path}), |out| {
                writeln!(out, "Library folder: {}", path.display())
            })
        }
        SettingsCmd::DefaultLanguage { value } => {
            let config = if let Some(value) = value {
                let value = value.trim();
                let language = if value.eq_ignore_ascii_case("auto") {
                    None
                } else if broscribe_core::config::valid_language_tag(value) {
                    Some(value.to_owned())
                } else {
                    return Err(broscribe_core::Error::InvalidInput(
                        "Use a language such as en or pt-BR, or auto.".into(),
                    ));
                };
                Config::update(ctx.data_dir(), |config| {
                    ctx.check_request_cancelled()?;
                    config.default_language = language;
                    Ok(())
                })?
                .0
            } else {
                ctx.config()?
            };
            ctx.emit(
                json!({"default_language": config.default_language}),
                |out| {
                    writeln!(
                        out,
                        "Default language: {}",
                        config.default_language.as_deref().unwrap_or("Automatic")
                    )
                },
            )
        }
        SettingsCmd::WordList => {
            let config = ctx.config()?;
            ctx.emit(json!({"words": config.hotwords}), |out| {
                for word in &config.hotwords {
                    writeln!(out, "{word}")?;
                }
                Ok(())
            })
        }
        SettingsCmd::Window => {
            let state: Value = std::fs::read(ctx.data_dir().join("ui-state.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
            let pane = state["settings_page"].as_str().unwrap_or("general");
            let position = &state["settings_window"];
            let mut output = json!({"pane": pane, "window": position});
            if state["main_window"].is_object() {
                output["main_window"] = state["main_window"].clone();
            }
            ctx.emit(output, |out| writeln!(out, "Last Settings pane: {pane}"))
        }
        SettingsCmd::ImportHandling { value } => {
            let config = if let Some(value) = value {
                Config::update(ctx.data_dir(), |config| {
                    ctx.check_request_cancelled()?;
                    config.import_handling = match value {
                        HandlingArg::Copy => Handling::Copy,
                        HandlingArg::Reference => Handling::Reference,
                        HandlingArg::Move => Handling::Move,
                    };
                    Ok(())
                })?
                .0
            } else {
                ctx.config()?
            };
            let text = match config.import_handling {
                Handling::Copy => "Imported recordings are copied into the library.",
                Handling::Reference => "Imported recordings stay in place.",
                Handling::Move => "Imported recordings move into the library.",
            };
            ctx.emit(json!({"import_handling": config.import_handling}), |out| {
                writeln!(out, "{text}")
            })
        }
        SettingsCmd::AutomaticSummaries { value } => {
            let config = if let Some(value) = value {
                Config::update(ctx.data_dir(), |config| {
                    ctx.check_request_cancelled()?;
                    config.auto_enrich = match value {
                        OnOff::On => AutoEnrich::New,
                        OnOff::Off => AutoEnrich::Off,
                    };
                    Ok(())
                })?
                .0
            } else {
                ctx.config()?
            };
            let on = config.auto_enrich == AutoEnrich::New;
            ctx.emit(json!({"automatic_summaries": on}), |out| {
                writeln!(
                    out,
                    "Automatic summaries of single imports are {}.",
                    if on { "on" } else { "off" }
                )
            })
        }
        SettingsCmd::ConsentReminder { value } => {
            let config = if let Some(value) = value {
                Config::update(ctx.data_dir(), |config| {
                    ctx.check_request_cancelled()?;
                    config.consent_reminder = matches!(value, OnOff::On);
                    Ok(())
                })?
                .0
            } else {
                ctx.config()?
            };
            let state = if config.consent_reminder { "on" } else { "off" };
            ctx.emit(
                json!({"consent_reminder": config.consent_reminder}),
                |out| writeln!(out, "The consent reminder is {state}."),
            )
        }
    }
}

fn speaker_grouping(ctx: &mut Ctx<'_>, preset: Option<SpeakerGroupingCmd>) -> Result<()> {
    let config = if let Some(preset) = preset {
        let grouping = match preset {
            SpeakerGroupingCmd::Standard => SpeakerGrouping::STANDARD,
            SpeakerGroupingCmd::Conservative => SpeakerGrouping::CONSERVATIVE,
            SpeakerGroupingCmd::Custom {
                suggest_at,
                auto_merge_at,
            } => SpeakerGrouping::custom(suggest_at, auto_merge_at)?,
        };
        Config::update(ctx.data_dir(), |config| {
            ctx.check_request_cancelled()?;
            let model = config.profile.speaker_embedding.clone();
            config.set_speaker_grouping(&model, grouping);
            Ok(())
        })?
        .0
    } else {
        ctx.config()?
    };
    let model = &config.profile.speaker_embedding;
    let grouping = config.speaker_grouping(model);
    let suggest = grouping.merge.map(|v| v * 100.0);
    let auto_merge = grouping.auto_merge.map(|v| v * 100.0);
    let display =
        |percent: Option<f32>| percent.map_or_else(|| "off".to_owned(), |v| format!("{v}%"));
    ctx.emit(
        json!({"model": model, "preset": grouping.preset, "suggest_at": suggest, "auto_merge_at": auto_merge}),
        |out| {
            writeln!(out, "Speaker grouping ({model}): {:?}", grouping.preset)?;
            writeln!(out, "Suggest at: {}", display(suggest))?;
            writeln!(out, "Merge automatically at: {}", display(auto_merge))
        },
    )
}

pub(super) fn disk_usage(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    let (path, _) = ctx.library_path(&config);
    let library_bytes = broscribe_core::fsutil::directory_bytes(&path)
        .map_err(|e| broscribe_core::Error::io_at(&path, e))?;
    let free_bytes =
        broscribe_platform_native::NativePlatform::system_info().free_disk_bytes(&path)?;
    ctx.emit(
        json!({"library": path, "library_bytes": library_bytes, "free_bytes": free_bytes}),
        |out| {
            writeln!(
                out,
                "{} bytes in the library, {} bytes free on this disk.",
                library_bytes, free_bytes
            )
        },
    )
}

/// Change the configured folder only after the running app and helper have
/// drained. The helper's connection-owned admission fences capture starts.
pub(super) fn switch_library(
    ctx: &mut Ctx<'_>,
    path: &std::path::Path,
    timeout: u64,
) -> Result<()> {
    use broscribe_core::{
        Error, Library,
        lock::{AppInstanceLock, DaemonStartupLock},
        queue::Queue,
    };
    use broscribe_ipc::{Call, IpcError};
    use broscribe_platform::shell::{HANDOFF_DONE, HandoffRequest};
    use std::time::{Duration, Instant};

    if ctx.paths.library_from_env.is_some() || ctx.library_flag.is_some() {
        return Err(Error::InvalidInput(
            "remove the library override before switching libraries".into(),
        ));
    }
    let config = ctx.config()?;
    let (old, _) = ctx.configured_library_path(&config);
    if ctx
        .request_library
        .as_ref()
        .is_some_and(|root| root != &old)
    {
        return Err(Error::InvalidInput(
            "reconnect MCP before switching libraries again".into(),
        ));
    }
    let data = ctx.data_dir();
    let channel = broscribe_platform_native::NativePlatform::instance_channel(data);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let interval = Duration::from_millis(250);
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let wait_error = || {
        Error::LibraryBusy("the app or agent did not become idle before the switch timeout".into())
    };
    let (mut held, had_app, startup) = loop {
        ctx.check_request_cancelled()?;
        if remaining().is_zero() {
            return Err(wait_error());
        }
        let app_active = AppInstanceLock::try_acquire(data)?.is_none();
        if app_active {
            match channel
                .request(
                    HandoffRequest::LibrarySwitchStatus,
                    remaining().min(Duration::from_secs(2)),
                )?
                .as_str()
            {
                "ready" => {}
                "busy" => {
                    std::thread::sleep(interval);
                    continue;
                }
                _ => {
                    return Err(Error::Unsupported(
                        "the running app does not support library switching from the CLI".into(),
                    ));
                }
            }
        }
        let mut held = super::daemon::connect(data, remaining().min(Duration::from_secs(2)))?;
        let mut startup = None;
        if let Some(client) = &mut held {
            let state = client
                .call(Call::Status)
                .map_err(super::daemon::call_error)?;
            if library_switch_busy(&state)
                || ctx.deps.autostart.agent_status()?
                    == broscribe_platform::AgentServiceStatus::NotInstalled
            {
                if !library_switch_busy(&state) {
                    return Err(Error::Unsupported(
                        "install BroScribe Agent before switching a manually launched agent".into(),
                    ));
                }
                std::thread::sleep(interval);
                continue;
            }
            let agent_library = state["library"]
                .as_str()
                .ok_or_else(|| Error::Internal("agent did not report its library".into()))?;
            if std::path::Path::new(agent_library).canonicalize().ok() != old.canonicalize().ok() {
                return Err(Error::InvalidInput(
                    "the running agent uses a different library than this profile".into(),
                ));
            }
            match client.call(Call::LibrarySwitchAcquire) {
                Ok(_) => {}
                Err(broscribe_platform_native::ipc::CallError::Remote(e))
                    if matches!(e.code.as_str(), "recording_active" | "busy" | "installing") =>
                {
                    std::thread::sleep(interval);
                    continue;
                }
                Err(e) => return Err(super::daemon::call_error(e)),
            }
            let after = client
                .call(Call::Status)
                .map_err(super::daemon::call_error)?;
            if jobs_busy(&after) {
                drop(held);
                std::thread::sleep(interval);
                continue;
            }
        } else {
            startup = Some(loop {
                ctx.check_request_cancelled()?;
                if remaining().is_zero() {
                    return Err(Error::Service {
                        code: IpcError::BUSY,
                        message: "timed out waiting for the profile's agent startup lock".into(),
                    });
                }
                if let Some(guard) = DaemonStartupLock::try_acquire(data)? {
                    break guard;
                }
                std::thread::sleep(interval.min(remaining()));
            });
            if super::daemon::agent_instance_held(data, ctx.deps.agent_instance.as_ref())? {
                return Err(Error::Service {
                    code: "agent_unreachable",
                    message: "an agent holds this profile's instance lock without a reachable endpoint; the library folder was not changed".into(),
                });
            }
        }
        // Also include jobs leased by a standalone worker.
        if let Some(queue) = Queue::open_existing(&old)?
            && queue
                .snapshot()?
                .jobs
                .iter()
                .any(|job| queue.is_held(job, ctx.deps.process.as_ref()))
        {
            drop(held);
            std::thread::sleep(interval);
            continue;
        }
        break (held, app_active, startup);
    };
    let new = Library::open(std::path::absolute(path).map_err(|e| Error::io_at(path, e))?)?;
    let new_path = new
        .root()
        .canonicalize()
        .map_err(|e| Error::io_at(new.root(), e))?;
    Config::update(data, |fresh| {
        ctx.check_request_cancelled()?;
        if fresh.library != config.library {
            return Err(Error::LibraryBusy(
                "the saved library changed while waiting".into(),
            ));
        }
        fresh.library = Some(new_path.clone());
        Ok(())
    })?;
    drop(startup);

    let had_agent = held.is_some();
    if let Some(client) = &mut held {
        let pid = super::daemon::ping(client)?.pid;
        client
            .call(Call::Shutdown)
            .map_err(super::daemon::call_error)?;
        drop(held);
        if !super::daemon::wait_exit(ctx.deps.process.as_ref(), pid, remaining()) {
            return Err(Error::LibraryBusy(
                "library saved, but the previous agent did not stop before the timeout".into(),
            ));
        }
        if super::daemon::connect(data, Duration::from_secs(1))?.is_none() {
            ctx.deps.autostart.start()?;
        }
        while !super::daemon::optional_call(data, Call::Status)?
            .and_then(|state| state["library"].as_str().map(std::path::PathBuf::from))
            .and_then(|path| path.canonicalize().ok())
            .is_some_and(|path| path == new_path)
        {
            if remaining().is_zero() {
                return Err(Error::LibraryBusy(
                    "library saved, but the agent did not open it before the timeout".into(),
                ));
            }
            std::thread::sleep(interval);
        }
    }
    if had_app {
        loop {
            if remaining().is_zero() {
                return Err(Error::LibraryBusy(
                    "library saved, but the app did not finish switching before the timeout".into(),
                ));
            }
            match channel
                .request(
                    HandoffRequest::ReloadLibrary,
                    remaining().min(Duration::from_secs(2)),
                )?
                .as_str()
            {
                HANDOFF_DONE => break,
                "busy" => std::thread::sleep(interval),
                _ => {
                    return Err(Error::Unsupported(
                        "library saved, but the app refused to switch".into(),
                    ));
                }
            }
        }
        loop {
            if remaining().is_zero() {
                return Err(Error::LibraryBusy(
                    "library saved, but the app did not reopen it before the timeout".into(),
                ));
            }
            if channel
                .request(
                    HandoffRequest::LibraryApplied,
                    remaining().min(Duration::from_secs(1)),
                )
                .is_ok_and(|answer| answer == "applied")
            {
                break;
            }
            std::thread::sleep(interval);
        }
    }
    ctx.emit(
        json!({"library_folder": new_path, "agent_switched": had_agent, "app_switched": had_app}),
        |out| writeln!(out, "Library folder: {}", new_path.display()),
    )
}

fn jobs_busy(state: &serde_json::Value) -> bool {
    !state["jobs"]["running"].is_null()
        || state["jobs"]["waiting"]
            .as_u64()
            .is_some_and(|waiting| waiting > 0)
        || state["jobs"]["deferred"]
            .as_u64()
            .is_some_and(|deferred| deferred > 0)
}

fn library_switch_busy(state: &serde_json::Value) -> bool {
    !state["recording"].is_null() || !state["install"].is_null() || jobs_busy(state)
}
