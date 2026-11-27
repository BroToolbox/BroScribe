//! The app's single Settings window.

use gpui::component::Root;
use gpui::{
    App, AppContext as _, Bounds, Entity, Global, TitlebarOptions, WeakEntity, WindowBounds,
    WindowHandle, WindowOptions,
};

use crate::state::{AppState, SettingsPage};
use crate::views::settings::SettingsView;
use crate::views::setup::SetupDeps;

use super::{assets, theme, windows};

const DEFAULT_SIZE: (f32, f32) = (760., 600.);
const MINIMUM_SIZE: (f32, f32) = (640., 480.);

struct OpenSettings {
    window: WindowHandle<Root>,
    view: WeakEntity<SettingsView>,
}

impl Global for OpenSettings {}

pub fn show_calendar(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    provider: &str,
    cx: &mut App,
) -> gpui::Result<()> {
    show(state, deps, Some(SettingsPage::Calendars), cx)?;
    if let Some(view) = cx.global::<OpenSettings>().view.upgrade() {
        view.update(cx, |view, cx| view.set_calendar_target(provider, cx));
    }
    Ok(())
}

pub fn show_word_list(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    cx: &mut App,
) -> gpui::Result<()> {
    show(state, deps, Some(SettingsPage::Transcription), cx)?;
    let open = cx.global::<OpenSettings>();
    let (handle, view) = (open.window, open.view.clone());
    handle.update(cx, |_, window, cx| {
        let _ = view.update(cx, |view, cx| view.focus_word_list(window, cx));
    })
}

pub(crate) fn show_your_voice(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    cx: &mut App,
) -> gpui::Result<()> {
    show(state, deps, Some(SettingsPage::Transcription), cx)?;
    let open = cx.global::<OpenSettings>();
    let (handle, view) = (open.window, open.view.clone());
    handle.update(cx, |_, window, cx| {
        let _ = view.update(cx, |view, _| view.scroll_to_your_voice(window));
    })
}

pub fn show_updates(state: &Entity<AppState>, deps: &SetupDeps, cx: &mut App) -> gpui::Result<()> {
    let recording = state
        .read(cx)
        .daemon()
        .is_some_and(|daemon| daemon.read(cx).panel.recording.is_some());
    let check = !recording
        && crate::views::update::UpdateNotices::current(cx)
            .is_none_or(|notices| notices.menu_state(false) == crate::menu::UpdateMenu::Check);
    show(state, deps, Some(SettingsPage::Updates), cx)?;
    if check && let Some(view) = cx.global::<OpenSettings>().view.upgrade() {
        view.update(cx, |view, cx| view.check_for_updates_from_menu(cx));
    }
    Ok(())
}

pub fn show(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    page: Option<SettingsPage>,
    cx: &mut App,
) -> gpui::Result<()> {
    windows::activate_for_regular_window(cx);
    if let Some((handle, view)) = cx
        .try_global::<OpenSettings>()
        .and_then(|open| Some((open.window, open.view.upgrade()?)))
        && handle
            .update(cx, |_, window, cx| {
                if let Some(page) = page {
                    view.update(cx, |view, cx| {
                        view.set_page(page, window, cx);
                        view.focus_first_control(window, cx);
                    });
                }
                window.activate_window();
            })
            .is_ok()
    {
        return Ok(());
    }

    let page = page
        .or_else(|| crate::ui_state::settings_page(cx))
        .unwrap_or(SettingsPage::General);
    let options = window_options(page, cx);
    let state = state.clone();
    let deps = deps.clone();
    let mut settings = None;
    let handle = match cx.open_window(options, |window, cx| {
        theme::follow_appearance(window, cx);
        let view = cx.new(|cx| {
            cx.observe_window_bounds(window, |_, window, cx| {
                crate::ui_state::remember_settings_window(window, cx);
            })
            .detach();
            cx.observe_window_activation(window, |_, _, cx| {
                crate::menu::window_activation_changed(cx)
            })
            .detach();
            let mut view = SettingsView::new(state, deps, window, cx);
            view.set_page(page, window, cx);
            view.focus_first_control(window, cx);
            view
        });
        let closing = view.downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            closing
                .update(cx, |view, cx| view.should_close(window, cx))
                .unwrap_or(true)
        });
        settings = Some(view.downgrade());
        cx.new(|cx| Root::new(view, window, cx))
    }) {
        Ok(handle) => handle,
        Err(error) => {
            windows::settle_presence(cx);
            return Err(error);
        }
    };
    windows::track_regular_window(handle.window_id(), cx);
    cx.set_global(OpenSettings {
        window: handle,
        view: settings.expect("the Settings window constructed its view"),
    });
    Ok(())
}

