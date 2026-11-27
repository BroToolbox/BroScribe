//! The title bar BroScribe draws itself where the window system leaves a
//! window's decorations to the app (SHL-03).
//!
//! GPUI asks for server-side decorations by default and BroScribe keeps
//! that request. A Linux compositor without a decoration manager (GNOME's
//! Mutter, Weston) answers with client-side decorations: the window then
//! has no title, no controls and nothing to drag unless the app draws
//! them. gpui-component's `Root` already gives such a window its resize
//! edges and shadow; this module adds the bar.
//!
//! - [`framed`] is for every ordinary window (main, About, startup error,
//!   maintenance): a 34 px row in the title-bar colours holding the
//!   window's title as a named heading and, as named buttons, the window
//!   controls the window system supports: Minimize, Maximize (Restore
//!   while maximized) and Close, which is always there. Pressing the title
//!   and moving the pointer moves the window, wherever that first motion
//!   lands, a double click on it maximizes or restores the window and a
//!   right click opens the window system's window menu.
//! - [`framed_panel`] is for the recording controls, which cannot be
//!   minimized (design §4): the same row with the title and Close only.
//!   Closing never stops capture.
//!
//! The row is BroScribe's, not gpui-component's `TitleBar`: that bar draws
//! its window controls as plain elements with no role or name, so
//! assistive technology could not reach them.
//!
//! Which decorations a window has is asked of the window each time it
//! renders ([`Window::window_decorations`]), so nothing here depends on the
//! operating system: macOS, Windows and a server-decorated Linux window
//! report server-side decorations and get nothing added. No control is a
//! tab stop, as a native title bar's are not, so the bar takes no keyboard
//! focus when its window opens.

use gpui::base::TextSelection;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::{
    ActiveTheme as _, GlobalState, IconName, Sizable as _, TITLE_BAR_HEIGHT, window_corner_radii,
};
use gpui::{
    AnyElement, App, ClickEvent, Decorations, DispatchPhase, Div, Entity, FontWeight,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, RenderOnce, Role, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Window, WindowControlArea, canvas, div,
    prelude::FluentBuilder as _, px,
};

use super::text_line;
use super::type_scale::{TextRole, TextRoleExt as _};

/// What assistive technology calls the Minimize button.
pub(crate) const MINIMIZE_LABEL: &str = "Minimize";
/// What assistive technology calls the Maximize button of a window that is
/// not maximized.
pub(crate) const MAXIMIZE_LABEL: &str = "Maximize";
/// What assistive technology calls the same button while the window is
/// maximized.
pub(crate) const RESTORE_LABEL: &str = "Restore";
/// What assistive technology calls an ordinary window's Close button.
pub(crate) const CLOSE_LABEL: &str = "Close";
/// What assistive technology calls the recording controls' Close button.
pub(crate) const PANEL_CLOSE_LABEL: &str = "Close recording controls";

/// The bar.
const BAR: &str = "window-bar";
/// The part of the bar that holds the title and moves the window.
const DRAG: &str = "window-bar-drag";
/// Whether a press on [`DRAG`] may still move the window.
const DRAG_STATE: &str = "window-bar-drag-state";
/// The window's title in the bar.
const TITLE: &str = "window-title";
/// The view under the bar, for the tests.
const CONTENT: &str = "window-content";
/// The Minimize button.
const MINIMIZE: &str = "window-minimize";
/// The Maximize or Restore button.
const MAXIMIZE: &str = "window-maximize";
/// The Close button.
const CLOSE: &str = "window-close";

/// The space before the title, as in gpui-component's `TitleBar` off macOS.
const TITLE_INSET: f32 = 12.;
pub(super) const TOOLBAR_HEIGHT: gpui::Pixels = px(44.);

/// Whether BroScribe has to draw `window`'s title bar: the window system
/// left its decorations to the app.
pub(super) fn client_decorated(window: &Window) -> bool {
    matches!(window.window_decorations(), Decorations::Client { .. })
}

/// The window's title in its bar: a heading named by its text, on one line.
fn window_title(title: SharedString) -> Stateful<Div> {
    text_line(TITLE, Role::Heading, title.clone())
        .aria_level(1)
        .debug_selector(|| TITLE.into())
        .text_role(TextRole::Label)
        .font_weight(FontWeight::SEMIBOLD)
        .min_w_0()
        .truncate()
        .tooltip(super::kit::title_tooltip(title))
}

/// `bar` above `content`, which keeps the rest of the window.
fn under(bar: impl IntoElement, content: impl IntoElement) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(bar)
        .child(
            div()
                .debug_selector(|| CONTENT.into())
                .flex_1()
                .min_h_0()
                .child(content),
        )
        .into_any_element()
}

