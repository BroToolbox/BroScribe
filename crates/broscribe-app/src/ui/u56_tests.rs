//! U56 mounted checks in the main window: focus targets when views open and
//! close (ACC-07), the jobs popover's Escape (ACC-24) and layouts at 200 %
//! text (ACC-31).

use broscribe_core::config::ReadingSize;
use gpui::{Focusable as _, TestAppContext, VisualTestContext, px, size};

use super::tests::{shell_state, split_meeting, split_shell};
use super::transient_editors::check;
use super::*;

/// The main window inside gpui-component's `Root`, as the app opens it, so
/// Tab, popovers, dialogs and the theme's rem size behave as they ship.
fn root_shell<'a>(
    state: &Entity<AppState>,
    cx: &'a mut TestAppContext,
) -> (Entity<MainWindow>, &'a mut VisualTestContext) {
    use broscribe_platform::fakes::{FakeMicCapture, FakeSystemInfo};
    let state = state.clone();
    cx.update(|cx| {
        theme::init(cx);
        cx.bind_keys(crate::menu::key_bindings(cx));
    });
    let main = std::rc::Rc::new(std::cell::RefCell::new(None));
    let (_, vcx) = cx.add_window_view({
        let main = main.clone();
        move |window, cx| {
            let view = cx.new(|cx| {
                MainWindow::new(
                    state,
                    SetupDeps {
                        mic: Arc::new(FakeMicCapture::new(16_000)),
                        system: Arc::new(FakeSystemInfo::default()),
                    },
                    window,
                    cx,
                )
            });
            *main.borrow_mut() = Some(view.clone());
            Root::new(view, window, cx)
        }
    });
    let main = main.borrow_mut().take().expect("the main window was built");
    vcx.update(|window, _| window.activate_window());
    vcx.run_until_parked();
    (main, vcx)
}

fn job(state: &Entity<AppState>, vcx: &mut VisualTestContext, id: &str) {
    state.update(vcx, |state, cx| {
        state.daemon_progress(
            &broscribe_ipc::JobProgress {
                job_id: id.into(),
                kind: "record-final".into(),
                meeting_id: None,
                phase: "started".into(),
                fraction: None,
                message: None,
            },
            cx,
        )
    });
    vcx.run_until_parked();
}

/// ACC-24: the "+N more" jobs popover opens from the keyboard, Escape closes
/// it and focus stays on its button.
#[gpui::test]
fn jobs_popover_closes_on_escape_back_to_its_button(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = shell_state(cx, dir.path());
    let (main, vcx) = root_shell(&state, cx);
    vcx.simulate_resize(size(px(1200.), px(760.)));
    // Start in the sidebar, as an empty Library opens.
    vcx.update(|window, cx| main.read(cx).sidebar_rows[0].clone().focus(window, cx));
    job(&state, vcx, "synthetic-a");
    job(&state, vcx, "synthetic-b");
    // F6 reaches the status area, as a keyboard user gets there.
    for _ in 0..6 {
        let status = main.read_with(vcx, |main, _| main.status_focus.clone());
        if vcx.update(|window, cx| status.contains_focused(window, cx)) {
            break;
        }
        vcx.simulate_keystrokes("f6");
        vcx.run_until_parked();
    }
    check::keyboard_open_then_escape(vcx, "jobs-more", "jobs");
}

/// ACC-07: each view takes focus on its main target when it opens: the
/// Library list (with the meeting you came from selected), the opened
/// meeting's transcript, the People list and the Review queue. Coming back
/// from a meeting to the Library lands on the list, not the sidebar.
#[gpui::test]
fn navigation_focuses_each_views_main_target(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = shell_state(cx, dir.path());
    let newer = split_meeting(&state, "Synthetic newer meeting", cx);
    let library = state.read_with(cx, |state, _| state.library().clone());
    broscribe_core::ops::with_index(&library, |tx, index| {
        let mut meeting = tx.library().load_meeting(&newer)?;
        meeting.segments.push(broscribe_core::schema::Segment {
            speech_overlap: Vec::new(),
            id: 0,
            track: "mixed".into(),
            speaker: None,
            start: 0.,
            end: 1.,
            text: "Invented line for the focus check.".into(),
            words: Vec::new(),
        });
        tx.save_meeting(&meeting)?;
        index.upsert_meeting(tx, &meeting)
    })
    .unwrap();
    let (main, vcx) = split_shell(&state, cx);
    vcx.update(|window, cx| main.update(cx, |main, cx| main.focus_on_open(window, cx)));
    // One pane at a time, so opening a meeting replaces the list.
    vcx.simulate_resize(size(px(700.), px(760.)));
    vcx.run_until_parked();
    vcx.update(|window, cx| {
        assert!(main.read(cx).library.focus_handle(cx).is_focused(window));
    });
    vcx.simulate_keystrokes("down enter");
    vcx.run_until_parked();
    vcx.update(|window, cx| window.draw(cx).clear(cx));
    vcx.run_until_parked();
    vcx.update(|window, cx| {
        let shell = main.read(cx);
        let meeting = shell.meeting_view().expect("the meeting opened").clone();
        assert!(
            meeting.read(cx).detail_focus().is_focused(window),
            "the opened meeting's transcript has focus"
        );
    });

    vcx.dispatch_action(crate::menu::NavigateBack);
    vcx.run_until_parked();
    vcx.update(|window, cx| {
        let shell = main.read(cx);
        assert!(shell.library.focus_handle(cx).is_focused(window));
        assert_eq!(shell.library.read(cx).selected_meeting(), Some(&newer));
    });

    state.update(vcx, |state, cx| state.navigate(Route::people(), cx));
    vcx.run_until_parked();
    vcx.update(|window, cx| {
        let people = main.read(cx).people.clone().expect("People opened");
        assert!(
            people.focus_handle(cx).is_focused(window),
            "the People list"
        );
    });

    state.update(vcx, |state, cx| state.navigate(Route::ReviewQueue, cx));
    vcx.run_until_parked();
    vcx.update(|window, cx| {
        let review = main.read(cx).review_view().expect("Review opened").clone();
        assert!(review.focus_handle(cx).contains_focused(window, cx));
    });

    state.update(vcx, |state, cx| state.navigate(Route::library(), cx));
    vcx.run_until_parked();
    vcx.update(|window, cx| {
        let shell = main.read(cx);
        assert!(shell.library.focus_handle(cx).is_focused(window));
        assert_eq!(shell.library.read(cx).selected_meeting(), Some(&newer));
    });
}

