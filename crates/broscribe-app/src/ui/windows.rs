//! Window sizes (SHL-11) and the app's presence in the Dock and the app
//! switcher (SHL-15).
//!
//! Every window opens at a default size and can't be made smaller than its
//! minimum. The main window's default is 1120×720, or 90 % of the primary
//! display's visible area when that is smaller.
//!
//! An accessory launch (the agent's, for notifications) runs without a Dock
//! icon, a ⌘-Tab entry or menu-bar menus. [`WindowPresence`] counts the
//! regular windows (main and About; Settings and setup join when they
//! become windows): a deliberate open of the first one switches the
//! activation policy to Regular before it activates the app
//! ([`activate_for_regular_window`]), and closing the last one returns an
//! accessory launch to Accessory. The recording controls never change the
//! policy, and nothing here runs for an automatic presentation. The policy
//! only has an effect on macOS (GPUI ignores it elsewhere).

use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use broscribe_ipc::{Call, IpcError};
use broscribe_platform::{ActivationPolicy, AppActivation, HostPlatform};
use futures::future::{Either, select};
use gpui::{App, Global, Pixels, Size, Task, WindowId, px, size};

/// The main window's default size, before the display caps it.
pub const MAIN_DEFAULT: (f32, f32) = (1120., 720.);
/// The share of the primary display's visible area the main window's
/// default never exceeds.
pub const MAIN_DISPLAY_SHARE: f32 = 0.9;
pub const MAIN_MIN: (f32, f32) = (600., 420.);
pub const ABOUT_SIZE: (f32, f32) = (520., 560.);
pub const ABOUT_MIN: (f32, f32) = (440., 480.);
/// The startup error and maintenance windows.
pub const STARTUP_SIZE: (f32, f32) = (560., 320.);
pub const STARTUP_MIN: (f32, f32) = (480., 280.);
/// The "BroScribe is already running" notice (SHL-14).
pub const NOTICE_SIZE: (f32, f32) = (480., 240.);

pub fn px_size((width, height): (f32, f32)) -> Size<Pixels> {
    size(px(width), px(height))
}

pub(crate) const STARTUP_TITLE: &str = "BroScribe can't start";

pub(crate) fn title_for(host: HostPlatform, location: &str) -> String {
    match host {
        HostPlatform::MacOs => location.into(),
        _ if location == STARTUP_TITLE => location.into(),
        _ => format!("{location} - BroScribe"),
    }
}

pub(crate) fn title(location: &str) -> String {
    title_for(
        broscribe_platform_native::NativePlatform::host_platform(),
        location,
    )
}

pub(crate) fn maintenance_title(host: HostPlatform) -> &'static str {
    match host {
        HostPlatform::MacOs => "Maintenance Mode",
        _ => "Maintenance mode",
    }
}

pub fn confirm_recording_quit(
    main: gpui::WindowHandle<gpui::component::Root>,
    recording: broscribe_ipc::RecordingState,
    daemon: gpui::Entity<crate::daemon::DaemonClient>,
    pending: Rc<std::cell::Cell<bool>>,
    quit: impl FnOnce(&mut App) + 'static,
    cx: &mut App,
) {
    confirm_recording_quit_with(
        main,
        recording,
        pending,
        quit,
        move |session_id, cx| daemon.read(cx).request(Call::RecordStop { session_id }, cx),
        cx,
    );
}

const QUIT_STOP_TIMEOUT: Duration = Duration::from_secs(3);

