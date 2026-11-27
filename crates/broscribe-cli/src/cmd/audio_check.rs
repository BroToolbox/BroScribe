//! Synthetic speaker-to-microphone check. Never plays through an active recording tap.
use crate::cli::EchoHandlingArg;
use crate::{Ctx, to_value};
use broscribe_core::config::{AudioCheckChoice, Config};
use broscribe_core::{Error, Result};
use broscribe_dsp::{
    AudioCheckThresholds, EchoHandling, TestSignalSpec, analyze_audio_check, test_signal,
};
use broscribe_ipc::Call;
use broscribe_platform::{DeviceDetails, DeviceInfo, MicCapture, MicChunk, PlayerState};
use broscribe_platform_native::NativePlatform;
use serde::Deserialize as _;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(super) fn run(
    ctx: &mut Ctx<'_>,
    save: bool,
    echo_handling: Option<EchoHandlingArg>,
) -> Result<()> {
    run_with(
        ctx,
        save,
        echo_handling,
        |mic| device_pair(NativePlatform::device_info().as_ref(), mic),
        measure,
    )
}

/// The microphone a recording would open for `config`: the pinned one while
/// the platform lists it, the default input (`None`) otherwise. The rule of
/// `broscribe_capture::session::pinned_input`, which the helper's audio
/// check (protocol 3, `device_id` in its result) applies too.
fn capture_mic(mic: &dyn MicCapture, config: &Config) -> Option<String> {
    broscribe_capture::session::pinned_input(mic, config.audio.pinned_mic.as_deref())
        .map(|device| device.id)
}

/// The microphone a check measured: its result's `device_id` (`null`: the
/// default input).
fn measured_mic(report: &Value) -> Result<Option<String>> {
    match report.get("device_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if !id.is_empty() => Ok(Some(id.clone())),
        Some(_) => Err(Error::InvalidInput(
            "the audio check named no valid microphone".into(),
        )),
    }
}