fn window_options(page: SettingsPage, cx: &App) -> WindowOptions {
    let (bounds, display_id) = match crate::ui_state::settings_window_bounds(DEFAULT_SIZE, cx) {
        Some((bounds, display)) => (bounds, Some(display)),
        None => (
            Bounds::centered(None, windows::px_size(DEFAULT_SIZE), cx),
            None,
        ),
    };
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        display_id,
        window_min_size: Some(windows::px_size(MINIMUM_SIZE)),
        is_minimizable: false,
        titlebar: Some(TitlebarOptions {
            title: Some(windows::title(page.title()).into()),
            ..Default::default()
        }),
        ..assets::window_options()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_core::{Library, config::Config};
    use broscribe_platform::fakes::{FakeMicCapture, FakePaths, FakeSystemInfo};
    use broscribe_platform::resolve_paths;
    use gpui::{TestAppContext, VisualContext as _, VisualTestContext};
    use std::sync::Arc;

    #[gpui::test]
    fn settings_shortcut_reuses_the_window_and_reopens_the_last_pane(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_path_buf();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(&data)).unwrap(),
                    Config::default(),
                    Library::open(data.join("library")).unwrap(),
                    data.join("synthetic-cli"),
                ),
                cx,
            )
        });
        let data = state.read_with(cx, |state, _| state.paths().data_dir.clone());
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(|cx| {
            gpui::init(cx);
            crate::ui_state::install(&data, cx);
            let options = window_options(SettingsPage::General, cx);
            assert_eq!(
                options.window_min_size,
                Some(windows::px_size((640., 480.)))
            );
            assert_eq!(
                options.window_bounds,
                Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    windows::px_size((760., 600.)),
                    cx,
                )))
            );
            assert!(!options.is_minimizable && options.is_resizable);
            let state = state.clone();
            let deps = deps.clone();
            crate::menu::install_app_menu(
                Some(&state.clone()),
                move |action, cx| {
                    if action == crate::menu::AppCommand::Settings {
                        show(&state, &deps, None, cx).unwrap();
                    }
                },
                cx,
            );
        });

        let main = cx.add_empty_window().window_handle();
        cx.dispatch_action(main, crate::menu::OpenSettings);
        let handle = cx.update(|cx| cx.global::<OpenSettings>().window);
        assert_eq!(
            VisualTestContext::from_window(handle.into(), cx)
                .window_title()
                .as_deref(),
            Some(windows::title("General").as_str())
        );
        VisualTestContext::from_window(handle.into(), cx).deactivate_window();
        Config::update(&data, |config| {
            config.consent_reminder = true;
            Ok(())
        })
        .unwrap();
        handle
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        assert!(state.read_with(cx, |state, _| state.config().consent_reminder));
        cx.update(|cx| show_calendar(&state, &deps, "google", cx).unwrap());
        assert_eq!(
            cx.update(|cx| cx.global::<OpenSettings>().window.window_id()),
            handle.window_id()
        );
        assert_eq!(
            VisualTestContext::from_window(handle.into(), cx)
                .window_title()
                .as_deref(),
            Some(windows::title("Calendars").as_str())
        );
        cx.update(|cx| show(&state, &deps, Some(SettingsPage::Updates), cx).unwrap());
        assert_eq!(
            VisualTestContext::from_window(handle.into(), cx)
                .window_title()
                .as_deref(),
            Some(windows::title("Updates").as_str())
        );
        cx.simulate_window_resize(handle.into(), windows::px_size((800., 620.)));
        handle
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        let saved = crate::ui_state::UiState::load(&data.join(crate::ui_state::FILE_NAME));
        assert!(saved.settings_window.is_some());
        assert_eq!(saved.settings_page.as_deref(), Some("updates"));
        cx.update(|cx| crate::ui_state::install(&data, cx));
        cx.dispatch_action(main, crate::menu::OpenSettings);
        let reopened = cx.update(|cx| cx.global::<OpenSettings>().window);
        assert_ne!(reopened.window_id(), handle.window_id());
        assert_eq!(
            VisualTestContext::from_window(reopened.into(), cx)
                .window_title()
                .as_deref(),
            Some(windows::title("Updates").as_str())
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.route().clone()),
            crate::state::Route::library()
        );
    }

    /// ACC-07: Settings opens with focus on the page's first control, also
    /// when another window opens it at a page, and moving through the page
    /// list keeps focus in the list.
    #[gpui::test]
    fn settings_opens_on_the_first_control_of_its_page(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_path_buf();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(&data)).unwrap(),
                    Config::default(),
                    Library::open(data.join("library")).unwrap(),
                    data.join("synthetic-cli"),
                ),
                cx,
            )
        });
        let data = state.read_with(cx, |state, _| state.paths().data_dir.clone());
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(|cx| {
            crate::ui::theme::init(cx);
            crate::ui_state::install(&data, cx);
            cx.bind_keys(crate::menu::key_bindings(cx));
        });
        cx.update(|cx| show(&state, &deps, Some(SettingsPage::General), cx).unwrap());
        let (handle, view) = cx.update(|cx| {
            let open = cx.global::<OpenSettings>();
            (open.window, open.view.upgrade().unwrap())
        });
        // The first frame draws the page, the next starts on its first control.
        let settle = |vcx: &mut VisualTestContext| {
            vcx.run_until_parked();
            vcx.update(|window, cx| {
                window.draw(cx).clear(cx);
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
        };
        // The focused element, by the ID the accessibility tree records.
        let focused_id = |vcx: &mut VisualTestContext| {
            vcx.update(|window, cx| {
                window.activate_a11y_for_tests();
                window.draw(cx).clear(cx);
                let tree: serde_json::Value =
                    serde_json::from_str(&window.debug_a11y_tree_json().unwrap()).unwrap();
                let focus = tree["gpui_focus"].as_str().expect("something has focus");
                tree["nodes"][focus]["element_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
        };
        let in_pane = |vcx: &mut VisualTestContext| {
            vcx.update(|window, cx| {
                let (pane, list, _) = view.read(cx).page_focus_for_test();
                assert!(!pane.is_focused(window), "moved past the anchor");
                assert!(pane.contains_focused(window, cx), "inside the page");
                assert!(!list.is_focused(window));
            });
        };
        let vcx = &mut VisualTestContext::from_window(handle.into(), cx);
        settle(vcx);
        in_pane(vcx);
        // The loaded config supplies the switch while its CLI read is pending.
        assert_eq!(focused_id(vcx), "Name(\"settings-login\")");
        vcx.simulate_resize(windows::px_size(MINIMUM_SIZE));
        vcx.update(|_, cx| {
            let size = broscribe_core::config::ReadingSize::Largest;
            state.update(cx, |state, cx| {
                let mut config = state.config().clone();
                config.appearance.reading_size = size;
                state.set_config(config, cx);
            });
            crate::ui::apply_reading_size(size, cx);
        });
        settle(vcx);
        vcx.update(|window, _| {
            let nodes = gpui::base::test_support::snapshots(window);
            let bounds = |id| {
                nodes
                    .iter()
                    .find(|node| {
                        node.path()
                            .last()
                            .is_some_and(|part| part.to_string() == id)
                    })
                    .unwrap_or_else(|| panic!("{id} was not laid out"))
                    .bounds()
            };
            let note = bounds("settings-login-note");
            let switch = bounds("settings-login");
            assert!(note.right() <= window.viewport_size().width, "{note:?}");
            assert!(switch.right() <= window.viewport_size().width, "{switch:?}");
            assert!(switch.top() >= note.bottom(), "{switch:?}, {note:?}");
        });
        vcx.update(|_, cx| {
            let size = broscribe_core::config::ReadingSize::Standard;
            state.update(cx, |state, cx| {
                let mut config = state.config().clone();
                config.appearance.reading_size = size;
                state.set_config(config, cx);
            });
            crate::ui::apply_reading_size(size, cx);
        });

        // Opened at another page from elsewhere (another window calls
        // `show`): that page's first control, in the same window.
        cx.update(|cx| show(&state, &deps, Some(SettingsPage::Storage), cx).unwrap());
        assert_eq!(
            cx.update(|cx| cx.global::<OpenSettings>().window.window_id()),
            handle.window_id()
        );
        let vcx = &mut VisualTestContext::from_window(handle.into(), cx);
        settle(vcx);
        in_pane(vcx);
        // Storage starts with the library folder's reveal button.
        assert_eq!(focused_id(vcx), "Name(\"reveal-library\")");

        // Arrows in the page list change the page and keep focus there.
        vcx.update(|window, cx| {
            let (_, list, _) = view.read(cx).page_focus_for_test();
            list.focus(window, cx);
        });
        vcx.simulate_keystrokes("down");
        settle(vcx);
        vcx.update(|window, cx| {
            let (_, list, page) = view.read(cx).page_focus_for_test();
            assert_ne!(page, Some(SettingsPage::Storage));
            assert!(list.is_focused(window));
        });
    }
}