/// ACC-31: at 200 % the window's rem doubles and named controls fit between
/// the window edges at its default and minimum sizes.
#[gpui::test]
fn the_main_window_fits_at_200_percent_text(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = shell_state(cx, dir.path());
    split_meeting(
        &state,
        "Synthetic meeting with a rather long title to wrap",
        cx,
    );
    let (_, vcx) = root_shell(&state, cx);
    let standard = vcx.update(|window, _| window.rem_size());
    let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());
    std::fs::create_dir_all(&data_dir).unwrap();
    let (saved, ()) = broscribe_core::config::Config::update(&data_dir, |config| {
        config.appearance.reading_size = ReadingSize::Largest;
        Ok(())
    })
    .unwrap();
    state.update(vcx, |s, cx| s.set_config(saved, cx));
    vcx.run_until_parked();
    for (width, height) in [(1120., 720.), (1000., 600.), (600., 420.)] {
        vcx.simulate_resize(size(px(width), px(height)));
        vcx.run_until_parked();
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(vcx.update(|window, _| window.rem_size()), standard * 2.);
        // Nothing named runs off the window's right edge.
        vcx.update(|window, _| {
            for node in gpui::base::test_support::snapshots(window) {
                let named = node.label().is_some_and(|label| !label.is_empty());
                let bounds = node.bounds();
                if named && bounds.size.width > px(0.) {
                    assert!(
                        bounds.left() >= px(-0.5) && bounds.right() <= px(width) + px(0.5),
                        "{:?} {bounds:?} runs past {width} px at 200 %",
                        node.label(),
                    );
                }
            }
        });
    }
}

/// Tab from the toolbar reaches the sidebar's one stop and the Library
/// list's one stop in the split view (design §4 Main window navigation).
#[gpui::test]
fn tab_reaches_the_sidebar_and_library_list_stops_in_the_split(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = shell_state(cx, dir.path());
    split_meeting(&state, "Synthetic tab meeting", cx);
    let (main, vcx) = root_shell(&state, cx);
    // The toolbar draws its Main menu button where there is no menu bar.
    vcx.update(|_, cx| {
        main.update(cx, |main, cx| {
            main.host = broscribe_platform::HostPlatform::Windows;
            cx.notify();
        })
    });
    vcx.simulate_resize(size(px(1400.), px(900.)));
    vcx.update(|window, cx| main.update(cx, |main, cx| main.focus_on_open(window, cx)));
    vcx.run_until_parked();
    let mut sidebar = false;
    let mut list = false;
    // A div that tracks a view's own handle takes its stop from the handle,
    // not from `tab_index`, so the lists set it there. Toolbar buttons stay
    // stops beside them.
    let mut toggle = false;
    let mut menu = false;
    for _ in 0..30 {
        vcx.simulate_keystrokes("tab");
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let shell = main.read(cx);
            sidebar |= shell.sidebar_rows.iter().any(|row| row.is_focused(window));
            list |= shell.library.focus_handle(cx).is_focused(window);
            toggle |= shell.sidebar_toggle_focus.is_focused(window);
            menu |= shell.main_menu_focus.is_focused(window);
        });
    }
    assert!(
        sidebar && list && toggle && menu,
        "Tab reached sidebar: {sidebar}, list: {list}, sidebar toggle: {toggle}, main menu: {menu}"
    );
}