fn confirm_recording_quit_with(
    main: gpui::WindowHandle<gpui::component::Root>,
    recording: broscribe_ipc::RecordingState,
    pending: Rc<std::cell::Cell<bool>>,
    quit: impl FnOnce(&mut App) + 'static,
    stop: impl FnOnce(String, &App) -> Task<Result<serde_json::Value, IpcError>> + 'static,
    cx: &mut App,
) {
    if pending.replace(true) {
        return;
    }
    cx.defer(move |cx| {
        let title = recording.title.as_deref().filter(|title| !title.trim().is_empty())
            .unwrap_or("Untitled meeting");
        let state = if recording.status == broscribe_ipc::RecordingStatus::Paused {
            "is paused"
        } else {
            "is still recording"
        };
        let detail = format!("“{title}” {state}. If you quit, recording continues, but BroScribe won't show it until you open BroScribe again.");
        let reply = main.update(cx, |_, window, cx| {
            super::kit::native_confirm_dialog(
                "Keep recording after BroScribe quits?",
                &detail,
                ["Stop and save, then quit", "Quit and keep recording"],
                window, cx,
            )
        });
        let Ok(reply) = reply else {
            pending.set(false);
            return;
        };
        cx.spawn(async move |cx| {
            let outcome = match reply.await {
                Ok(0) => {
                    let (stop, timeout) = cx.update(|cx| {
                        (
                            stop(recording.session_id, cx),
                            cx.background_executor().timer(QUIT_STOP_TIMEOUT),
                        )
                    });
                    match select(stop, Box::pin(timeout)).await {
                        Either::Left((Ok(_), _)) => Ok(true),
                        Either::Left((Err(error), _)) => {
                            Err(crate::daemon::agent_error_text(&error))
                        }
                        Either::Right(_) => {
                            tracing::warn!(method = "record.stop", code = "timeout", "quit stop wait expired");
                            Err("BroScribe Agent didn't confirm whether the recording stopped.".into())
                        }
                    }
                }
                Ok(1) => Ok(true),
                _ => Ok(false),
            };
            match outcome {
                Ok(exit) => {
                    pending.set(false);
                    if exit {
                        cx.update(quit);
                    }
                }
                Err(cause) => {
                    let detail = crate::jobs::failure_text(
                        "",
                        &cause,
                        "Check Recording controls before trying again.",
                    );
                    let notice = cx.update(|cx| {
                        main.update(cx, |_, window, cx| {
                            window.prompt(
                                gpui::PromptLevel::Warning,
                                "Couldn't stop and save the recording.",
                                Some(&detail),
                                &[gpui::PromptButton::cancel("OK")],
                                cx,
                            )
                        })
                    });
                    if let Ok(notice) = notice {
                        let _ = notice.await;
                    }
                    pending.set(false);
                }
            }
        }).detach();
    });
}

/// `MAIN_DEFAULT`, capped at 90 % of `visible` (the primary display's
/// visible area) each way.
pub fn main_default_size(visible: Option<Size<Pixels>>) -> Size<Pixels> {
    let (width, height) = MAIN_DEFAULT;
    match visible {
        Some(visible) => size(
            px(width.min(f32::from(visible.width) * MAIN_DISPLAY_SHARE)),
            px(height.min(f32::from(visible.height) * MAIN_DISPLAY_SHARE)),
        ),
        None => px_size(MAIN_DEFAULT),
    }
}

/// The main window's default size on this system's primary display.
pub fn main_default_size_here(cx: &App) -> Size<Pixels> {
    main_default_size(
        cx.primary_display()
            .map(|display| display.visible_bounds().size),
    )
}

/// GPUI's own activation: `App::set_activation_policy` and
/// `App::activate(true)`.
pub struct GpuiActivation;

impl AppActivation<App> for GpuiActivation {
    fn set_activation_policy(&self, policy: ActivationPolicy, cx: &mut App) {
        cx.set_activation_policy(match policy {
            ActivationPolicy::Regular => gpui::ActivationPolicy::Regular,
            ActivationPolicy::Accessory => gpui::ActivationPolicy::Accessory,
        });
    }

    fn activate(&self, cx: &mut App) {
        cx.activate(true);
    }
}

/// The open regular windows and the policy they call for.
pub struct WindowPresence {
    accessory_launch: bool,
    keep_dock_icon: bool,
    policy: ActivationPolicy,
    regular: HashSet<WindowId>,
    activation: Rc<dyn AppActivation<App>>,
}

impl Global for WindowPresence {}