/// An ordinary window's root: `content` as it is under server-side
/// decorations; under client-side ones, below a bar holding `title` and
/// the window controls.
pub(crate) fn framed(
    title: impl Into<SharedString>,
    content: impl IntoElement,
    window: &Window,
) -> AnyElement {
    if !client_decorated(window) {
        return content.into_any_element();
    }
    under(WindowBar::new(Controls::Window, title.into()), content)
}

/// An auxiliary window with a close handler that can await cleanup.
pub(crate) fn framed_with_close(
    title: impl Into<SharedString>,
    content: impl IntoElement,
    window: &Window,
    close: impl Fn(&mut App) + 'static,
) -> AnyElement {
    if !client_decorated(window) {
        return content.into_any_element();
    }
    let mut bar = WindowBar::new(Controls::Window, title.into());
    bar.close = Some(Box::new(close));
    under(bar, content)
}

/// Client-decorated main windows put the toolbar beside the named window
/// controls in the same row. The title remains the drag target.
pub(super) fn with_toolbar(
    title: SharedString,
    toolbar: AnyElement,
    content: impl IntoElement,
) -> AnyElement {
    let mut bar = WindowBar::new(Controls::Window, title);
    bar.toolbar = Some(toolbar);
    under(bar, content)
}

/// The recording controls' root: `content` as it is under server-side
/// decorations; under client-side ones, below a bar holding `title` and
/// Close only.
pub(crate) fn framed_panel(
    title: impl Into<SharedString>,
    content: impl IntoElement,
    window: &Window,
) -> AnyElement {
    if !client_decorated(window) {
        return content.into_any_element();
    }
    under(WindowBar::new(Controls::PanelClose, title.into()), content)
}

pub(crate) fn panel_chrome_height(window: &Window) -> gpui::Pixels {
    if client_decorated(window) {
        TITLE_BAR_HEIGHT
    } else {
        px(0.)
    }
}

/// Which controls a bar offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Controls {
    /// An ordinary window's: Minimize, Maximize or Restore where the
    /// window system supports them, and Close.
    Window,
    /// The recording controls': Close only. The panel cannot be minimized
    /// (design §4), and neither a double click nor the window menu offers
    /// to maximize or minimize it.
    PanelClose,
}

type CloseHandler = Box<dyn Fn(&mut App)>;

/// The bar a client-decorated window shows above its content.
#[derive(IntoElement)]
struct WindowBar {
    controls: Controls,
    /// Built with the bar, so a render names the heading at once.
    title: Stateful<Div>,
    toolbar: Option<AnyElement>,
    close: Option<CloseHandler>,
}

impl WindowBar {
    fn new(controls: Controls, title: SharedString) -> Self {
        Self {
            controls,
            title: window_title(title),
            toolbar: None,
            close: None,
        }
    }
}

/// Whether the last left press on the title may still turn into a window
/// move: set by the press, cleared by the release, a press elsewhere or the
/// first move, which starts the move.
#[derive(Default)]
struct DragState {
    armed: bool,
}

/// Follows an armed press on the title across the whole window, not only
/// over the bar: GPUI calls an element's `on_mouse_move` only while the
/// pointer is over it, and a quick drag from low on the 34 px bar can
/// leave it with its first motion. That motion starts the move while the
/// left button is still down; a release anywhere, or a motion without the
/// button, disarms the press. The listeners run in the capture phase, so
/// nothing under the pointer can swallow the motion first.
fn follow_drag(drag: Entity<DragState>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |_, (), window, _| {
            window.on_mouse_event({
                let drag = drag.clone();
                move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture || !drag.read(cx).armed {
                        return;
                    }
                    drag.update(cx, |state, _| state.armed = false);
                    // The move starts with the first motion after the
                    // press, not with the press, so a click on the title
                    // stays a click.
                    if event.pressed_button == Some(MouseButton::Left) {
                        TextSelection::end(window, cx);
                        cx.stop_active_drag(window);
                        window.start_window_move();
                    }
                }
            });
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture
                    && event.button == MouseButton::Left
                    && drag.read(cx).armed
                {
                    drag.update(cx, |state, _| state.armed = false);
                }
            });
        },
    )
    .absolute()
    .size_0()
}