/// `get_pair(mic)` describes the microphone `mic` (`None`: the default
/// input) and the default output; `measure(ctx, mic)` runs the check on
/// `mic` and reports the microphone it measured as `device_id`.
fn run_with(
    ctx: &mut Ctx<'_>,
    save: bool,
    echo_handling: Option<EchoHandlingArg>,
    mut get_pair: impl FnMut(Option<&str>) -> Result<(DeviceDetails, DeviceDetails)>,
    measure: impl FnOnce(&mut Ctx<'_>, Option<&str>) -> Result<Value>,
) -> Result<()> {
    let config = ctx.config()?;
    let mic = capture_mic(ctx.deps.mic.as_ref(), &config);
    let before = if save {
        Some(get_pair(mic.as_deref())?)
    } else {
        None
    };
    let mut report = measure(ctx, mic.as_deref())?;
    let measured = measured_mic(&report)?;
    report["device_id"] = measured.clone().into();
    if let Some(before) = before {
        // The choice is saved for the microphone the check measured, and
        // only when that is the one recording opens here.
        if measured != mic {
            return Err(Error::InvalidInput(
                "the audio check measured another microphone than the one recording uses; run it again".into(),
            ));
        }
        let after = get_pair(measured.as_deref())?;
        if (&after.0.id, &after.1.id, &after.1.output_data_source)
            != (&before.0.id, &before.1.id, &before.1.output_data_source)
        {
            return Err(Error::InvalidInput(
                "the microphone or output changed during the check; run it again".into(),
            ));
        }
        EchoHandling::deserialize(&report["recommendation"]).map_err(|_| {
            Error::InvalidInput(
                "the audio check returned no valid echo-handling recommendation".into(),
            )
        })?;
        let choice = choice_to_save(&report, &after, echo_handling);
        let saved = choice.is_some();
        // Only the data directory's config changes: no library is opened
        // or created.
        Config::update(ctx.data_dir(), move |config| {
            if let Some(choice) = choice {
                config.set_audio_check(choice);
            }
            super::setup::mark(config, "audio_check");
            Ok(())
        })?;
        report["saved"] = saved.into();
        report["setup_completed"] = true.into();
    }
    emit_report(ctx, report)
}

/// The microphone `mic` (`None`: the default input) and the default output.
fn device_pair(
    devices: &dyn DeviceInfo,
    mic: Option<&str>,
) -> Result<(DeviceDetails, DeviceDetails)> {
    let input = match mic {
        Some(id) => devices
            .devices()?
            .into_iter()
            .find(|d| d.id == id && d.input_channels > 0)
            .ok_or_else(|| {
                Error::InvalidInput(format!(
                    "the pinned microphone {id} is not described by the system; run the check again"
                ))
            })?,
        None => devices
            .default_input()?
            .ok_or_else(|| Error::InvalidInput("no default microphone".into()))?,
    };
    let output = devices
        .default_output()?
        .ok_or_else(|| Error::InvalidInput("no default output device".into()))?;
    Ok((input, output))
}

/// The per-pair choice `--save` writes: only a real measurement (delay and
/// leakage both measured) replaces the pair's saved echo handling.
fn choice_to_save(
    report: &Value,
    (input, output): &(DeviceDetails, DeviceDetails),
    echo_handling: Option<EchoHandlingArg>,
) -> Option<AudioCheckChoice> {
    if report["delay_ms"].as_f64().is_none() || report["leakage_db"].as_f64().is_none() {
        return None;
    }
    let handling = match echo_handling {
        Some(handling) => handling.as_str().to_owned(),
        None => {
            let name = report["recommendation"].as_str()?;
            serde_json::from_value::<EchoHandling>(name.into()).ok()?;
            name.to_owned()
        }
    };
    Some(AudioCheckChoice {
        input_uid: input.id.clone(),
        output_uid: output.id.clone(),
        output_data_source: output.output_data_source.clone(),
        echo_handling: handling,
    })
}

/// The helper's check (it opens the microphone recording would), else the
/// same check in this process on `mic`.
fn measure(ctx: &mut Ctx<'_>, mic: Option<&str>) -> Result<Value> {
    if let Some(value) =
        super::daemon::optional_call(ctx.data_dir(), Call::AudioCheck { seconds: 4.0 })?
    {
        return Ok(value);
    }
    let played = test_signal(&TestSignalSpec::new(16_000));
    let temp = tempfile::Builder::new()
        .prefix("broscribe-audio-check-")
        .suffix(".wav")
        .tempfile()
        .map_err(|e| Error::io("creating audio check signal", e))?;
    broscribe_pipeline::worker::write_wav_16k(temp.path(), &played)?;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    let session = ctx.deps.mic.start(
        mic,
        Box::new(move |chunk: MicChunk| {
            let samples = if chunk.sample_rate == 16_000 {
                chunk.samples_mono_f32
            } else {
                super::voice::resample(&chunk.samples_mono_f32, chunk.sample_rate, 16_000)
            };
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(samples);
        }),
    )?;
    let mut player = NativePlatform::audio_player();
    player.play(temp.path(), 0.0, None)?;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match player.poll() {
            PlayerState::Finished { .. } | PlayerState::Stopped => break,
            PlayerState::Failed { message } => {
                player.stop();
                session.stop()?;
                return Err(Error::Decode(message));
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            player.stop();
            session.stop()?;
            return Err(Error::InvalidInput("audio check playback timed out".into()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(500));
    let failure = session.failure();
    session.stop()?;
    if let Some(message) = failure {
        return Err(Error::Decode(message));
    }
    let output = NativePlatform::device_info().default_output()?;
    let raw = captured.lock().unwrap_or_else(|e| e.into_inner());
    if raw.is_empty() {
        return Err(Error::Decode("microphone returned no audio".into()));
    }
    let mut report = analyze_audio_check(
        &played,
        &raw,
        16_000,
        output.as_ref().map(|d| d.nominal_rate),
        &AudioCheckThresholds::default(),
    );
    report.bluetooth_call_profile = output
        .as_ref()
        .is_some_and(|d| d.is_bluetooth_call_profile());
    let mut value = to_value(&report)?;
    value["device_id"] = mic.into();
    Ok(value)
}

fn emit_report(ctx: &mut Ctx<'_>, value: Value) -> Result<()> {
    let delay = value["delay_ms"]
        .as_f64()
        .map(|v| format!("{v:.1} ms"))
        .unwrap_or_else(|| "not measurable".into());
    let leakage = value["leakage_db"]
        .as_f64()
        .map(|v| format!("{v:.1} dB"))
        .unwrap_or_else(|| "not measurable".into());
    let recommendation = value["recommendation"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    let bluetooth = value["bluetooth_call_profile"].as_bool().unwrap_or(false);
    let saved = value["saved"].as_bool();
    let measured = value["delay_ms"].as_f64().is_some() && value["leakage_db"].as_f64().is_some();
    let setup_completed = value["setup_completed"].as_bool() == Some(true);
    let microphone = match value["device_id"].as_str() {
        Some(id) => format!("pinned microphone {id}"),
        None => "default input".to_owned(),
    };
    ctx.emit(value, |out| {
        writeln!(out, "Microphone: {microphone}")?;
        writeln!(out, "Delay: {delay}")?;
        writeln!(out, "Leakage: {leakage}")?;
        if measured {
            writeln!(out, "Recommendation: {}", recommendation.replace('_', " "))?;
        } else {
            writeln!(
                out,
                "The check ran, but the echo path was not measurable. Headphones, built-in \
                 echo cancellation, an inaudible test sound or a muted microphone can cause this."
            )?;
        }
        if bluetooth {
            writeln!(
                out,
                "Warning: Bluetooth call profile is active; output bandwidth is reduced."
            )?;
        }
        match saved {
            Some(true) => writeln!(out, "Saved for this microphone and output.")?,
            Some(false) => writeln!(
                out,
                "No calibration or requested override was saved; these devices keep their \
                 saved echo handling or setup default."
            )?,
            None => {}
        }
        if setup_completed {
            writeln!(
                out,
                "Setup step marked completed; this is not a calibration pass."
            )?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::{DeviceTransport, devices::FakeDeviceInfo};
    use serde_json::json;

    fn pair() -> (DeviceDetails, DeviceDetails) {
        let mut output = FakeDeviceInfo::device("out", DeviceTransport::BuiltIn, false, true);
        output.output_data_source = Some("Speakers".into());
        (
            FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false),
            output,
        )
    }

    fn report(delay: Value, leakage: Value) -> Value {
        serde_json::json!({
            "delay_ms": delay, "leakage_db": leakage, "correlation_peak": 0.0,
            "noise_floor_dbfs": -70.0, "recommendation": "none",
            "bluetooth_call_profile": false
        })
    }

    #[test]
    fn unmeasured_audio_check_saves_nothing() {
        for (delay, leakage) in [
            (Value::Null, Value::Null),
            (12.5.into(), Value::Null),
            (Value::Null, (-30.0).into()),
        ] {
            let report = report(delay, leakage);
            assert!(choice_to_save(&report, &pair(), None).is_none());
            assert!(choice_to_save(&report, &pair(), Some(EchoHandlingArg::FullAec)).is_none());
        }
    }

    #[test]
    fn measured_audio_check_saves_the_pair_choice() {
        let measured = report(12.5.into(), (-30.0).into());
        let choice = choice_to_save(&measured, &pair(), None).unwrap();
        assert_eq!(
            (
                choice.input_uid.as_str(),
                choice.output_uid.as_str(),
                choice.output_data_source.as_deref(),
                choice.echo_handling.as_str()
            ),
            ("mic", "out", Some("Speakers"), "none")
        );
        let chosen = choice_to_save(&measured, &pair(), Some(EchoHandlingArg::ResidueGating));
        assert_eq!(chosen.unwrap().echo_handling, "residue_gating");
        // An unknown recommendation is never saved.
        let mut bogus = measured;
        bogus["recommendation"] = "bogus".into();
        assert!(choice_to_save(&bogus, &pair(), None).is_none());
    }

    fn invoke(
        root: &std::path::Path,
        save: bool,
        handling: Option<EchoHandlingArg>,
        get_pair: impl FnMut(Option<&str>) -> Result<(DeviceDetails, DeviceDetails)>,
        measurement: Result<Value>,
    ) -> (Result<()>, String) {
        invoke_with(
            root,
            broscribe_platform::fakes::FakeMicCapture::new(16_000),
            save,
            handling,
            get_pair,
            move |_| measurement,
        )
    }

    fn invoke_with(
        root: &std::path::Path,
        mic: broscribe_platform::fakes::FakeMicCapture,
        save: bool,
        handling: Option<EchoHandlingArg>,
        get_pair: impl FnMut(Option<&str>) -> Result<(DeviceDetails, DeviceDetails)>,
        measure: impl FnOnce(Option<&str>) -> Result<Value>,
    ) -> (Result<()>, String) {
        use broscribe_platform::fakes::{
            FakeAutostart, FakeBrowserLauncher, FakePaths, FakeProcessProbe,
            FakeProcessTreeControl, FakeSecretStore, FakeTrustStore,
        };
        let deps = crate::Deps {
            clock: crate::system_clock(),
            paths: Box::new(FakePaths::new(root)),
            updates: crate::UpdateDeps::with_transport(
                std::sync::Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                    std::path::Path::new(&root).join("install"),
                    "synthetic-test-target",
                )),
                std::sync::Arc::new(broscribe_updates::FakeTransport::default()),
            ),
            models: Box::new(broscribe_models::fakes::FakeModelProvider::default()),
            decoder: broscribe_platform::DecoderChain::new(vec![Box::new(
                broscribe_platform::symphonia_decoder::SymphoniaDecoder::new(),
            )]),
            process: Box::new(FakeProcessProbe::new(std::process::id())),
            mic: Box::new(mic),
            secrets: Arc::new(FakeSecretStore::default()),
            browser: Arc::new(FakeBrowserLauncher::default()),
            agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
            autostart: Box::new(FakeAutostart::default()),
            agents: crate::AgentEnv::isolated(root),
            // The audio check starts no model process and makes no request.
            execution: broscribe_models::ExecutionContext::new(
                Arc::new(FakeProcessTreeControl::default()),
                Arc::new(FakeTrustStore::default()),
            ),
        };
        let mut paths = broscribe_platform::resolve_paths(deps.paths.as_ref()).unwrap();
        paths.data_dir = root.join("data");
        paths.cache_dir = root.join("cache");
        paths.library_from_env = None;
        paths.default_library_dir = root.join("library");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = Ctx::scoped(
            &deps,
            true,
            Some(root.join("library")),
            paths,
            None,
            &mut out,
            &mut err,
            |ctx| run_with(ctx, save, handling, get_pair, |_, mic| measure(mic)),
        );
        (result, String::from_utf8(out).unwrap())
    }

    fn seed(root: &std::path::Path) -> broscribe_core::config::Config {
        let mut config = broscribe_core::config::Config::default();
        config.set_audio_check(AudioCheckChoice {
            input_uid: "mic".into(),
            output_uid: "out".into(),
            output_data_source: Some("Speakers".into()),
            echo_handling: "full_aec".into(),
        });
        config.save(&root.join("data")).unwrap();
        config
    }

    #[test]
    fn successful_unmeasured_check_completes_without_replacing_calibration() {
        for (delay, leakage) in [
            (Value::Null, Value::Null),
            (12.5.into(), Value::Null),
            (Value::Null, (-30.0).into()),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let previous = seed(dir.path());
            let value = report(delay, leakage);
            let (result, out) = invoke(
                dir.path(),
                true,
                Some(EchoHandlingArg::None),
                |_| Ok(pair()),
                Ok(value.clone()),
            );
            result.unwrap();
            let emitted: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(emitted["saved"], false);
            assert_eq!(emitted["setup_completed"], true);
            let config = broscribe_core::config::Config::load(&dir.path().join("data")).unwrap();
            assert_eq!(config.audio_checks, previous.audio_checks);
            assert_eq!(config.setup.completed_steps, ["audio_check"]);
            let bytes = std::fs::read(dir.path().join("data/config.toml")).unwrap();
            invoke(dir.path(), true, None, |_| Ok(pair()), Ok(value))
                .0
                .unwrap();
            assert_eq!(
                std::fs::read(dir.path().join("data/config.toml")).unwrap(),
                bytes
            );
        }
    }

    #[test]
    fn measured_check_saves_explicit_choice_and_completion() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let (result, out) = invoke(
            dir.path(),
            true,
            Some(EchoHandlingArg::ResidueGating),
            |_| Ok(pair()),
            Ok(report(12.5.into(), (-30.0).into())),
        );
        result.unwrap();
        let emitted: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(emitted["saved"], true);
        assert_eq!(emitted["setup_completed"], true);
        let config = broscribe_core::config::Config::load(&dir.path().join("data")).unwrap();
        assert_eq!(config.audio_checks.len(), 1);
        assert_eq!(config.audio_checks[0].echo_handling, "residue_gating");
        assert_eq!(config.setup.completed_steps, ["audio_check"]);
        assert!(
            !dir.path().join("library").exists(),
            "saving the check created the library"
        );
    }

    #[test]
    fn diagnostic_check_without_save_keeps_configuration_read_only() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let path = dir.path().join("data/config.toml");
        let bytes = std::fs::read(&path).unwrap();
        let (result, out) = invoke(
            dir.path(),
            false,
            None,
            |_| panic!("diagnostic must not request a save pair"),
            Ok(report(Value::Null, Value::Null)),
        );
        result.unwrap();
        let emitted: Value = serde_json::from_str(&out).unwrap();
        assert!(emitted.get("saved").is_none());
        assert!(emitted.get("setup_completed").is_none());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert!(!dir.path().join("library").exists());
    }

    #[test]
    fn malformed_or_failed_check_does_not_complete_or_create_library() {
        for handling in [None, Some(EchoHandlingArg::FullAec)] {
            for recommendation in [Some("bogus"), None] {
                let dir = tempfile::tempdir().unwrap();
                seed(dir.path());
                let path = dir.path().join("data/config.toml");
                let bytes = std::fs::read(&path).unwrap();
                let mut value = report(12.5.into(), (-30.0).into());
                if let Some(name) = recommendation {
                    value["recommendation"] = name.into();
                } else {
                    value.as_object_mut().unwrap().remove("recommendation");
                }
                assert!(
                    invoke(dir.path(), true, handling, |_| Ok(pair()), Ok(value))
                        .0
                        .is_err()
                );
                assert_eq!(std::fs::read(path).unwrap(), bytes);
                assert!(!dir.path().join("library").exists());
            }
        }
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let bytes = std::fs::read(dir.path().join("data/config.toml")).unwrap();
        let mut calls = 0;
        let changing_pair = |_: Option<&str>| {
            calls += 1;
            let mut pair = pair();
            if calls > 1 {
                pair.1.output_data_source = Some("Headphones".into());
            }
            Ok(pair)
        };
        assert!(
            invoke(
                dir.path(),
                true,
                None,
                changing_pair,
                Ok(report(12.5.into(), (-30.0).into())),
            )
            .0
            .is_err()
        );
        assert!(
            invoke(
                dir.path(),
                true,
                None,
                |_| Ok(pair()),
                Err(Error::Inference("synthetic playback failure".into())),
            )
            .0
            .is_err()
        );
        assert_eq!(
            std::fs::read(dir.path().join("data/config.toml")).unwrap(),
            bytes
        );
        assert!(!dir.path().join("library").exists());
    }

    #[test]
    fn failed_progress_write_is_not_reported_as_complete() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data/config.toml")).unwrap();
        let (result, out) = invoke(
            dir.path(),
            true,
            None,
            |_| Ok(pair()),
            Ok(report(Value::Null, Value::Null)),
        );
        assert!(result.is_err());
        assert!(out.is_empty());
        assert!(!dir.path().join("library").exists());
    }

    /// The system's devices: default input `mic`, a connected `usb-mic` and
    /// the default output `out` (on its speakers).
    fn system() -> FakeDeviceInfo {
        let (mic, out) = pair();
        let mut usb = FakeDeviceInfo::device("usb-mic", DeviceTransport::Usb, true, false);
        usb.is_default_input = false;
        FakeDeviceInfo::new(vec![mic, usb, out])
    }

    /// A microphone capture listing the default `fake-mic` and `usb-mic`.
    fn mic_with_usb() -> broscribe_platform::fakes::FakeMicCapture {
        let mut mic = broscribe_platform::fakes::FakeMicCapture::new(16_000);
        mic.devices.push(broscribe_platform::AudioDevice {
            id: "usb-mic".into(),
            name: "USB microphone".into(),
            is_default: false,
            sample_rates: vec![16_000],
        });
        mic
    }

    fn pin(root: &std::path::Path, device: &str) {
        let data = root.join("data");
        let mut config = broscribe_core::config::Config::load(&data).unwrap();
        config.audio.pinned_mic = Some(device.into());
        config.save(&data).unwrap();
    }

    #[test]
    fn a_connected_pinned_microphone_is_measured_and_saved_as_the_pair() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        pin(dir.path(), "usb-mic");
        let system = system();
        let (result, out) = invoke_with(
            dir.path(),
            mic_with_usb(),
            true,
            None,
            |mic| device_pair(&system, mic),
            |mic| {
                assert_eq!(mic, Some("usb-mic"));
                let mut value = report(12.5.into(), (-30.0).into());
                value["device_id"] = "usb-mic".into();
                Ok(value)
            },
        );
        result.unwrap();
        let emitted: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            (&emitted["saved"], &emitted["device_id"]),
            (&json!(true), &json!("usb-mic"))
        );
        let config = broscribe_core::config::Config::load(&dir.path().join("data")).unwrap();
        let saved: Vec<&str> = config
            .audio_checks
            .iter()
            .map(|c| c.input_uid.as_str())
            .collect();
        // The default microphone's earlier choice is kept beside the pin's.
        assert_eq!(saved.len(), 2);
        assert!(saved.contains(&"usb-mic") && saved.contains(&"mic"));
    }

    #[test]
    fn a_check_of_another_microphone_than_the_pin_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        pin(dir.path(), "usb-mic");
        let path = dir.path().join("data/config.toml");
        let bytes = std::fs::read(&path).unwrap();
        let system = system();
        // The helper measured the default input although the pin is listed.
        let (result, _) = invoke_with(
            dir.path(),
            mic_with_usb(),
            true,
            None,
            |mic| device_pair(&system, mic),
            |_| Ok(report(12.5.into(), (-30.0).into())),
        );
        assert_eq!(result.unwrap_err().code(), "invalid_input");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(!dir.path().join("library").exists());
    }

    #[test]
    fn an_unplugged_pin_checks_and_saves_the_default_input() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        pin(dir.path(), "gone-mic");
        let system = system();
        let (result, out) = invoke_with(
            dir.path(),
            mic_with_usb(),
            true,
            Some(EchoHandlingArg::ResidueGating),
            |mic| device_pair(&system, mic),
            |mic| {
                assert_eq!(mic, None);
                Ok(report(12.5.into(), (-30.0).into()))
            },
        );
        result.unwrap();
        let emitted: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(emitted["device_id"], Value::Null);
        let config = broscribe_core::config::Config::load(&dir.path().join("data")).unwrap();
        assert_eq!(config.audio_checks.len(), 1);
        assert_eq!(
            (
                config.audio_checks[0].input_uid.as_str(),
                config.audio_checks[0].echo_handling.as_str()
            ),
            ("mic", "residue_gating")
        );
    }
}