/// Starts counting regular windows and applies the initial Dock policy.
pub fn install_presence(
    accessory: bool,
    keep_dock_icon: bool,
    activation: Rc<dyn AppActivation<App>>,
    cx: &mut App,
) {
    let policy = if accessory && !keep_dock_icon {
        ActivationPolicy::Accessory
    } else {
        ActivationPolicy::Regular
    };
    if accessory {
        activation.set_activation_policy(policy, cx);
    }
    cx.set_global(WindowPresence {
        accessory_launch: accessory,
        keep_dock_icon,
        policy,
        regular: HashSet::new(),
        activation,
    });
    cx.on_window_closed(|cx, id| {
        if cx.has_global::<WindowPresence>() {
            cx.global_mut::<WindowPresence>().regular.remove(&id);
            settle_presence(cx);
        }
    })
    .detach();
}

/// The user deliberately opens or shows a regular window: Regular first
/// when the app runs as an accessory, then the app comes forward.
pub fn activate_for_regular_window(cx: &mut App) {
    let Some(presence) = cx.try_global::<WindowPresence>() else {
        cx.activate(true);
        return;
    };
    let activation = presence.activation.clone();
    if presence.policy == ActivationPolicy::Accessory {
        cx.global_mut::<WindowPresence>().policy = ActivationPolicy::Regular;
        activation.set_activation_policy(ActivationPolicy::Regular, cx);
    }
    activation.activate(cx);
}

/// A regular window opened.
pub fn track_regular_window(id: WindowId, cx: &mut App) {
    if cx.has_global::<WindowPresence>() {
        cx.global_mut::<WindowPresence>().regular.insert(id);
    }
    crate::menu::window_activation_changed(cx);
}