impl RenderOnce for WindowBar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let drag = window.use_keyed_state(DRAG_STATE, cx, |_, _| DragState::default());
        let supported = window.window_controls();
        let ordinary = self.controls == Controls::Window;
        let can_minimize = ordinary && supported.minimize && window.is_minimizable();
        let can_maximize = ordinary && supported.maximize && window.is_resizable();
        let window_menu = ordinary && supported.window_menu;
        let maximized = window.is_maximized();

        let title_area = div()
            .id(DRAG)
            .debug_selector(|| DRAG.into())
            .window_control_area(WindowControlArea::Drag)
            .flex()
            .when(self.toolbar.is_none(), |el| el.flex_1())
            .when(self.toolbar.is_some(), |el| {
                el.w(gpui::rems(6.)).flex_none()
            })
            .min_w_0()
            .h_full()
            .items_center()
            .pl(px(TITLE_INSET))
            .on_mouse_down(
                MouseButton::Left,
                window.listener_for(&drag, |state, _: &MouseDownEvent, window, cx| {
                    state.armed = !window.default_prevented();
                    // The bar moves the window; it never starts a text
                    // selection.
                    if cx.has_global::<GlobalState>() {
                        GlobalState::suppress_text_selection(cx);
                    }
                }),
            )
            .when(can_maximize, |area| {
                area.on_click(|event: &ClickEvent, window, _| {
                    if event.click_count() == 2 {
                        window.zoom_window();
                    }
                })
            })
            .when(window_menu, |area| {
                area.on_mouse_down(MouseButton::Right, |event, window, _| {
                    window.show_window_menu(event.position)
                })
            })
            .child(
                self.title
                    .when(self.toolbar.is_some(), |title| title.max_w(gpui::rems(4.5))),
            );

        bar_row(window, cx)
            .when(self.toolbar.is_some(), |el| el.h(TOOLBAR_HEIGHT))
            .on_mouse_down_out(
                window.listener_for(&drag, |state, _: &MouseDownEvent, _, _| {
                    state.armed = false;
                }),
            )
            .child(follow_drag(drag))
            .child(title_area)
            .children(self.toolbar)
            .when(can_minimize, |bar| {
                bar.child(control(
                    MINIMIZE,
                    IconName::WindowMinimize,
                    MINIMIZE_LABEL,
                    |window, _| window.minimize_window(),
                ))
            })
            .when(can_maximize, |bar| {
                let (icon, label) = if maximized {
                    (IconName::WindowRestore, RESTORE_LABEL)
                } else {
                    (IconName::WindowMaximize, MAXIMIZE_LABEL)
                };
                bar.child(control(MAXIMIZE, icon, label, |window, _| {
                    window.zoom_window()
                }))
            })
            .child(control(
                CLOSE,
                IconName::WindowClose,
                if ordinary {
                    CLOSE_LABEL
                } else {
                    PANEL_CLOSE_LABEL
                },
                // Only removes the window: for the recording controls,
                // capture goes on without them.
                move |window, cx| {
                    if let Some(close) = &self.close {
                        close(cx);
                    } else {
                        window.remove_window();
                    }
                },
            ))
    }
}

/// The row every bar is drawn in: as tall as gpui-component's `TitleBar`
/// and in the theme's title-bar colours.
fn bar_row(window: &Window, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let radii = window_corner_radii(window, cx);
    div()
        .id(BAR)
        .debug_selector(|| BAR.into())
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .h(TITLE_BAR_HEIGHT)
        .pr_1()
        .rounded_tl(radii.top_left)
        .rounded_tr(radii.top_right)
        .border_b_1()
        .border_color(theme.title_bar_border)
        .bg(theme.title_bar)
        .text_color(theme.foreground)
}

/// One window control: a named button that is no tab stop, as a native
/// title bar's controls are not.
fn control(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    act: impl Fn(&mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id)
        .debug_selector(move || id.into())
        .ghost()
        .small()
        .icon(icon)
        .accessibility_label(label)
        .tooltip(label)
        .tab_stop(false)
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            act(window, cx);
        })
}

/// What the tests of every window share: the window system's answer, and
/// what a drawn window shows of its bar.
#[cfg(test)]
pub(crate) mod testing {
    use gpui::component::TITLE_BAR_HEIGHT;
    use gpui::test::TestWindowExt as _;
    use gpui::{
        AnyWindowHandle, Bounds, Decorations, Pixels, Render, Role, TestAppContext, Tiling,
        VisualTestContext, WindowControls, WindowHandle, point, px, size,
    };

    use super::{BAR, CLOSE, CONTENT, DRAG, MAXIMIZE, MINIMIZE, TITLE};
    use crate::ui::spoken_log;

