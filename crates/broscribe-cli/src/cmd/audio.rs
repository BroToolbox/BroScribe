//! `audio pin`: the microphone recordings use while it is connected.

use broscribe_core::config::Config;
use broscribe_core::{Error, Result};
use serde_json::json;

use crate::Ctx;

/// Pins `device` (an `audio devices` ID), or unpins with `None`, in the data
/// directory's config only: no library is opened or created.
pub(super) fn pin(ctx: &mut Ctx<'_>, device: Option<String>) -> Result<()> {
    if let Some(id) = &device
        && !ctx.deps.mic.devices()?.iter().any(|d| &d.id == id)
    {
        return Err(Error::NotFound(format!(
            "audio input device {id}; see `broscribe audio devices`"
        )));
    }
    Config::update(ctx.data_dir(), |c| {
        ctx.check_request_cancelled()?;
        c.audio.pinned_mic.clone_from(&device);
        Ok(())
    })?;
    ctx.emit(json!({ "pinned_mic": device }), |out| match &device {
        Some(id) => writeln!(out, "Recording with {id} while it is connected."),
        None => writeln!(out, "Recording with the system default microphone."),
    })
}

pub(super) fn pin_status(ctx: &mut Ctx<'_>) -> Result<()> {
    let pin = ctx.config()?.audio.pinned_mic;
    let device = broscribe_capture::session::pinned_input(ctx.deps.mic.as_ref(), pin.as_deref());
    let name = device.as_ref().map(|d| d.name.as_str());
    let connected = pin.as_ref().map(|_| device.is_some());
    ctx.emit(
        json!({"pinned_mic": pin, "connected": connected, "name": name}),
        |out| match (&pin, name) {
            (None, _) => writeln!(out, "Recording with the system default microphone."),
            (Some(id), Some(name)) => writeln!(out, "{name} ({id}) is pinned and connected."),
            (Some(id), None) => writeln!(out, "{id} is pinned and not connected."),
        },
    )
}

pub(super) fn check_status(ctx: &mut Ctx<'_>) -> Result<()> {
    let checks = ctx.config()?.audio_checks;
    let last = checks.last();
    let devices = broscribe_platform_native::NativePlatform::device_info()
        .devices()
        .unwrap_or_default();
    let device_name = |uid: &str| devices.iter().find(|d| d.id == uid).map(|d| d.name.clone());
    let mic = last.and_then(|c| device_name(&c.input_uid));
    let output = last.and_then(|c| {
        c.output_data_source
            .clone()
            .or_else(|| device_name(&c.output_uid))
    });
    ctx.emit(
        json!({"checks": checks, "last": last, "microphone_name": mic, "output_name": output}),
        |out| match last {
            None => writeln!(out, "Audio check has not run."),
            Some(c) => writeln!(
                out,
                "Measured with {} and {} ({})",
                mic.as_ref().unwrap_or(&c.input_uid),
                output.as_ref().unwrap_or(&c.output_uid),
                c.echo_handling
            ),
        },
    )
}