pub(crate) fn bring_all_to_front(cx: &mut App) {
    cx.defer(|cx| {
        cx.activate(true);
        for handle in cx
            .window_stack()
            .unwrap_or_else(|| cx.windows())
            .into_iter()
            .rev()
        {
            if cx
                .try_global::<WindowPresence>()
                .is_some_and(|presence| !presence.regular.contains(&handle.window_id()))
            {
                continue;
            }
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
    });
}

/// Keep a way back to the app while the menu-bar preference is off.
pub fn keep_dock_icon(keep: bool, cx: &mut App) {
    let Some(presence) = cx.try_global::<WindowPresence>() else {
        return;
    };
    if presence.keep_dock_icon == keep {
        return;
    }
    let presence = cx.global_mut::<WindowPresence>();
    presence.keep_dock_icon = keep;
    if keep && presence.policy == ActivationPolicy::Accessory {
        presence.policy = ActivationPolicy::Regular;
        let activation = presence.activation.clone();
        activation.set_activation_policy(ActivationPolicy::Regular, cx);
    } else if !keep {
        settle_presence(cx);
    }
}

/// Back to Accessory when an accessory launch has no regular window open
/// (the last one closed, or one failed to open).
pub fn settle_presence(cx: &mut App) {
    let Some(presence) = cx.try_global::<WindowPresence>() else {
        return;
    };
    if presence.accessory_launch
        && !presence.keep_dock_icon
        && presence.regular.is_empty()
        && presence.policy == ActivationPolicy::Regular
    {
        let activation = presence.activation.clone();
        cx.global_mut::<WindowPresence>().policy = ActivationPolicy::Accessory;
        activation.set_activation_policy(ActivationPolicy::Accessory, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn quit_confirmation_keeps_capture_unless_the_shown_session_is_explicitly_stopped(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::daemon::{DaemonClient, EventStream, Transport};
        use broscribe_ipc::{Call, IpcError, RecordingStatus};
        use gpui::VisualTestContext;
        use std::cell::Cell;
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Agent {
            calls: Mutex<Vec<Call>>,
            accept: std::sync::atomic::AtomicBool,
        }
        impl Transport for Agent {
            fn subscribe(&self) -> Result<Box<dyn EventStream>, IpcError> {
                unreachable!()
            }
            fn call(&self, call: Call) -> Result<serde_json::Value, IpcError> {
                self.calls.lock().unwrap().push(call);
                if self.accept.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(serde_json::json!({"stopped": true}));
                }
                Err(IpcError::new(
                    "stale_session",
                    "A different recording is active",
                ))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let state = super::super::tests::shell_state(cx, dir.path());
        let deps = crate::views::setup::SetupDeps {
            mic: Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16_000)),
            system: Arc::new(broscribe_platform::fakes::FakeSystemInfo::default()),
        };
        let agent = Arc::new(Agent::default());
        let (main, daemon) = cx.update(|cx| {
            gpui::init(cx);
            (
                super::super::open_main_window(&state, &deps, cx).unwrap(),
                DaemonClient::detached(agent.clone(), cx),
            )
        });
        let pending = Rc::new(Cell::new(false));
        let exits = Rc::new(Cell::new(0));
        let recording = fake_recording();
        cx.update(|cx| {
            let (pending, exits) = (pending.clone(), exits.clone());
            let daemon = daemon.clone();
            crate::menu::install_app_menu(
                Some(&state),
                move |command, cx| {
                    if command != crate::menu::AppCommand::Quit {
                        return;
                    }
                    let Some(recording) = daemon.read(cx).panel.recording.clone() else {
                        exits.set(exits.get() + 1);
                        return;
                    };
                    let exits = exits.clone();
                    confirm_recording_quit(
                        main,
                        recording,
                        daemon.clone(),
                        pending.clone(),
                        move |_| exits.set(exits.get() + 1),
                        cx,
                    );
                },
                cx,
            );
        });
        let mut visual = VisualTestContext::from_window(main.into(), cx);
        let shortcut =
            if broscribe_platform_native::NativePlatform::host_platform() == HostPlatform::MacOs {
                "cmd-q"
            } else {
                "ctrl-q"
            };
        visual.simulate_keystrokes(shortcut);
        assert_eq!(exits.get(), 1);
        // The fake agent publishes capture without mounting its real loop.
        let set_recording = |cx: &mut App, recording| {
            daemon.update(cx, |daemon, cx| {
                daemon.panel.recording = Some(recording);
                cx.notify();
            });
        };
        visual.update(|_, cx| set_recording(cx, recording.clone()));
        visual.simulate_keystrokes(&format!("{shortcut} {shortcut}"));
        assert!(pending.get());
        assert_eq!(
            visual.pending_prompt().unwrap(),
            ("Keep recording after BroScribe quits?".into(),
             "“Synthetic orbit review” is still recording. If you quit, recording continues, but BroScribe won't show it until you open BroScribe again.".into())
        );
        visual.simulate_prompt_answer("Cancel");
        visual.run_until_parked();
        assert!(!visual.has_pending_prompt());
        assert_eq!(exits.get(), 1);
        assert!(agent.calls.lock().unwrap().is_empty());

        let mut paused = recording.clone();
        paused.status = RecordingStatus::Paused;
        visual.update(|_, cx| {
            set_recording(cx, paused.clone());
            cx.dispatch_action(&crate::menu::QuitApp);
        });
        visual.run_until_parked();
        assert!(visual.pending_prompt().unwrap().1.contains("is paused"));
        visual.simulate_prompt_answer("Quit and keep recording");
        visual.run_until_parked();
        assert_eq!(exits.get(), 2);
        assert!(agent.calls.lock().unwrap().is_empty());

        visual.update(|_, cx| cx.dispatch_action(&crate::menu::QuitApp));
        visual.run_until_parked();
        let mut replacement = recording;
        replacement.session_id = "s-synthetic-two".into();
        visual.update(|_, cx| set_recording(cx, replacement));
        visual.simulate_prompt_answer("Stop and save, then quit");
        visual.run_until_parked();
        assert_eq!(exits.get(), 2);
        assert!(
            matches!(agent.calls.lock().unwrap().as_slice(), [Call::RecordStop { session_id }] if session_id == "s-synthetic-one")
        );
        let (heading, detail) = visual.pending_prompt().unwrap();
        assert_eq!(heading, "Couldn't stop and save the recording.");
        assert!(detail.contains("A different recording is active."));
        assert!(!detail.contains("stale_session"));
        visual.simulate_prompt_answer("OK");
        visual.run_until_parked();
        assert!(!pending.get());

        agent
            .accept
            .store(true, std::sync::atomic::Ordering::Relaxed);
        visual.update(|_, cx| cx.dispatch_action(&crate::menu::QuitApp));
        visual.run_until_parked();
        visual.simulate_prompt_answer("Stop and save, then quit");
        visual.run_until_parked();
        assert_eq!(exits.get(), 3);
        assert!(!visual.has_pending_prompt());
        assert!(!pending.get());
    }

    fn fake_recording() -> broscribe_ipc::RecordingState {
        serde_json::from_value(serde_json::json!({
            "session_id": "s-synthetic-one", "status": "recording",
            "started_at": "2026-11-23T09:00:00Z", "elapsed_s": 30.,
            "title": "Synthetic orbit review", "health": { "tracks": [] }
        }))
        .unwrap()
    }

    #[gpui::test]
    fn silence_from_the_agent_keeps_the_app_open(cx: &mut gpui::TestAppContext) {
        use gpui::VisualTestContext;
        use std::cell::Cell;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let state = super::super::tests::shell_state(cx, dir.path());
        let deps = crate::views::setup::SetupDeps {
            mic: Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16_000)),
            system: Arc::new(broscribe_platform::fakes::FakeSystemInfo::default()),
        };
        let main = cx.update(|cx| {
            gpui::init(cx);
            super::super::open_main_window(&state, &deps, cx).unwrap()
        });
        let pending = Rc::new(Cell::new(false));
        let mut visual = VisualTestContext::from_window(main.into(), cx);
        visual.update(|_, cx| {
            confirm_recording_quit_with(
                main,
                fake_recording(),
                pending.clone(),
                |_| panic!("the app quit without a stop acknowledgement"),
                |session, cx| {
                    assert_eq!(session, "s-synthetic-one");
                    cx.foreground_executor().spawn(std::future::pending())
                },
                cx,
            );
        });
        visual.run_until_parked();
        visual.simulate_prompt_answer("Stop and save, then quit");
        visual.run_until_parked();
        assert!(pending.get());
        assert!(!visual.has_pending_prompt());
        visual.executor().advance_clock(QUIT_STOP_TIMEOUT);
        visual.run_until_parked();
        let (heading, detail) = visual.pending_prompt().unwrap();
        assert_eq!(heading, "Couldn't stop and save the recording.");
        assert!(detail.contains("BroScribe Agent didn't confirm whether the recording stopped."));
        visual.simulate_prompt_answer("OK");
        visual.run_until_parked();
        assert!(!pending.get());
    }

    #[test]
    fn the_main_window_default_fits_the_display() {
        assert_eq!(main_default_size(None), size(px(1120.), px(720.)));
        assert_eq!(
            main_default_size(Some(size(px(2560.), px(1415.)))),
            size(px(1120.), px(720.))
        );
        // A 1280×800 display with a 25 px menu bar and a 75 px Dock.
        assert_eq!(
            main_default_size(Some(size(px(1280.), px(700.)))),
            size(px(1120.), px(630.))
        );
        assert_eq!(
            main_default_size(Some(size(px(1000.), px(600.)))),
            size(px(900.), px(540.))
        );
    }

    #[test]
    fn every_default_is_at_least_its_minimum() {
        for (default, min) in [
            (MAIN_DEFAULT, MAIN_MIN),
            (ABOUT_SIZE, ABOUT_MIN),
            (STARTUP_SIZE, STARTUP_MIN),
            (NOTICE_SIZE, NOTICE_SIZE),
        ] {
            assert!(
                default.0 >= min.0 && default.1 >= min.1,
                "{default:?} {min:?}"
            );
        }
    }
}