    /// Every window opened from now on is left its decorations, as a
    /// compositor without a decoration manager leaves them, with every
    /// window control supported.
    pub(crate) fn report_client_decorations(cx: &mut TestAppContext) {
        report_client_decorations_with(cx, WindowControls::default());
    }

    /// As [`report_client_decorations`], with only `controls` supported.
    pub(crate) fn report_client_decorations_with(
        cx: &mut TestAppContext,
        controls: WindowControls,
    ) {
        cx.set_window_system(
            Decorations::Client {
                tiling: Tiling::default(),
            },
            controls,
        );
    }

    /// A window control as a drawn window shows it.
    #[derive(Clone, Debug)]
    pub(crate) struct Control {
        /// Its element ID.
        pub(crate) id: &'static str,
        /// Where it is drawn.
        pub(crate) bounds: Bounds<Pixels>,
        /// Its accessible role.
        pub(crate) role: Option<Role>,
        /// The name assistive technology reads for it.
        pub(crate) label: Option<String>,
        /// Whether it has keyboard focus.
        pub(crate) focused: bool,
    }

    /// What a drawn window shows of the chrome BroScribe draws.
    #[derive(Debug)]
    pub(crate) struct Chrome {
        /// The window's drawable area.
        pub(crate) viewport: Bounds<Pixels>,
        /// The bar.
        pub(crate) bar: Option<Bounds<Pixels>>,
        /// The part of the bar that moves the window.
        pub(crate) drag: Option<Bounds<Pixels>>,
        /// The title in the bar.
        pub(crate) title: Option<Bounds<Pixels>>,
        /// The view below the bar.
        pub(crate) content: Option<Bounds<Pixels>>,
        /// The window controls in the bar, left to right.
        pub(crate) controls: Vec<Control>,
        /// Whether anything in the window has keyboard focus.
        pub(crate) focused: bool,
    }

    impl Chrome {
        /// Asserts the window shows no bar of BroScribe's, as under
        /// server-side decorations.
        pub(crate) fn assert_none(&self) {
            assert_eq!(self.bar, None, "a title bar was drawn");
            assert_eq!(self.title, None, "a window title was drawn");
            assert_eq!(self.content, None, "the view was wrapped");
            assert!(self.controls.is_empty(), "{:?}", self.controls);
        }

        /// Asserts the bar is the first thing in the window (its full
        /// width, 34 px from the top), holds the title and every control,
        /// and the view has the rest.
        pub(crate) fn assert_first(&self) {
            self.assert_first_with_height(TITLE_BAR_HEIGHT);
        }

        pub(crate) fn assert_first_with_height(&self, height: Pixels) {
            let viewport = self.viewport;
            let bar = self.bar.expect("a title bar");
            assert_eq!(
                bar,
                Bounds::new(viewport.origin, size(viewport.size.width, height)),
                "the bar is the window's first row"
            );
            assert_eq!(TITLE_BAR_HEIGHT, px(34.));
            let inside = |inner: Bounds<Pixels>| {
                bar.contains(&inner.origin) && bar.contains(&inner.bottom_right())
            };
            let title = self.title.expect("the bar holds the window's title");
            assert!(inside(title), "the title {title:?} is outside {bar:?}");
            assert!(title.size.width > px(0.), "the title has no width");
            // The title's side moves the window; the controls are beside
            // it, not in it.
            let drag = self.drag.expect("a drag area");
            assert!(
                drag.contains(&title.origin) && drag.contains(&title.bottom_right()),
                "title {title:?} is outside drag area {drag:?}"
            );
            for control in &self.controls {
                assert!(inside(control.bounds), "{control:?} is outside {bar:?}");
                assert!(drag.right() <= control.bounds.left(), "{control:?}");
            }
            assert_eq!(
                self.content,
                Some(Bounds::from_corners(
                    point(viewport.left(), bar.bottom()),
                    viewport.bottom_right()
                )),
                "the view has the rest of the window"
            );
        }

        /// The names assistive technology reads for the controls, left to
        /// right; asserts each is a button without focus.
        pub(crate) fn control_labels(&self) -> Vec<&str> {
            self.controls
                .iter()
                .map(|control| {
                    assert_eq!(control.role, Some(Role::Button), "{control:?}");
                    assert!(!control.focused, "{control:?} has focus");
                    control.label.as_deref().expect("a named control")
                })
                .collect()
        }

        /// The control assistive technology calls `label`.
        pub(crate) fn control(&self, label: &str) -> &Control {
            let mut found = self
                .controls
                .iter()
                .filter(|control| control.label.as_deref() == Some(label));
            let control = found
                .next()
                .unwrap_or_else(|| panic!("no {label:?} in {:?}", self.controls));
            assert!(found.next().is_none(), "two {label:?} controls");
            control
        }
    }

