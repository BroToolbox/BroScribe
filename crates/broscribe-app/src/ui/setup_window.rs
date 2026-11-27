//! The non-modal setup window, shared by first run and the Settings entry.

use gpui::component::Root;
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, Global, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Subscription, TitlebarOptions, WeakEntity, Window,
    WindowBounds, WindowHandle, WindowOptions, div,
};

use crate::menu::CloseWindow;
use crate::state::AppState;
use crate::views::setup::{SetupAssistant, SetupDeps, SetupEvent};

use super::{assets, chrome, theme, windows};

const TITLE: &str = "Set up BroScribe";
const DEFAULT_SIZE: (f32, f32) = (760., 600.);
const MINIMUM_SIZE: (f32, f32) = (640., 480.);

struct OpenSetup {
    window: WindowHandle<Root>,
    assistant: WeakEntity<SetupAssistant>,
}

impl Global for OpenSetup {}

pub(crate) fn capture_active(cx: &App) -> bool {
    cx.try_global::<OpenSetup>()
        .and_then(|open| open.assistant.upgrade())
        .is_some_and(|assistant| assistant.read(cx).is_capturing())
}

/// Opens the assistant once, or brings its existing window forward.
pub(crate) fn show(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    cx: &mut App,
) -> gpui::Result<Entity<SetupAssistant>> {
    show_with_focus(state, deps, true, cx)
}

pub(super) fn show_unfocused(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    cx: &mut App,
) -> gpui::Result<Entity<SetupAssistant>> {
    show_with_focus(state, deps, false, cx)
}

fn show_with_focus(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    focus: bool,
    cx: &mut App,
) -> gpui::Result<Entity<SetupAssistant>> {
    if focus {
        windows::activate_for_regular_window(cx);
    }
    if let Some((window, assistant)) = cx
        .try_global::<OpenSetup>()
        .and_then(|open| Some((open.window, open.assistant.upgrade()?)))
        && window
            .update(cx, |_, window, _| {
                if focus {
                    window.activate_window();
                }
            })
            .is_ok()
    {
        return Ok(assistant);
    }

    let state = state.clone();
    let deps = deps.clone();
    let mut assistant = None;
    let mut options = window_options(cx);
    options.focus = focus;
    let window = match cx.open_window(options, |window, cx| {
        theme::follow_appearance(window, cx);
        window.set_window_title(&windows::title(TITLE));
        let setup = cx.new(|cx| SetupAssistant::new(state, deps, window, cx));
        assistant = Some(setup.clone());
        let content = cx.new(|cx| SetupWindow::new(setup, window, cx));
        cx.new(|cx| Root::new(content, window, cx))
    }) {
        Ok(window) => window,
        Err(error) => {
            windows::settle_presence(cx);
            return Err(error);
        }
    };
    let assistant = assistant.expect("the setup window constructed its assistant");
    windows::track_regular_window(window.window_id(), cx);
    cx.set_global(OpenSetup {
        window,
        assistant: assistant.downgrade(),
    });
    assistant.update(cx, |assistant, cx| assistant.clear_deferral(cx));
    Ok(assistant)
}

pub(crate) fn request_step(step: crate::views::setup::Step, cx: &mut App) {
    let Some((window, assistant)) = cx
        .try_global::<OpenSetup>()
        .and_then(|setup| Some((setup.window, setup.assistant.upgrade()?)))
    else {
        return;
    };
    let _ = window.update(cx, |_, window, cx| {
        assistant.update(cx, |assistant, cx| assistant.request_step(step, window, cx))
    });
}

/// Observes the assistant's window closing from any of its entry points.
pub(crate) fn on_closed(
    cx: &mut App,
    mut callback: impl FnMut(&mut App) + 'static,
) -> Subscription {
    cx.on_window_closed(move |cx, id| {
        if cx
            .try_global::<OpenSetup>()
            .is_some_and(|setup| setup.window.window_id() == id)
        {
            callback(cx);
        }
    })
}

fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            windows::px_size(DEFAULT_SIZE),
            cx,
        ))),
        window_min_size: Some(windows::px_size(MINIMUM_SIZE)),
        titlebar: Some(TitlebarOptions {
            title: Some(windows::title(TITLE).into()),
            ..Default::default()
        }),
        ..assets::window_options()
    }
}

struct SetupWindow {
    assistant: Entity<SetupAssistant>,
    _closed: Subscription,
}

impl SetupWindow {
    fn new(assistant: Entity<SetupAssistant>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_window_activation(window, |_, _, cx| {
            crate::menu::window_activation_changed(cx)
        })
        .detach();
        let closed = cx.subscribe_in(&assistant, window, |_, _, event, window, _| match event {
            SetupEvent::Closed => window.remove_window(),
        });
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |this, cx| this.close(window, cx)).is_err()
        });
        Self {
            assistant,
            _closed: closed,
        }
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.assistant
            .update(cx, |assistant, cx| assistant.request_close(window, cx));
    }
}

impl Render for SetupWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let setup = cx.weak_entity();
        let handle = window.window_handle();
        chrome::framed_with_close(
            TITLE,
            div()
                .size_full()
                .on_action(cx.listener(|this, _: &CloseWindow, window, cx| {
                    this.close(window, cx);
                    cx.stop_propagation();
                }))
                .child(self.assistant.clone()),
            window,
            move |cx| {
                let _ = cx.update_window(handle, |_, window, cx| {
                    let _ = setup.update(cx, |this, cx| this.close(window, cx));
                });
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_core::config::Config;
    use broscribe_platform::fakes::{FakeMicCapture, FakeSystemInfo};
    use gpui::{TestAppContext, VisualTestContext};
    use std::sync::Arc;

    #[gpui::test]
    fn first_launch_opens_one_assistant_and_set_up_later_leaves_main_available(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let state = super::super::tests::shell_state(cx, dir.path());
        let data = state.read_with(cx, |state, _| state.paths().data_dir.clone());
        let config = Config::update(&data, |config| {
            config.setup.completed_steps = vec!["languages".into()];
            Ok(())
        })
        .unwrap()
        .0;
        state.update(cx, |state, cx| state.set_config(config, cx));
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        let main = cx.update(|cx| {
            gpui::init(cx);
            crate::ui_state::install(&data, cx);
            let options = window_options(cx);
            assert_eq!(
                options.window_min_size,
                Some(windows::px_size(MINIMUM_SIZE))
            );
            assert_eq!(
                options.window_bounds,
                Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    windows::px_size(DEFAULT_SIZE),
                    cx,
                )))
            );
            super::super::open_main_window(&state, &deps, cx).unwrap()
        });
        let (handle, assistant) = cx.update(|cx| {
            let opened = cx.global::<OpenSetup>();
            (opened.window, opened.assistant.upgrade().unwrap())
        });
        assert_eq!(
            VisualTestContext::from_window(handle.into(), cx)
                .window_title()
                .as_deref(),
            Some(windows::title(TITLE).as_str()),
        );
        assert_eq!(
            assistant.read_with(cx, |assistant, _| assistant.step()),
            crate::views::setup::Step::Models
        );
        assert_eq!(cx.update(|cx| show(&state, &deps, cx).unwrap()), assistant);
        assert_eq!(cx.windows().len(), 2);
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(cx.windows().len(), 1);
        let saved = Config::load(&data).unwrap();
        assert!(saved.setup.defer_assistant);
        assert_eq!(saved.setup.completed_steps, ["languages"]);

        main.update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| super::super::open_main_window(&state, &deps, cx).unwrap());
        assert_eq!(cx.windows().len(), 1);
    }
}