    /// The window title a render of `window`'s view names as a heading
    /// for assistive technology, if it names one.
    pub(crate) fn title_heading<V: Render + 'static>(
        window: WindowHandle<V>,
        cx: &mut TestAppContext,
    ) -> Option<String> {
        let named = window
            .update(cx, |view, window, cx| {
                spoken_log::while_building(|| Render::render(view, window, cx)).1
            })
            .unwrap();
        let mut titles = named
            .into_iter()
            .filter(|(id, ..)| id == TITLE)
            .map(|(_, role, text)| {
                assert_eq!(role, Role::Heading, "the window title is a heading");
                text
            });
        let title = titles.next();
        assert_eq!(titles.next(), None, "one window title");
        title
    }

    /// Draws `window` afresh and reads its chrome.
    pub(crate) fn chrome(window: AnyWindowHandle, cx: &mut TestAppContext) -> Chrome {
        cx.run_until_parked();
        let mut cx = VisualTestContext::from_window(window, cx);
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let (viewport, mut controls, focused) = cx.update(|window, cx| {
            let controls: Vec<Control> = [MINIMIZE, MAXIMIZE, CLOSE]
                .into_iter()
                .flat_map(|id| {
                    window.find_all(id).into_iter().map(move |found| Control {
                        id,
                        bounds: found.bounds(),
                        role: found.role(),
                        label: found.label().map(str::to_owned),
                        focused: found.focused() == Some(true),
                    })
                })
                .collect();
            (
                Bounds::new(Default::default(), window.viewport_size()),
                controls,
                window.focused(cx).is_some(),
            )
        });
        controls.sort_by(|a, b| a.bounds.left().partial_cmp(&b.bounds.left()).unwrap());
        Chrome {
            viewport,
            bar: cx.debug_bounds(BAR),
            drag: cx.debug_bounds(DRAG),
            title: cx.debug_bounds(TITLE),
            content: cx.debug_bounds(CONTENT),
            controls,
            focused,
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        AppContext as _, Bounds, Context, Fill, Modifiers, Point, Render, TestAppContext,
        VisualTestContext, WindowBounds, WindowControls, WindowOptions, point,
    };

    use super::testing::{
        Chrome, chrome, report_client_decorations, report_client_decorations_with, title_heading,
    };
    use super::*;

    const PROBE_TITLE: &str = "Synthetic window";
    const PROBE: &str = "probe";

    /// A view as every window's: one root that fills the window.
    struct Probe {
        panel: bool,
    }

    impl Render for Probe {
        fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let content = div().debug_selector(|| PROBE.into()).size_full();
            if self.panel {
                framed_panel(PROBE_TITLE, content, window)
            } else {
                framed(PROBE_TITLE, content, window)
            }
        }
    }

    fn open(panel: bool, cx: &mut TestAppContext) -> gpui::WindowHandle<Probe> {
        cx.add_window(move |_, _| Probe { panel })
    }

    fn probe_bounds(
        window: gpui::WindowHandle<Probe>,
        cx: &mut TestAppContext,
    ) -> gpui::Bounds<gpui::Pixels> {
        VisualTestContext::from_window(window.into(), cx)
            .debug_bounds(PROBE)
            .expect("the view is drawn")
    }

    /// Presses `button` `count` times at `at` and lets go, as a click, a
    /// double click or a right click does.
    fn click(
        window: gpui::WindowHandle<Probe>,
        at: Point<gpui::Pixels>,
        button: MouseButton,
        count: usize,
        cx: &mut TestAppContext,
    ) {
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.simulate_mouse_move(at, None, Modifiers::default());
        for click_count in 1..=count {
            cx.simulate_event(MouseDownEvent {
                button,
                position: at,
                modifiers: Modifiers::default(),
                click_count,
                first_mouse: false,
            });
            cx.simulate_event(MouseUpEvent {
                button,
                position: at,
                modifiers: Modifiers::default(),
                click_count,
            });
        }
    }

    /// Clicks the control assistive technology calls `label`.
    fn click_control(
        window: gpui::WindowHandle<Probe>,
        shown: &Chrome,
        label: &str,
        cx: &mut TestAppContext,
    ) {
        let at = shown.control(label).bounds.center();
        click(window, at, MouseButton::Left, 1, cx);
    }

    #[gpui::test]
    fn server_side_decorations_add_nothing(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        for panel in [false, true] {
            let window = open(panel, cx);
            let shown = chrome(window.into(), cx);
            shown.assert_none();
            assert_eq!(probe_bounds(window, cx), shown.viewport);
            assert_eq!(title_heading(window, cx), None);
            assert!(!shown.focused);
        }
    }

    /// Under client-side decorations the window's first row is the bar:
    /// the title as a heading, then Minimize, Maximize and Close as named
    /// buttons, and nothing in it takes focus when the window opens.
    #[gpui::test]
    fn client_side_decorations_put_a_title_bar_above_the_view(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        report_client_decorations(cx);
        let window = open(false, cx);
        let shown = chrome(window.into(), cx);
        shown.assert_first();
        // The view still fills what the bar leaves.
        assert_eq!(Some(probe_bounds(window, cx)), shown.content);
        assert_eq!(title_heading(window, cx).as_deref(), Some(PROBE_TITLE));
        assert_eq!(
            shown.control_labels(),
            [MINIMIZE_LABEL, MAXIMIZE_LABEL, CLOSE_LABEL]
        );
        assert_eq!(
            shown.controls.iter().map(|c| c.id).collect::<Vec<_>>(),
            [MINIMIZE, MAXIMIZE, CLOSE]
        );
        assert!(!shown.focused, "the title bar took focus on open");
    }

    /// A tiling compositor may support neither minimizing nor maximizing:
    /// the bar then offers neither, and Close stays.
    #[gpui::test]
    fn the_bar_offers_only_the_controls_the_window_system_supports(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        let only = |minimize, maximize| WindowControls {
            minimize,
            maximize,
            fullscreen: false,
            window_menu: false,
        };
        for (supported, labels) in [
            (only(false, true), vec![MAXIMIZE_LABEL, CLOSE_LABEL]),
            (only(true, false), vec![MINIMIZE_LABEL, CLOSE_LABEL]),
            (only(false, false), vec![CLOSE_LABEL]),
        ] {
            report_client_decorations_with(cx, supported);
            let window = open(false, cx);
            let shown = chrome(window.into(), cx);
            shown.assert_first();
            assert_eq!(shown.control_labels(), labels, "{supported:?}");

            // Without maximize, a double click on the title does not zoom;
            // without a window menu, a right click opens none.
            let title = shown.title.unwrap().center();
            click(window, title, MouseButton::Left, 2, cx);
            click(window, title, MouseButton::Right, 1, cx);
            let requests = cx.window_requests(window.into());
            assert_eq!(requests.zooms, usize::from(supported.maximize));
            assert!(requests.window_menus.is_empty(), "{requests:?}");
        }
    }

    /// A window that may not be minimized or resized offers neither
    /// Minimize nor Maximize, even where the window system supports both;
    /// Close stays.
    #[gpui::test]
    fn the_bar_offers_only_the_controls_the_window_allows(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        report_client_decorations(cx);
        let window = cx.update(|cx| {
            let bounds = Bounds::maximized(None, cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    is_minimizable: false,
                    is_resizable: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Probe { panel: false }),
            )
            .unwrap()
        });
        let shown = chrome(window.into(), cx);
        shown.assert_first();
        assert_eq!(shown.control_labels(), [CLOSE_LABEL]);
        assert_eq!(
            shown.controls.iter().map(|c| c.id).collect::<Vec<_>>(),
            [CLOSE]
        );

        // Nor does a double click on the title zoom a window that cannot
        // be resized.
        let title = shown.title.unwrap().center();
        click(window, title, MouseButton::Left, 2, cx);
        assert_eq!(cx.window_requests(window.into()).zooms, 0);
    }

    /// Maximize maximizes the window; the same button then reads Restore
    /// and restores it.
    #[gpui::test]
    fn maximize_becomes_restore_while_the_window_is_maximized(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        report_client_decorations(cx);
        let window = open(false, cx);
        let shown = chrome(window.into(), cx);
        assert_eq!(shown.control(MAXIMIZE_LABEL).id, MAXIMIZE);

        click_control(window, &shown, MAXIMIZE_LABEL, cx);
        assert_eq!(cx.window_requests(window.into()).zooms, 1);
        let shown = chrome(window.into(), cx);
        assert_eq!(
            shown.control_labels(),
            [MINIMIZE_LABEL, RESTORE_LABEL, CLOSE_LABEL]
        );
        assert_eq!(shown.control(RESTORE_LABEL).id, MAXIMIZE);

        click_control(window, &shown, RESTORE_LABEL, cx);
        assert_eq!(cx.window_requests(window.into()).zooms, 2);
        let shown = chrome(window.into(), cx);
        assert_eq!(
            shown.control_labels(),
            [MINIMIZE_LABEL, MAXIMIZE_LABEL, CLOSE_LABEL]
        );

        // The window system maximizing the window by other means (a
        // keyboard shortcut, a screen edge) changes the button too.
        cx.simulate_window_maximized(window.into(), true);
        let shown = chrome(window.into(), cx);
        assert_eq!(shown.control(RESTORE_LABEL).id, MAXIMIZE);
    }

    #[gpui::test]
    fn minimize_asks_the_window_system_to_minimize(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        report_client_decorations(cx);
        let window = open(false, cx);
        let shown = chrome(window.into(), cx);
        click_control(window, &shown, MINIMIZE_LABEL, cx);
        let requests = cx.window_requests(window.into());
        assert_eq!(requests.minimizes, 1);
        assert_eq!((requests.zooms, requests.moves), (0, 0), "{requests:?}");
    }

    #[gpui::test]
    fn close_removes_the_window(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        report_client_decorations(cx);
        for (panel, label) in [(false, CLOSE_LABEL), (true, PANEL_CLOSE_LABEL)] {
            let window = open(panel, cx);
            let shown = chrome(window.into(), cx);
            click_control(window, &shown, label, cx);
            cx.run_until_parked();
            assert!(window.update(cx, |_, _, _| ()).is_err(), "still open");
        }
    }

    /// A double click on the title maximizes or restores the window, as a
    /// native title bar's does; a right click opens the window menu where
    /// the pointer is.
    #[gpui::test]
    fn the_title_zooms_on_a_double_click_and_opens_the_window_menu(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        report_client_decorations(cx);
        let window = open(false, cx);
        let title = chrome(window.into(), cx).title.unwrap().center();

        click(window, title, MouseButton::Left, 1, cx);
        assert_eq!(cx.window_requests(window.into()).zooms, 0, "a single click");
        click(window, title, MouseButton::Left, 2, cx);
        assert_eq!(cx.window_requests(window.into()).zooms, 1);
        assert_eq!(
            chrome(window.into(), cx).control_labels(),
            [MINIMIZE_LABEL, RESTORE_LABEL, CLOSE_LABEL]
        );

        click(window, title, MouseButton::Right, 1, cx);
        let requests = cx.window_requests(window.into());
        assert_eq!(requests.window_menus, [title]);
        assert_eq!(requests.moves, 0, "{requests:?}");
    }

    /// Pressing the title moves the window with the first motion that
    /// follows, once; a plain click, a motion without the press and a
    /// press on a control move nothing.
    #[gpui::test]
    fn dragging_the_title_moves_the_window_from_the_first_motion(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        report_client_decorations(cx);
        for panel in [false, true] {
            let window = open(panel, cx);
            let shown = chrome(window.into(), cx);
            let title = shown.title.unwrap().center();
            let beside = point(title.x + px(4.), title.y);
            let mut cx = VisualTestContext::from_window(window.into(), cx);
            let moves = |cx: &VisualTestContext| cx.window_requests(window.into()).moves;
            let none = Modifiers::default();

            cx.simulate_mouse_move(beside, None, none);
            cx.simulate_click(title, none);
            assert_eq!(moves(&cx), 0, "a click or a bare motion");

            cx.simulate_mouse_down(title, MouseButton::Left, none);
            assert_eq!(moves(&cx), 0, "the press alone");
            cx.simulate_mouse_move(beside, MouseButton::Left, none);
            assert_eq!(moves(&cx), 1, "the first motion");
            cx.simulate_mouse_move(title, MouseButton::Left, none);
            cx.simulate_mouse_up(title, MouseButton::Left, none);
            assert_eq!(moves(&cx), 1, "one move per press");

            // A press on Close that slides onto the title is no window move.
            let close = shown.controls.last().unwrap().bounds.center();
            cx.simulate_mouse_move(close, None, none);
            cx.simulate_mouse_down(close, MouseButton::Left, none);
            cx.simulate_mouse_move(beside, MouseButton::Left, none);
            assert_eq!(moves(&cx), 1, "a press on a control");
        }
    }

    /// A press low on the bar whose first motion already leaves it still
    /// moves the window, once: the bar follows an armed press over the
    /// whole window, not only while the pointer is over the bar.
    #[gpui::test]
    fn a_drag_whose_first_motion_leaves_the_bar_moves_the_window(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        report_client_decorations(cx);
        for panel in [false, true] {
            let window = open(panel, cx);
            let shown = chrome(window.into(), cx);
            let (bar, drag) = (shown.bar.unwrap(), shown.drag.unwrap());
            let low = point(drag.center().x, drag.bottom() - px(1.));
            let below = point(low.x, bar.bottom() + px(12.));
            let further = point(low.x + px(8.), bar.bottom() + px(40.));
            assert!(drag.contains(&low) && !bar.contains(&below), "{bar:?}");
            let mut cx = VisualTestContext::from_window(window.into(), cx);
            let moves = |cx: &VisualTestContext| cx.window_requests(window.into()).moves;
            let none = Modifiers::default();

            cx.simulate_mouse_move(low, None, none);
            cx.simulate_mouse_down(low, MouseButton::Left, none);
            cx.simulate_mouse_move(below, MouseButton::Left, none);
            assert_eq!(moves(&cx), 1, "panel: {panel}: the first motion");
            cx.simulate_mouse_move(further, MouseButton::Left, none);
            cx.simulate_mouse_up(further, MouseButton::Left, none);
            assert_eq!(moves(&cx), 1, "panel: {panel}: one move per press");
        }
    }

    /// A release before any motion ends the press wherever the pointer is:
    /// no motion after it moves the window, over the bar or below it.
    #[gpui::test]
    fn a_release_before_any_motion_moves_nothing(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        report_client_decorations(cx);
        for panel in [false, true] {
            let window = open(panel, cx);
            let shown = chrome(window.into(), cx);
            let title = shown.title.unwrap().center();
            let beside = point(title.x + px(4.), title.y);
            let content = shown.content.unwrap().center();
            let mut cx = VisualTestContext::from_window(window.into(), cx);
            let moves = |cx: &VisualTestContext| cx.window_requests(window.into()).moves;
            let none = Modifiers::default();

            // Released where it was pressed.
            cx.simulate_mouse_move(title, None, none);
            cx.simulate_mouse_down(title, MouseButton::Left, none);
            cx.simulate_mouse_up(title, MouseButton::Left, none);
            assert_eq!(moves(&cx), 0, "panel: {panel}: the release");
            cx.simulate_mouse_move(content, MouseButton::Left, none);
            cx.simulate_mouse_move(beside, MouseButton::Left, none);
            cx.simulate_mouse_move(content, None, none);
            assert_eq!(moves(&cx), 0, "panel: {panel}: a motion after the release");

            // Released below the bar, where the bar's own listeners do not
            // hear it.
            cx.simulate_mouse_move(title, None, none);
            cx.simulate_mouse_down(title, MouseButton::Left, none);
            cx.simulate_mouse_up(content, MouseButton::Left, none);
            cx.simulate_mouse_move(beside, MouseButton::Left, none);
            cx.simulate_mouse_move(content, MouseButton::Left, none);
            assert_eq!(moves(&cx), 0, "panel: {panel}: a release below the bar");
        }
    }

    /// The recording controls' bar has the title and Close only: never
    /// minimize or maximize (design §4), no zoom on a double click and no
    /// window menu, which would offer both.
    #[gpui::test]
    fn the_panel_bar_has_the_title_and_close_only(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        report_client_decorations(cx);
        let window = open(true, cx);
        let shown = chrome(window.into(), cx);
        shown.assert_first();
        assert_eq!(Some(probe_bounds(window, cx)), shown.content);
        assert_eq!(shown.control_labels(), [PANEL_CLOSE_LABEL]);
        assert_eq!(PANEL_CLOSE_LABEL, "Close recording controls");
        assert!(!shown.focused, "the bar took focus on open");
        assert_eq!(title_heading(window, cx).as_deref(), Some(PROBE_TITLE));

        let title = shown.title.unwrap().center();
        click(window, title, MouseButton::Left, 2, cx);
        click(window, title, MouseButton::Right, 1, cx);
        let requests = cx.window_requests(window.into());
        assert_eq!(requests.zooms, 0, "{requests:?}");
        assert!(requests.window_menus.is_empty(), "{requests:?}");
    }

    #[gpui::test]
    fn the_bar_wears_the_title_bar_tokens(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        let window = open(true, cx);
        window
            .update(cx, |_, window, cx| {
                let mut bar = bar_row(window, cx);
                let theme = cx.theme();
                let style = bar.style();
                assert_eq!(style.background, Some(Fill::from(theme.title_bar)));
                assert_eq!(style.border_color, Some(theme.title_bar_border));
                assert_eq!(style.text.color, Some(theme.foreground));
                assert_eq!(style.size.height, Some(TITLE_BAR_HEIGHT.into()));
            })
            .unwrap();
    }
}
