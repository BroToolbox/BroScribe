//! The BroScribe design system's components, built from GPUI elements and
//! the theme: badges, status lines, tag chips, confirmation boxes, the level
//! meter, the visor and the mascot. They only style; ids, roles and spoken
//! names stay with the callers (UI26: a state is always words and a role,
//! never colour alone). Pending lines update their elapsed time while visible.

use std::time::{Duration, Instant};

use broscribe_platform::DialogButtonOrder;
use broscribe_platform_native::NativePlatform;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::dialog::{Cancel, DialogEntrance};
use gpui::component::empty::Empty;
use gpui::component::progress::Progress;
use gpui::component::radio::Radio;
use gpui::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Context, Div, ElementId, FocusHandle, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Rems, RenderOnce, Role, SharedString,
    Stateful, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, img,
    px, rems, svg,
};

use super::Spoken as _;
use super::assets;
use super::brand::brand;
use super::type_scale::{TextRole, TextRoleExt as _};

pub const CONTROL_GAP: Rems = rems(0.5);
pub const GROUP_GAP: Rems = rems(0.75);

gpui::actions!(rows, [OpenRowMenu]);

/// A row's popup with the same anchored presentation for keyboard and pointer.
pub(crate) struct RowPopup {
    menu: gpui::Entity<gpui::component::menu::PopupMenu>,
    origin: gpui::Point<gpui::Pixels>,
    menu_focus: FocusHandle,
    opener: FocusHandle,
    _dismiss: Subscription,
}

impl RowPopup {
    pub(crate) fn open<V: 'static>(
        origin: gpui::Point<gpui::Pixels>,
        opener: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<V>,
        items: impl FnOnce(gpui::component::menu::PopupMenu) -> gpui::component::menu::PopupMenu,
        dismiss: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        use gpui::Focusable as _;

        let menu = gpui::component::menu::PopupMenu::build(window, cx, |menu, _, _| {
            items(menu.action_context(opener.clone()))
        });
        let subscription = cx.subscribe_in(
            &menu,
            window,
            move |view, _, _: &gpui::DismissEvent, window, cx| {
                dismiss(view, window, cx);
            },
        );
        let menu_focus = menu.focus_handle(cx);
        menu_focus.focus(window, cx);
        Self {
            menu,
            origin,
            menu_focus,
            opener: opener.clone(),
            _dismiss: subscription,
        }
    }

    pub(crate) fn restore_focus(self, window: &mut Window, cx: &mut App) {
        if self.menu_focus.contains_focused(window, cx) {
            self.opener.focus(window, cx);
        }
    }

    pub(crate) fn render<V: 'static>(
        &self,
        id: &'static str,
        cx: &Context<V>,
        dismiss: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> impl IntoElement {
        gpui::deferred(
            gpui::anchored()
                .position(self.origin)
                .snap_to_window_with_margin(px(8.))
                .child(
                    div()
                        .id(id)
                        .on_mouse_down_out(
                            cx.listener(move |view, _, window, cx| dismiss(view, window, cx)),
                        )
                        .child(self.menu.clone()),
                ),
        )
        .with_priority(gpui::base::POPUP_PRIORITY)
    }
}

pub fn stack() -> Div {
    div().flex().flex_col().items_start().gap(CONTROL_GAP)
}

/// A natural-width action bound to a view. Use Medium for view actions and
/// Small inside rows. Reserve primary for one action per region. Use a danger
/// outline to ask for confirmation and solid danger on the confirming action.
pub fn button<V: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    cx: &Context<V>,
    on_press: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> Button {
    disable_action(Button::new(id).label(label).self_start(), !enabled, cx).on_click(cx.listener(
        move |this, _, window, cx| {
            on_press(this, window, cx);
            cx.stop_propagation();
        },
    ))
}

/// The one primary action of a region: violet, carrying the primary shadow
/// in light (none in dark). Disabled, it is muted: the muted fill and the
/// secondary ink at full opacity, without the shadow, never a faded violet.
/// Pass the same `disabled` the button is given.
pub fn primary(button: Button, disabled: bool, cx: &App) -> Button {
    let theme = cx.theme();
    let (muted, label) = (theme.muted, theme.muted_foreground);
    button.primary().map(|button| {
        if disabled {
            button.bg(muted).border_color(muted).text_color(label)
        } else {
            button.shadow(brand(cx).primary_shadow())
        }
    })
}

/// The record dot a Start recording button carries before its label: the
/// recording red on a secondary button, the label's own colour on a primary
/// one (the muted label's when it is disabled). Decorative: the label says
/// what the button does.
pub fn record_dot(on_primary: bool, disabled: bool, cx: &App) -> Div {
    let theme = cx.theme();
    let colour = match (on_primary, disabled) {
        (true, false) => theme.primary_foreground,
        (true, true) => theme.muted_foreground,
        (false, _) => brand(cx).recording,
    };
    div()
        .debug_selector(|| "record-dot".into())
        .flex_none()
        .size(rems(0.5))
        .rounded_full()
        .bg(colour)
}

/// Keep an inert button in keyboard traversal so its accessibility
/// description is discoverable. The pinned component skips disabled buttons
/// unless GPUI's element-level tab stop is set explicitly.
pub fn disable_action(button: Button, disabled: bool, cx: &App) -> Button {
    button.disabled(disabled).when(disabled, |button| {
        gpui::InteractiveElement::tab_index(button, 0).focus_visible(focus_halo(cx.theme().ring))
    })
}

/// Preserve a disabled action's explanation on hover and in its accessible description.
pub fn disable_with_reason(
    button: Button,
    disabled: bool,
    reason: impl Into<SharedString>,
    cx: &App,
) -> Button {
    disable_action(button, disabled, cx).when(disabled, |button| button.tooltip(reason))
}

type ChooseRadio = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

pub(crate) struct ChoiceOption {
    id: ElementId,
    label: SharedString,
    checked: bool,
    accessibility_label: Option<SharedString>,
    disabled: bool,
    focus: Option<FocusHandle>,
    choose: ChooseRadio,
    trailing: Option<AnyElement>,
    details: Option<AnyElement>,
}

impl ChoiceOption {
    pub(crate) fn details(mut self, element: impl IntoElement) -> Self {
        self.details = Some(element.into_any_element());
        self
    }

    pub(crate) fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing = Some(element.into_any_element());
        self
    }

    pub(crate) fn accessibility_label(mut self, label: impl Into<SharedString>) -> Self {
        self.accessibility_label = Some(label.into());
        self
    }

    pub(crate) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub(crate) fn track_focus(mut self, focus: &FocusHandle) -> Self {
        self.focus = Some(focus.clone());
        self
    }
}

pub(crate) fn choice(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
    choose: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> ChoiceOption {
    ChoiceOption {
        id: id.into(),
        label: label.into(),
        checked,
        accessibility_label: None,
        disabled: false,
        focus: None,
        choose: std::rc::Rc::new(move |window, cx| choose(&true, window, cx)),
        trailing: None,
        details: None,
    }
}

pub(crate) fn radio_option<V: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
    enabled: bool,
    cx: &Context<V>,
    choose_option: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> ChoiceOption {
    choice(
        id,
        label,
        checked,
        cx.listener(move |this, _: &bool, window, cx| {
            choose_option(this, window, cx);
        }),
    )
    .disabled(!enabled)
}

#[derive(IntoElement)]
pub(crate) struct ChoiceGroup {
    id: ElementId,
    group: Stateful<Div>,
    label: SharedString,
    options: Vec<ChoiceOption>,
    fit_options: bool,
}

impl ChoiceGroup {
    pub(crate) fn new(
        layout: Div,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
    ) -> Self {
        let id = id.into();
        let label = label.into();
        Self {
            group: layout
                .id(id.clone())
                .role(Role::RadioGroup)
                .aria_label(label.clone()),
            id,
            label,
            options: Vec::new(),
            fit_options: false,
        }
    }

    /// Give every option room in one row, allowing its label to wrap.
    pub(crate) fn fit_options(mut self) -> Self {
        self.fit_options = true;
        self
    }

    pub(crate) fn option(mut self, option: ChoiceOption) -> Self {
        self.options.push(option);
        self
    }
}

impl RenderOnce for ChoiceGroup {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        if self.options.len() > 5 {
            return self.long_list().into_any_element();
        }
        let selected = self.options.iter().position(|option| option.checked);
        let tab_stop = selected
            .filter(|&index| !self.options[index].disabled)
            .or_else(|| self.options.iter().position(|option| !option.disabled));
        let size = self.options.len();
        let mut enabled = Vec::with_capacity(size);
        // Construct each component in the group's namespace. The Radio and
        // the arrow handler use the same keyed focus handle for that option.
        let radios = window.with_element_namespace(self.id, |window| {
            self.options
                .into_iter()
                .enumerate()
                .map(|(index, option)| {
                    let focus = window
                        .use_keyed_state(option.id.clone(), cx, |_, cx| {
                            option.focus.unwrap_or_else(|| cx.focus_handle())
                        })
                        .read(cx)
                        .clone();
                    if !option.disabled {
                        enabled.push((index, focus, option.choose.clone()));
                    }
                    let radio = Radio::new(option.id)
                        .label(option.label)
                        .when(self.fit_options, |radio| radio.flex_1().min_w_0())
                        .children(option.details)
                        .checked(option.checked)
                        .when_some(option.accessibility_label, |radio, label| {
                            radio.accessibility_label(label)
                        })
                        .disabled(option.disabled)
                        .tab_stop(tab_stop == Some(index))
                        .aria_position_in_set(index + 1)
                        .aria_size_of_set(size)
                        .on_change(move |checked, window, cx| {
                            if *checked {
                                (option.choose)(window, cx);
                            }
                        })
                        .render(window, cx)
                        .into_any_element();
                    if let Some(trailing) = option.trailing {
                        div()
                            .flex()
                            .items_center()
                            .gap(CONTROL_GAP)
                            .child(radio)
                            .child(trailing)
                            .into_any_element()
                    } else {
                        radio
                    }
                })
                .collect::<Vec<_>>()
        });
        let group = self
            .group
            .on_key_down(move |event, window, cx| {
                radio_keys(event, &enabled, tab_stop, window, cx);
            })
            .children(radios);
        #[cfg(test)]
        let group = gpui::base::TestSupportExt::test_support(group);
        group.into_any_element()
    }
}

impl ChoiceGroup {
    fn long_list(self) -> super::select::Picker {
        use super::select::{OptionItem, Picker};

        let selected = self
            .options
            .iter()
            .find(|option| option.checked)
            .map(|option| option.id.to_string())
            .unwrap_or_default();
        let items = self
            .options
            .iter()
            .map(|option| {
                let mut item = OptionItem::new(option.id.to_string(), option.label.clone());
                item.disabled = option.disabled;
                item
            })
            .collect();
        let disabled = self.options.iter().all(|option| option.disabled);
        Picker::with_handler(
            self.id,
            self.label,
            items,
            selected,
            std::rc::Rc::new(move |value, window, cx| {
                if let Some(option) = self
                    .options
                    .iter()
                    .find(|option| !option.disabled && option.id.to_string() == value)
                {
                    (option.choose)(window, cx);
                }
            }),
        )
        .disabled(disabled)
    }
}

fn radio_keys(
    event: &gpui::KeyDownEvent,
    options: &[(usize, FocusHandle, ChooseRadio)],
    tab_stop: Option<usize>,
    window: &mut Window,
    cx: &mut App,
) {
    if event.keystroke.modifiers.modified() || options.len() < 2 {
        return;
    }
    let step = match event.keystroke.key.as_str() {
        "left" | "up" => options.len() - 1,
        "right" | "down" => 1,
        _ => return,
    };
    let Some(current) = options
        .iter()
        .position(|(_, focus, _)| focus.is_focused(window))
    else {
        return;
    };
    let (next, focus, choose) = &options[(current + step) % options.len()];
    focus.focus(window, cx);
    if Some(*next) != tab_stop {
        choose(window, cx);
    }
    cx.stop_propagation();
}

/// Keyboard focus treatment for custom focusable elements. GPUI components
/// apply their own theme-backed ring.
pub fn focus_halo(ring: Hsla) -> impl Fn(gpui::StyleRefinement) -> gpui::StyleRefinement {
    move |style| {
        style.border_color(ring).shadow(vec![
            gpui::BoxShadow::new(px(0.), px(0.), ring.opacity(0.5)).spread_radius(px(3.)),
        ])
    }
}

pub fn title_tooltip(
    title: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let title = title.into();
    move |window, cx| gpui::component::tooltip::Tooltip::new(title.clone()).build(window, cx)
}

/// A named GPUI tab list with the shared selection and focus styles.
pub fn tabs(
    id: impl Into<ElementId>,
    name: impl Into<SharedString>,
    labels: impl IntoIterator<Item = SharedString>,
    selected: usize,
    on_select: impl Fn(&usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> gpui::base::Tabs {
    tabs_with_names(
        id,
        name,
        labels.into_iter().map(|label| (label.clone(), label)),
        selected,
        on_select,
        cx,
    )
}

pub fn tabs_with_names(
    id: impl Into<ElementId>,
    name: impl Into<SharedString>,
    labels: impl IntoIterator<Item = (SharedString, SharedString)>,
    selected: usize,
    on_select: impl Fn(&usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> gpui::base::Tabs {
    tabs_with_content(id, name, labels, selected, on_select, cx)
}

pub(crate) fn tabs_with_content<E: IntoElement>(
    id: impl Into<ElementId>,
    name: impl Into<SharedString>,
    content: impl IntoIterator<Item = (E, SharedString)>,
    selected: usize,
    on_select: impl Fn(&usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> gpui::base::Tabs {
    let on_select = std::rc::Rc::new(on_select);
    let theme = cx.theme();
    let selected_fill = theme.list_active;
    gpui::base::Tabs::new(id)
        .aria_label(name)
        .flex()
        .flex_wrap()
        .gap_1()
        .children(
            content
                .into_iter()
                .enumerate()
                .map(|(index, (content, name))| {
                    let on_select = on_select.clone();
                    gpui::base::Tab::new(("tab", index))
                        .accessibility_label(name)
                        .selected(index == selected)
                        .px_3()
                        .py_1()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(gpui::transparent_black())
                        .styles(|styles| styles.selected(|style| style.bg(selected_fill)))
                        .focus_visible(focus_halo(cx.theme().ring))
                        .child(content)
                        .on_click(move |_, window, cx| on_select(&index, window, cx))
                }),
        )
}

/// A card on the desk: every block of content (color.css "Desk and
/// cards"). The card ground, a 1px border, the 16px card radius and the card
/// shadow (soft in light, a 1px line in dark). No padding: callers pad or
/// put strips edge to edge. A card grows with its content; never give one a
/// fixed height for text.
pub fn desk_card(id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    as_card(div().id(id).flex().flex_col(), cx)
}

/// Gives an existing element the card's ground, edge, radius and shadow, so
/// a pane keeps its ID, role and focus while it becomes a card.
pub fn as_card<E: gpui::Styled>(element: E, cx: &App) -> E {
    let colours = brand(cx);
    element
        .bg(colours.card)
        .border_1()
        .border_color(cx.theme().border)
        .rounded(super::brand::RADIUS_CARD)
        .shadow(colours.card_shadow())
}

/// Gives an existing element the inset look: a box inside a card.
pub fn as_inset<E: gpui::Styled>(element: E, cx: &App) -> E {
    element
        .bg(brand(cx).surface_inset)
        .rounded(cx.theme().radius_lg)
}

/// What an AI provider wrote, apart from the transcript: an inset box with
/// a 1px edge and a 3px violet rule on its leading edge. The caller keeps
/// the caution line that says, in words, that an AI wrote it
/// ([`super::brand::Brand::ai_foreground`]).
pub fn as_ai_result<E: gpui::Styled + gpui::ParentElement>(element: E, cx: &App) -> E {
    let theme = cx.theme();
    let rule = brand(cx).ai_border;
    let radius = theme.radius_lg;
    as_inset(element, cx)
        .relative()
        .border_1()
        .border_color(theme.border)
        .py_3()
        .pr_3()
        .pl_4()
        .child(
            div()
                .debug_selector(|| "ai-rule".into())
                .absolute()
                .left(px(-1.))
                .top(px(-1.))
                .bottom(px(-1.))
                .w(px(3.))
                .rounded_l(radius)
                .bg(rule),
        )
}

/// A box inside a card: the desk colour, the smaller radius, no shadow.
pub fn inset(cx: &App) -> Div {
    as_inset(div(), cx)
}

/// A padded box inside a card, for a passage, a sample or a comparison.
pub fn inset_box(id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    as_inset(div().id(id).flex().flex_col().gap_2().p_4(), cx)
}

/// How far cards sit from the window edge.
pub const DESK_PADDING: Rems = rems(1.5);

/// A padded card for a standalone form, group or result.
pub fn card(id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    desk_card(id, cx).gap_2().p_4()
}

/// A sidebar item: the selected one is a card-coloured pill with the pill
/// shadow; the others are quiet and take the muted hover. A focused item
/// (the keyboard cursor) carries the focus ring. The caller marks the icon
/// with [`sidebar_icon_colour`].
pub fn sidebar_item(row: Stateful<Div>, selected: bool, focused: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let colours = brand(cx);
    let hover = theme.secondary_hover;
    let active = theme.secondary_active;
    let ring = theme.list_active_border;
    let radius = theme.radius;
    row.relative()
        .rounded(radius)
        .aria_selected(selected)
        .text_color(theme.foreground)
        .when(!selected, |row| {
            row.hover(move |style| style.bg(hover))
                .active(move |style| style.bg(active))
        })
        .when(selected, |row| {
            row.bg(colours.sidebar_selected)
                .shadow(colours.pill_shadow())
        })
        .when(focused, |row| {
            row.child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(radius)
                    .border_2()
                    .border_color(ring),
            )
        })
}

/// A sidebar icon's colour: violet for the current item, secondary ink for
/// the rest.
pub fn sidebar_icon_colour(selected: bool, cx: &App) -> Hsla {
    if selected {
        cx.theme().primary
    } else {
        cx.theme().muted_foreground
    }
}

/// Selection belongs to the row even when a descendant has focus. A
/// selected row is marked by more than its tint: a 3px violet bar on its
/// leading edge, and a 2px inset border while the list has focus.
pub fn selected_row(
    row: Stateful<Div>,
    selected: bool,
    list_focused: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let active = theme.list_active;
    let foreground = theme.foreground;
    let border = theme.list_active_border;
    // Violet, or the accent colour on macOS (HIG: selection follows it).
    let bar = brand(cx).selection_mark;
    row.relative()
        .aria_selected(selected)
        .when(!selected, |row| {
            let hover = theme.list_hover;
            row.hover(move |style| style.bg(hover))
        })
        .active(move |style| style.bg(active))
        .when(selected, |row| {
            row.bg(active)
                .text_color(foreground)
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(theme.radius)
                        .when(list_focused, |line| line.border_2().border_color(border)),
                )
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_1()
                        .bottom_1()
                        .w(px(3.))
                        .rounded_full()
                        .bg(bar),
                )
        })
}

/// A full-width row with a stable minimum height and three reusable slots.
pub fn list_row(
    id: impl Into<ElementId>,
    selected: bool,
    list_focused: bool,
    cx: &App,
) -> Stateful<Div> {
    selected_row(
        div()
            .id(id)
            .w_full()
            .min_h(rems(2.))
            .flex()
            .gap(GROUP_GAP)
            .rounded(cx.theme().radius)
            .px_2(),
        selected,
        list_focused,
        cx,
    )
}

pub fn row_leading(width: Rems, content: impl IntoElement) -> Div {
    div().w(width).flex_none().overflow_hidden().child(content)
}

pub fn row_body(
    title: impl Into<SharedString>,
    detail: Option<impl Into<SharedString>>,
    selected: bool,
    cx: &App,
) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .child(
            div()
                .truncate()
                .text_role(TextRole::Label)
                .when(selected, |line| line.font_weight(FontWeight::SEMIBOLD))
                .child(title.into()),
        )
        .when_some(detail, |body, detail| {
            body.min_h(rems(3.5))
                .flex()
                .flex_col()
                .justify_center()
                .child(
                    div()
                        .truncate()
                        .text_role(TextRole::Caption)
                        .text_color(row_metadata_colour(selected, cx))
                        .child(detail.into()),
                )
        })
}

pub fn row_trailing(content: impl IntoElement) -> Div {
    div()
        .flex_none()
        .flex()
        .justify_end()
        .text_right()
        .child(content)
}

pub fn row_metadata_colour(selected: bool, cx: &App) -> Hsla {
    if selected {
        cx.theme().foreground
    } else {
        cx.theme().muted_foreground
    }
}

pub fn section(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    first: bool,
    content: Div,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(CONTROL_GAP)
        .when(!first, |section| section.mt_6())
        .child(super::heading(id, title))
        .child(content.flex().flex_col().gap(GROUP_GAP))
}

/// The label is supplied to the control constructor for its accessible name.
pub fn field<C: IntoElement>(
    label_id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    help: Option<(ElementId, SharedString)>,
    control: impl FnOnce(SharedString) -> C,
    cx: &App,
) -> Div {
    let label = label.into();
    let visible = super::text_line(label_id, Role::Label, label.clone()).text_role(TextRole::Label);
    field_layout(visible, help, control(label), cx)
}

/// A setting group that is useful in heading navigation. Its control still
/// receives the same label as its accessible name.
pub(crate) fn field_heading<C: IntoElement>(
    label_id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    help: Option<(ElementId, SharedString)>,
    control: impl FnOnce(SharedString) -> C,
    cx: &App,
) -> Div {
    let label = label.into();
    field_layout(
        super::subheading(label_id, label.clone()),
        help,
        control(label),
        cx,
    )
}

fn field_layout(
    label: Stateful<Div>,
    help: Option<(ElementId, SharedString)>,
    control: impl IntoElement,
    cx: &App,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(label)
        .child(control)
        .when_some(help, |field, (id, text)| {
            field.child(
                super::text_line(id, Role::Label, text)
                    .text_role(TextRole::Caption)
                    .text_color(cx.theme().muted_foreground),
            )
        })
}

/// A setting with a trailing control. Narrow layouts wrap at the label's
/// minimum width. Large text can put every control below its caption.
pub fn field_row<C: IntoElement>(
    label_id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    help: Option<(ElementId, SharedString)>,
    vertical: bool,
    control: impl FnOnce(SharedString) -> C,
    cx: &App,
) -> Div {
    let label = label.into();
    let leading = div()
        .flex()
        .flex_col()
        .gap_1()
        .when_else(
            vertical,
            |leading| leading.w_full().min_w_0().flex_none(),
            |leading| leading.flex_1().min_w(rems(15.)),
        )
        .child(super::text_line(label_id, Role::Label, label.clone()).text_role(TextRole::Label))
        .when_some(help, |field, (id, text)| {
            let help = super::text_line(id, Role::Label, text)
                .text_role(TextRole::Caption)
                .text_color(cx.theme().muted_foreground);
            #[cfg(test)]
            let help = gpui::base::TestSupportExt::test_support(help);
            field.child(help)
        });
    div()
        .flex()
        .gap(CONTROL_GAP)
        .w_full()
        .when(vertical, |row| row.flex_col().items_start())
        .when(!vertical, |row| row.flex_wrap().items_center())
        .child(leading)
        .child(div().flex_none().max_w_full().child(control(label)))
}

pub const EMPTY_MASCOT_SIZE: f32 = 128.;
pub const ABOUT_TILE_SIZE: f32 = 64.;

/// The first action is the region's primary. A second action is an outline.
pub fn empty_state(
    title_id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    body: Stateful<Div>,
    show_mascot: bool,
    primary: Option<Button>,
    secondary: Option<Button>,
    cx: &App,
) -> Empty {
    Empty::new()
        .gap(CONTROL_GAP)
        .p_4()
        .border_0()
        .when(show_mascot, |el| {
            el.child(mascot(assets::MASCOT_IDLE, EMPTY_MASCOT_SIZE))
        })
        .child(super::heading(title_id, title))
        .child(
            body.max_w(rems(28.))
                .text_role(TextRole::Body)
                .text_color(cx.theme().muted_foreground),
        )
        .when(primary.is_some() || secondary.is_some(), |el| {
            el.child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_center()
                    .gap(CONTROL_GAP)
                    .children(primary.map(|button| self::primary(button, false, cx)))
                    .children(secondary.map(Button::outline)),
            )
        })
}

pub fn disclosure<V: 'static>(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    open: bool,
    cx: &Context<V>,
    toggle: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> Button {
    use super::icons;
    button(id, title, true, cx, toggle)
        .ghost()
        .icon(icons::icon(if open {
            icons::DISCLOSURE_OPEN
        } else {
            icons::DISCLOSURE_CLOSED
        }))
        .aria_expanded(open)
}

/// Who a disc stands for. Only the disc carries a person's colour; names
/// stay in the foreground ink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Disc {
    /// A known person: a solid disc in their stored hue (1 to 7).
    Person(u8),
    /// You: a slate disc, so it never reads as a button.
    Me,
    /// A suggested person, not confirmed: a transparent dashed ring.
    Suggested,
    /// A voice nobody named: a dashed ring, with its letter where it has one.
    Voice(Option<char>),
}

impl Disc {
    /// The disc for `person`, or for an unnamed voice when there is none.
    pub(crate) fn for_person(person: Option<&broscribe_core::schema::Person>) -> Self {
        match person {
            Some(person) if person.is_self => Self::Me,
            Some(person) => Self::Person(person.hue()),
            None => Self::Voice(None),
        }
    }
}

/// The disc's ground, initials colour and whether its edge is dashed.
fn disc_style(disc: Disc, cx: &App) -> (Option<Hsla>, Hsla, bool) {
    let colours = brand(cx);
    match disc {
        Disc::Person(hue) => (Some(colours.person_hue(hue)), colours.on_person, false),
        Disc::Me => (Some(colours.me), colours.on_person, false),
        Disc::Suggested | Disc::Voice(_) => (None, cx.theme().foreground, true),
    }
}

/// Gives `element` a disc's ground, ring and initials colour.
fn as_disc(element: Div, disc: Disc, cx: &App) -> Div {
    let (ground, ink, dashed) = disc_style(disc, cx);
    element
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(ink)
        .when_some(ground, |el, ground| el.bg(ground))
        .when(dashed, |el| {
            el.border_1().border_dashed().border_color(cx.theme().input)
        })
}

/// A decorative photo, or a disc with initials (a letter for an unnamed
/// voice). The surrounding row names the person. Initials use the Caption
/// role, which stays above 10 px at every text size.
pub(crate) fn avatar(
    name: &str,
    disc: Disc,
    photo: Option<&std::sync::Arc<gpui::Image>>,
    size: f32,
    cx: &App,
) -> Div {
    use gpui::StyledImage as _;
    let frame = div().size(gpui::px(size)).overflow_hidden();
    if let Some(photo) = photo {
        return frame.flex_none().rounded_full().child(
            gpui::img(photo.clone())
                .size_full()
                .object_fit(gpui::ObjectFit::Cover),
        );
    }
    let initials: String = match disc {
        Disc::Voice(letter) => letter.map(String::from).unwrap_or_default(),
        _ => name
            .split_whitespace()
            .take(2)
            .filter_map(|word| word.chars().next())
            .flat_map(char::to_uppercase)
            .collect(),
    };
    as_disc(frame, disc, cx)
        .text_role(TextRole::Caption)
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(initials)
}

/// The 1 rem disc before a speaker's name in a transcript: colour only, no
/// initials (the name beside it says who).
pub(crate) fn speaker_disc(disc: Disc, cx: &App) -> Div {
    as_disc(div().size(rems(1.)), disc, cx)
}

/// How a badge reads beside a transcript line or a speaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeTone {
    /// A neutral fact: *Voice 2*, *corrected*, a participant.
    Neutral,
    /// Settled by the user: *Named by you · Ana Example*. Done teal with a
    /// check, not the primary violet, so it never looks like a button.
    Strong,
    /// Offered, not settled: *Suggested · Ana Example*. Dashed, like every
    /// unconfirmed mark.
    Suggested,
    /// A decision needs attention, or a line is undecided: *Unresolved*.
    /// The warning-soft fill with the attention triangle.
    Attention,
    /// Provisional: *Preview*.
    Outline,
}

/// A small pill of words (decorative: the caller's row already carries the
/// words as its name, so a badge adds no node of its own).
pub fn badge(tone: BadgeTone, text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    let colours = brand(cx);
    let (bg, fg, border) = match tone {
        BadgeTone::Neutral => (theme.muted, theme.foreground, theme.muted),
        BadgeTone::Strong => (
            colours.success_soft,
            colours.success,
            colours.success_border,
        ),
        BadgeTone::Suggested => (gpui::transparent_black(), theme.foreground, theme.input),
        BadgeTone::Attention => (colours.warning_soft, theme.warning, theme.warning),
        BadgeTone::Outline => (theme.background, theme.muted_foreground, theme.input),
    };
    div()
        .flex_none()
        .max_w_full()
        .overflow_hidden()
        .min_h(rems(1.25))
        .py_0p5()
        .px_2()
        .flex()
        .items_center()
        .rounded_full()
        .border_1()
        .when(
            matches!(tone, BadgeTone::Outline | BadgeTone::Suggested),
            |badge| badge.border_dashed(),
        )
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .text_role(TextRole::Micro)
        .gap_1()
        .when(tone == BadgeTone::Strong, |badge| {
            badge.child(super::icons::icon(super::icons::DONE).size(rems(0.75)))
        })
        .when(tone == BadgeTone::Attention, |badge| {
            badge.child(
                svg()
                    .path(StatusKind::Attention.glyph_path())
                    .flex_none()
                    .size(rems(0.75))
                    .text_color(fg),
            )
        })
        .child(div().min_w_0().truncate().child(text.into()))
}

/// A sidebar count of decisions waiting (the Review count): amber with dark
/// text, semibold. Decorative like [`badge`]: the item's name says the count.
pub fn count_badge(count: usize, cx: &App) -> Div {
    let colours = brand(cx);
    badge(BadgeTone::Neutral, count.to_string(), cx)
        .border_color(colours.badge)
        .bg(colours.badge)
        .text_color(colours.badge_foreground)
        .font_weight(FontWeight::SEMIBOLD)
}

/// What a status line says about the control beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    /// A neutral note about the current view.
    Info,
    /// Waiting for an answer (dashed ring).
    Pending,
    /// Done (check).
    Done,
    /// Needs a decision, nothing lost (a triangle with `!`, warning colour).
    Attention,
    /// Failed (a circle with ×, danger colour); announced as an alert.
    Failed,
    /// Paused (two bars).
    Paused,
}

impl StatusKind {
    /// The role the line is announced with.
    pub fn role(self) -> Role {
        match self {
            Self::Failed => Role::Alert,
            _ => Role::Status,
        }
    }

    pub(crate) fn glyph_path(self) -> &'static str {
        match self {
            Self::Info => "icons/info.svg",
            Self::Pending => assets::STATUS_PENDING,
            Self::Done => assets::STATUS_DONE,
            Self::Attention => assets::STATUS_ATTENTION,
            Self::Failed => assets::STATUS_FAILED,
            Self::Paused => assets::STATUS_PAUSED,
        }
    }

    fn colours(self, cx: &App) -> (Hsla, Hsla) {
        let theme = cx.theme();
        match self {
            Self::Info | Self::Pending | Self::Paused => {
                (theme.muted_foreground, theme.muted_foreground)
            }
            Self::Done => (theme.success, theme.foreground),
            Self::Attention => (theme.warning, theme.warning),
            Self::Failed => (theme.danger, theme.danger),
        }
    }
}

/// A one-rem single-ink glyph from the brand files, painted in `colour`.
pub fn glyph(path: &'static str, colour: Hsla) -> impl IntoElement {
    svg()
        .path(path)
        .flex_none()
        .size(rems(1.))
        .text_color(colour)
}

/// A one-line answer beside a control: its glyph and words, named by the
/// words with `kind`'s role (`Status`, or `Alert` for a failure).
pub fn status_line(
    id: impl Into<ElementId>,
    kind: StatusKind,
    text: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    status_line_role(id, kind, kind.role(), text, cx)
}

pub(crate) fn start_recovery_button(
    id: impl Into<ElementId>,
    refusal: &crate::daemon::StartRefusal,
    state: &gpui::Entity<crate::state::AppState>,
) -> Option<Button> {
    use crate::state::{AppEvent, SettingsPage};
    use broscribe_ipc::recording_words::StartRecovery;
    let (label, event) = match refusal.recovery? {
        StartRecovery::Storage => (
            "Open Storage settings",
            Some(AppEvent::OpenSettings(Some(SettingsPage::Storage))),
        ),
        StartRecovery::Setup => ("Show setup assistant", Some(AppEvent::ShowSetup)),
        StartRecovery::Updates => (
            "Open update settings",
            Some(AppEvent::OpenSettings(Some(SettingsPage::Updates))),
        ),
        StartRecovery::Recording => (
            "Open Recording settings",
            Some(AppEvent::OpenSettings(Some(SettingsPage::Recording))),
        ),
        StartRecovery::MicrophonePrivacy => (
            "Open Windows Settings",
            Some(AppEvent::OpenMicrophonePrivacy),
        ),
        StartRecovery::CopyDetails => ("Copy details", None),
    };
    let state = state.clone();
    let code = refusal.code.clone();
    Some(
        Button::new(id)
            .label(label)
            .outline()
            .small()
            .on_click(move |_, _, cx| {
                if let Some(event) = &event {
                    state.update(cx, |_, cx| cx.emit(event.clone()));
                } else {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(format!(
                        "Recording start failed ({code})."
                    )));
                }
            }),
    )
}

pub(crate) fn capture_access_warnings<'a>(
    access: broscribe_ipc::CaptureAccess,
    recording: Option<broscribe_ipc::RecordingStatus>,
    state: &'a gpui::Entity<crate::state::AppState>,
    cx: &'a App,
) -> impl Iterator<Item = Stateful<Div>> + 'a {
    use crate::state::AppEvent;
    use broscribe_ipc::{RecordingStatus, Track};
    [Track::Mic, Track::System]
        .into_iter()
        .filter(move |track| access.denied(*track))
        .map(move |track| {
            let (control, agent, name, event) = match track {
                Track::Mic => (
                    "Microphone access",
                    "can't use the microphone",
                    "Me",
                    AppEvent::OpenMicrophonePrivacy,
                ),
                Track::System => (
                    "System audio access",
                    "can't record system audio",
                    "Others",
                    AppEvent::OpenSystemAudioPrivacy,
                ),
            };
            let text = match recording {
                Some(RecordingStatus::Recording) => {
                    format!("BroScribe Agent {agent}, so {name} is recording silence.")
                }
                Some(RecordingStatus::Paused) => format!(
                    "BroScribe Agent {agent}, so {name} will record silence when you resume."
                ),
                None => {
                    format!("{control} is off for BroScribe Agent, so {name} would record silence.")
                }
            };
            let state = state.clone();
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(status_line_role(
                    ("capture-access", track.index()),
                    StatusKind::Attention,
                    if recording.is_some() {
                        Role::Alert
                    } else {
                        Role::Status
                    },
                    text,
                    cx,
                ))
                .child(
                    Button::new(("open-capture-privacy", track.index()))
                        .label("Open System Settings")
                        .accessibility_label(format!("Open System Settings for {control}"))
                        .outline()
                        .small()
                        .on_click(move |_, _, cx| state.update(cx, |_, cx| cx.emit(event.clone()))),
                )
                .id(("capture-access-row", track.index()))
        })
}

/// An interrupting warning can use Alert while keeping the warning glyph
/// and colour. Its countdown belongs in a separate non-live timer.
pub fn status_line_role(
    id: impl Into<ElementId>,
    kind: StatusKind,
    role: Role,
    text: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    status_words(id, kind, role, text, false, None, cx)
}

/// A static status, or a wait with its own clock or progress indicator.
pub fn status_without_wait_clock(
    id: impl Into<ElementId>,
    kind: StatusKind,
    text: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    status_words(id, kind, kind.role(), text, true, None, cx)
}

fn status_words(
    id: impl Into<ElementId>,
    kind: StatusKind,
    role: Role,
    text: impl Into<SharedString>,
    external_clock: bool,
    mark: Option<Hsla>,
    cx: &App,
) -> Stateful<Div> {
    let text = text.into();
    let (kind_mark, words) = kind.colours(cx);
    let mark = mark.unwrap_or(kind_mark);
    div()
        .id(id)
        .spoken(role, text.clone())
        .flex()
        .flex_wrap()
        .items_start()
        .min_w_0()
        .max_w_full()
        .gap_1()
        .text_role(TextRole::Caption)
        .text_color(words)
        .child(glyph(kind.glyph_path(), mark))
        .map(|el| {
            if kind == StatusKind::Failed {
                el.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(gpui::base::SelectableText::new("status-copy", text)),
                )
            } else {
                el.child(div().flex_1().min_w_0().whitespace_normal().child(text))
            }
        })
        .when(kind == StatusKind::Pending && !external_clock, |el| {
            el.child(PendingElapsed)
        })
}

const SHOW_WAIT_AFTER: Duration = Duration::from_secs(2);

#[derive(IntoElement)]
struct PendingElapsed;

struct WaitClock {
    started: Instant,
    ticker: Option<Task<()>>,
    line_visible: bool,
    window_visible: bool,
    _visibility: Subscription,
}

impl WaitClock {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut clock = Self {
            started: cx.background_executor().now(),
            ticker: None,
            line_visible: true,
            window_visible: window.is_visible(),
            _visibility: cx.observe_window_visibility(window, |clock, visibility, _, cx| {
                clock.window_visible = visibility.is_visible();
                clock.sync_ticker(cx);
                if clock.window_visible {
                    cx.notify();
                }
            }),
        };
        clock.sync_ticker(cx);
        clock
    }

    fn sync_ticker(&mut self, cx: &mut Context<Self>) {
        if !self.window_visible || !self.line_visible {
            self.ticker = None;
        } else if self.ticker.is_none() {
            let elapsed = cx
                .background_executor()
                .now()
                .saturating_duration_since(self.started);
            let mut delay = SHOW_WAIT_AFTER
                .checked_sub(elapsed)
                .filter(|remaining| !remaining.is_zero())
                .unwrap_or(Duration::from_secs(1));
            self.ticker = Some(cx.spawn(async move |clock, cx| {
                loop {
                    cx.background_executor().timer(delay).await;
                    if clock.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                    delay = Duration::from_secs(1);
                }
            }));
        }
    }
}

impl RenderOnce for PendingElapsed {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let clock = window.use_keyed_state("pending-clock", cx, WaitClock::new);
        let elapsed = cx
            .background_executor()
            .now()
            .saturating_duration_since(clock.read(cx).started);
        // The anchor lays out beside the wait text even before the timer
        // appears. Stop updating elapsed time if that line is scrolled out.
        let anchor = gpui::canvas(
            move |bounds, window, cx| {
                let clip = window.content_mask().bounds;
                let visible = bounds.origin.y >= clip.top() && bounds.origin.y < clip.bottom();
                clock.update(cx, |clock, cx| {
                    if clock.line_visible != visible {
                        clock.line_visible = visible;
                        clock.sync_ticker(cx);
                    }
                });
            },
            |_, _, _, _| {},
        );
        div()
            .relative()
            .min_w_0()
            .max_w_full()
            .child(anchor.absolute().top_0().left_0())
            .when(elapsed >= SHOW_WAIT_AFTER, |el| {
                let seconds = elapsed.as_secs();
                el.child(super::timer_line(
                    "pending-elapsed",
                    "Waiting time",
                    super::spoken_seconds(seconds),
                    super::format_clock(seconds as f64),
                ))
                .when(seconds >= 30, |el| {
                    el.child(
                        super::text_line(
                            "pending-help",
                            Role::Label,
                            "If this continues, check Diagnostics for details.",
                        )
                        .text_role(TextRole::Caption),
                    )
                })
            })
    }
}

/// A banner's fill and edge: attention, failure and information on their
/// soft fills with their borders; every other kind on the neutral muted
/// ground. The words and the glyph still say which it is.
pub(crate) fn banner_colours(kind: StatusKind, cx: &App) -> (Hsla, Hsla) {
    let colours = brand(cx);
    match kind {
        StatusKind::Attention => (colours.warning_soft, colours.warning_border),
        StatusKind::Failed => (colours.danger_soft, colours.danger_border),
        StatusKind::Info => (colours.info_soft, colours.info_border),
        StatusKind::Pending | StatusKind::Done | StatusKind::Paused => {
            let muted = cx.theme().muted;
            (muted, muted)
        }
    }
}

/// A notice about a region, on its state's fill ([`banner_colours`]); an
/// information banner's glyph is in the information ink.
pub fn banner(
    id: impl Into<ElementId>,
    kind: StatusKind,
    role: Role,
    text: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let text = text.into();
    let theme = cx.theme();
    let (fill, edge) = banner_colours(kind, cx);
    let line = if role == Role::Note {
        super::text_line(id, Role::Note, text).text_role(TextRole::Caption)
    } else {
        let mark = (kind == StatusKind::Info).then(|| brand(cx).info);
        status_words(id, kind, role, text, false, mark, cx).text_color(theme.foreground)
    }
    // Wrap inside the row instead of keeping one line's width; the row's
    // actions move below once less than the basis is left.
    .flex_grow(1.)
    .flex_shrink(1.)
    .flex_basis(rems(16.))
    .min_w_0();
    #[cfg(test)]
    let line = gpui::base::TestSupportExt::test_support(line);
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap(CONTROL_GAP)
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(edge)
        .bg(fill)
        .child(line)
}

/// A failed list read with a retry action bound to that list.
pub fn retry_line<V: 'static>(
    id: &'static str,
    failure: impl Into<SharedString>,
    cx: &Context<V>,
    retry: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> Div {
    retry_line_with_id(id, format!("{id}-retry"), failure, cx, retry)
}

pub fn retry_line_with_id<V: 'static>(
    id: impl Into<ElementId>,
    retry_id: impl Into<ElementId>,
    failure: impl Into<SharedString>,
    cx: &Context<V>,
    retry: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> Div {
    let failure = failure.into();
    let retry_id = retry_id.into();
    let selector = retry_id.to_string();
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(CONTROL_GAP)
        .child(status_line(id, StatusKind::Failed, failure.clone(), cx))
        .child(
            button(retry_id, "Try again", true, cx, retry)
                .debug_selector(move || selector.clone())
                .ghost()
                .small()
                .accessibility_label(format!("Try again: {failure}")),
        )
}

/// Determinate progress. Callers supply the reported fraction and omit the
/// bar while the total is unknown.
pub fn progress(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    fraction: f32,
) -> Progress {
    Progress::new(id)
        .accessibility_label(label)
        .value(fraction * 100.)
        .w_full()
        .max_w(rems(15.))
}

/// Width of a [`fact_row`]'s label column, in rems.
pub const FACT_LABEL_REMS: f32 = 8.;

/// The narrowest a [`fact_row`]'s value may be beside its label, in rems:
/// with less than the label column plus this (32 rem) available, the label
/// sits above the value instead.
pub const FACT_VALUE_REMS: f32 = 24.;

/// What a [`fact_row`] shows (and speaks) for an empty value.
pub const FACT_NONE: &str = "None";

/// One label–value pair ("Library folder", "/ux/TEST-library"): a single
/// `Role::Label` node named "Library folder: /ux/TEST-library", so the pair
/// is read as one line. The label is a muted caption in an 8 rem column
/// beside the value, which wraps; with less than 32 rem available the value
/// wraps under the label instead (a flex wrap, so it follows the text size
/// without measuring the window). An empty value shows a muted "None" and
/// is spoken "None". Neither part writes a node of its own.
pub fn fact_row(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    fact(id.into(), label.into(), value.into(), false, cx)
}

/// A [`fact_row`] whose value is a path or an identifier, set in the
/// theme's monospace family.
pub fn fact_row_mono(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    fact(id.into(), label.into(), value.into(), true, cx)
}

fn fact(
    id: ElementId,
    label: SharedString,
    value: SharedString,
    mono: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let empty = value.trim().is_empty();
    let shown: SharedString = if empty { FACT_NONE.into() } else { value };
    // Test builds look the parts up by these; elsewhere they cost nothing.
    let selector = id.to_string();
    let value_selector = selector.clone();
    div()
        .id(id)
        .spoken(Role::Label, format!("{label}: {shown}"))
        .flex()
        .flex_row()
        .flex_wrap()
        .w_full()
        .child(
            div()
                .debug_selector(move || format!("{selector}-label"))
                .flex_none()
                .w(rems(FACT_LABEL_REMS))
                .pr_2()
                .text_role(TextRole::Caption)
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(
            div()
                .debug_selector(move || format!("{value_selector}-value"))
                .flex_grow_1()
                .flex_shrink_1()
                .flex_basis(rems(FACT_VALUE_REMS))
                .min_w_0()
                .text_role(if mono { TextRole::Mono } else { TextRole::Body })
                .when(mono, |el| el.font_family(theme.mono_font_family.clone()))
                .when(empty, |el| el.text_color(theme.muted_foreground))
                .child(shown),
        )
}

/// A user's tag as a chip on the muted fill; the caller adds the
/// remove button, if any, as a child.
pub fn tag_chip(content: impl IntoElement, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .flex_none()
        .min_h(rems(1.5))
        .py_0p5()
        .pl_2()
        .pr_1()
        .flex()
        .items_center()
        .gap_1()
        .rounded(theme.radius_lg)
        .bg(theme.muted)
        .text_color(theme.foreground)
        .text_role(TextRole::Caption)
        .child(content)
}

/// The tone of a confirmation: what its edge says before anyone reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmTone {
    /// Irreversible: discard, delete, forget.
    Danger,
    /// A consequence to accept: a licence, an update step, a provider.
    Warning,
}

/// The frame of an inline confirmation: a `radius-lg` card on the popover
/// ground, its edge in the tone's colour. The caller adds the question,
/// what goes and what stays, and the buttons (safe choice focused).
pub fn confirm_box(tone: ConfirmTone, cx: &App) -> Div {
    let theme = cx.theme();
    let edge = match tone {
        ConfirmTone::Danger => theme.danger,
        ConfirmTone::Warning => theme.warning,
    };
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(edge)
        .bg(theme.popover)
}

/// Places a confirmation's safe (`Cancel`) and action buttons at the trailing
/// end in the platform's dialog button order (SYS-151): `[safe, act]` on macOS
/// and Linux (`CancelFirst`), `[act, safe]` on Windows (`ActionFirst`). Focus
/// assignment stays with the caller so Cancel remains focused regardless of
/// visual order.
pub fn confirm_actions(safe: Button, act: Button, _cx: &App) -> Div {
    let row = h_flex().justify_end().gap(GROUP_GAP);
    match NativePlatform::dialog_button_order() {
        DialogButtonOrder::CancelFirst => row.child(safe).child(act),
        DialogButtonOrder::ActionFirst => row.child(act).child(safe),
    }
}

/// SHL-13 uses the platform confirmation so macOS can attach a sheet to
/// the main window. The only button with native key semantics is Cancel.
pub fn native_confirm_dialog<const N: usize>(
    message: &str,
    detail: &str,
    actions: [&str; N],
    window: &mut Window,
    cx: &mut App,
) -> futures::channel::oneshot::Receiver<usize> {
    let mut buttons = actions.map(gpui::PromptButton::new).to_vec();
    buttons.push(gpui::PromptButton::cancel("Cancel"));
    window.prompt(
        gpui::PromptLevel::Warning,
        message,
        Some(detail),
        &buttons,
        cx,
    )
}

pub type ConfirmLine = (ElementId, SharedString);

pub fn confirm_line(id: impl Into<ElementId>, text: impl Into<SharedString>) -> ConfirmLine {
    (id.into(), text.into())
}

/// Content and stable IDs for one irreversible action. Build the spec again
/// on a redraw when a dry run or a write can change the available actions.
type ConfirmRecovery = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Clone)]
pub struct ConfirmSpec {
    pub title: ConfirmLine,
    pub body_id: &'static str,
    pub footer_id: &'static str,
    pub cancel_id: &'static str,
    pub action: ConfirmLine,
    pub warning: Option<ConfirmLine>,
    pub goes: Vec<ConfirmLine>,
    pub changes: Vec<ConfirmLine>,
    pub stays: Vec<ConfirmLine>,
    pub headings: [&'static str; 3],
    pub caveat: Option<ConfirmLine>,
    pub primary_action: bool,
    pub irreversible: bool,
    pub irreversible_id: &'static str,
    pub status: Option<(ConfirmLine, StatusKind)>,
    pub recovery: Option<(ConfirmLine, ConfirmRecovery)>,
    pub action_enabled: bool,
    pub action_visible: bool,
    pub success_focus: Option<FocusHandle>,
    pub cancel_enabled: bool,
}

impl ConfirmSpec {
    pub fn new(
        title: ConfirmLine,
        body_id: &'static str,
        footer_id: &'static str,
        cancel_id: &'static str,
        action: ConfirmLine,
    ) -> Self {
        Self {
            title,
            body_id,
            footer_id,
            cancel_id,
            action,
            warning: None,
            goes: Vec::new(),
            changes: Vec::new(),
            stays: Vec::new(),
            headings: ["what-goes", "what-changes", "what-stays"],
            caveat: None,
            primary_action: false,
            irreversible: true,
            irreversible_id: "confirmation-irreversible",
            status: None,
            recovery: None,
            action_enabled: true,
            action_visible: true,
            success_focus: None,
            cancel_enabled: true,
        }
    }

    fn description(&self) -> String {
        self.warning
            .iter()
            .chain(&self.goes)
            .chain(&self.changes)
            .chain(&self.stays)
            .chain(&self.caveat)
            .map(|(_, text)| text.as_ref())
            .chain(self.irreversible.then_some("This can't be undone."))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn cancel_on_secondary_dot(ev: &gpui::KeyDownEvent, window: &mut Window, cx: &mut App) {
    if ev.keystroke.key == "." && ev.keystroke.modifiers.secondary(cx) {
        cx.stop_propagation();
        window.dispatch_action(Box::new(Cancel), cx);
    }
}

/// Opens the platform-ordered AlertDialog. Returning true from a callback
/// closes it. A pending write can keep it open until its result arrives.
pub fn confirm_dialog(
    opener: FocusHandle,
    window: &mut Window,
    cx: &mut App,
    spec: impl Fn(&App) -> Option<ConfirmSpec> + 'static,
    on_cancel: impl Fn(&mut Window, &mut App) -> bool + Clone + 'static,
    on_confirm: impl Fn(&mut Window, &mut App) -> bool + Clone + 'static,
) {
    opener.focus(window, cx);
    let cancel_focus = cx.focus_handle();
    let initial_focus = std::cell::Cell::new(true);
    window.open_alert_dialog(cx, move |alert, window, cx| {
        // BroScribe's dialogs stand on the popover ground, not the desk.
        let alert = alert.bg(cx.theme().popover);
        let Some(spec) = spec(cx) else {
            return alert.on_ok(|_, _, _| false).footer(div());
        };
        if initial_focus.replace(false) {
            cancel_focus.focus(window, cx);
        }
        let description = spec.description();
        let title = spec.title.1.clone();
        let heading =
            super::heading(spec.title.0, spec.title.1).a11y_synthetic_children(move |builder| {
                if let Some(node) = builder.ancestor_node(Role::AlertDialog) {
                    node.set_label(title.to_string());
                    node.set_description(description);
                }
            });
        let body = div()
            .id(spec.body_id)
            .flex()
            .flex_col()
            .gap(CONTROL_GAP)
            .on_key_down(cancel_on_secondary_dot)
            .when_some(spec.warning, |el, (id, words)| {
                el.child(status_line_role(
                    id,
                    StatusKind::Attention,
                    Role::Alert,
                    words,
                    cx,
                ))
            })
            .children(
                [
                    ("What goes", spec.goes),
                    ("What changes", spec.changes),
                    ("What stays", spec.stays),
                ]
                .into_iter()
                .zip(spec.headings)
                .filter(|((_, lines), _)| !lines.is_empty())
                .map(|((title, lines), id)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(super::heading(id, title))
                        .children(lines.into_iter().map(|(id, words)| {
                            super::text_line(id, Role::Label, words).text_role(TextRole::Body)
                        }))
                }),
            )
            .when_some(spec.caveat, |el, (id, words)| {
                el.child(
                    super::text_line(id, Role::Label, words)
                        .text_role(TextRole::Caption)
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .when_some(spec.status, |el, ((id, words), kind)| {
                el.child(status_line(id, kind, words, cx))
            })
            .when_some(spec.recovery, |el, ((id, words), recover)| {
                el.child(
                    div().child(
                        Button::new(id)
                            .label(words)
                            .outline()
                            .on_click(move |_, window, cx| recover(window, cx)),
                    ),
                )
            })
            .when(spec.irreversible, |body| {
                body.child(
                    super::text_line(spec.irreversible_id, Role::Label, "This can't be undone.")
                        .text_role(TextRole::Body),
                )
            });
        let on_escape = on_cancel.clone();
        let opener_on_cancel = opener.clone();
        let opener_on_action = spec.success_focus.unwrap_or_else(|| opener.clone());
        let opener_on_close = opener.clone();
        let on_cancel = on_cancel.clone();
        let on_confirm = on_confirm.clone();
        let cancel_id = spec.cancel_id;
        let safe = Button::new(cancel_id)
            .debug_selector(move || cancel_id.to_owned())
            .label("Cancel")
            .outline()
            .track_focus(&cancel_focus)
            .map(|button| disable_action(button, !spec.cancel_enabled, cx))
            .on_click(move |_, window, cx| {
                if on_cancel(window, cx) {
                    window.close_dialog(cx);
                    opener_on_cancel.focus(window, cx);
                }
            });
        let action_id = spec.action.0.to_string();
        let action = Button::new(spec.action.0)
            .debug_selector(move || action_id.clone())
            .label(spec.action.1)
            .map(|button| {
                if spec.primary_action {
                    self::primary(button, !spec.action_enabled, cx)
                } else {
                    button.danger()
                }
            })
            .map(|button| disable_action(button, !spec.action_enabled, cx))
            .on_click(move |_, window, cx| {
                if on_confirm(window, cx) {
                    window.close_dialog(cx);
                    opener_on_action.focus(window, cx);
                }
            });
        let footer = if spec.action_visible {
            confirm_actions(safe, action, cx)
        } else {
            h_flex().justify_end().child(safe)
        };
        alert
            .width(px(440.))
            .entrance(DialogEntrance::None)
            .keyboard(true)
            .on_ok(|_, _, _| false)
            .on_cancel(move |_, window, cx| on_escape(window, cx))
            .on_close(move |_, window, cx| opener_on_close.focus(window, cx))
            .title(heading)
            .child(body)
            .footer(
                div()
                    .id(spec.footer_id)
                    .on_key_down(cancel_on_secondary_dot)
                    .child(footer),
            )
    });
}

/// Which audio track a meter shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    /// The microphone.
    Me,
    /// System audio.
    Others,
}

/// Segments in a level meter.
const METER_SEGMENTS: usize = 20;

/// The share of a level meter lit for `dbfs` (−60..0 dBFS over the meter).
pub fn meter_fraction(dbfs: f32) -> f32 {
    ((dbfs + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Visible segments and audible state for redraw coalescing.
pub(crate) fn meter_readout(dbfs: f32) -> (usize, bool) {
    let lit = (meter_fraction(dbfs) * METER_SEGMENTS as f32).round() as usize;
    (lit, dbfs >= -50.)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MeterState {
    Recording,
    Paused,
    Retrying,
    NoAccess,
    Disconnected,
}

/// A track's capture state and signal level. The name changes only on a
/// state change or when sound crosses the silence threshold.
pub(crate) fn level_meter(
    id: impl Into<ElementId>,
    track: Track,
    dbfs: f32,
    state: MeterState,
    cx: &App,
) -> Stateful<Div> {
    let colours = brand(cx);
    let (label, lit_colour) = match track {
        Track::Me => ("Me", colours.meter_me),
        Track::Others => ("Others", colours.meter_others),
    };
    let dbfs = if state == MeterState::Recording && dbfs.is_finite() {
        dbfs.clamp(-60., 0.)
    } else {
        -60.
    };
    let (lit, has_sound) = meter_readout(dbfs);
    let activity = match state {
        MeterState::Recording => "recording",
        MeterState::Paused => "paused",
        MeterState::Retrying => "retrying",
        MeterState::NoAccess => "no access",
        MeterState::Disconnected => "not connected",
    };
    let words = if state == MeterState::Recording {
        let sound = if has_sound { "sound" } else { "silent" };
        format!("{label}: {activity}, {sound}")
    } else {
        format!("{label}: {activity}")
    };
    div()
        .id(id)
        .role(Role::Meter)
        .aria_label(SharedString::from(words))
        .aria_numeric_value(f64::from(dbfs))
        .aria_min_numeric_value(-60.)
        .aria_max_numeric_value(0.)
        .when(state == MeterState::Disconnected, |el| el.opacity(0.5))
        .flex()
        .items_center()
        .gap_2()
        .text_role(TextRole::Label)
        .child(div().min_w(px(52.)).flex_none().child(label))
        .child(
            div()
                .flex()
                .gap(px(2.))
                .flex_shrink_0()
                .children((0..METER_SEGMENTS).map(|segment| {
                    div()
                        .w(px(5.))
                        .h(px(10.))
                        .rounded_full()
                        .bg(if segment < lit {
                            lit_colour
                        } else {
                            colours.meter_track
                        })
                })),
        )
        .child(
            super::type_scale::tabular(div())
                .text_role(TextRole::Caption)
                .text_color(cx.theme().muted_foreground)
                .child(activity),
        )
}

/// The visor's face, the way the mascot's face shows its state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Face {
    /// Eyes and the recording dot: audio is being captured.
    Recording,
    /// Eyes and a flat line.
    Paused,
    /// Eyes only: queued.
    Waiting,
    /// Eyes and `Bro_`: transcribing.
    Working,
    /// Eyes and `!` in the visor's accent: retrying, or not able to
    /// record yet.
    Attention,
    /// Eyes and `!` in the visor's red: a recording that could not be
    /// transcribed or saved.
    Failed,
    /// Eyes and a smile: saved.
    Saved,
}

/// The visor's face, 44 by 36 points: two eyes and the state's mouth.
pub fn visor_face(face: Face, cx: &App) -> Div {
    let colours = brand(cx);
    let eye = || {
        div()
            .debug_selector(|| "visor-eye".into())
            .w(px(5.))
            .h(px(13.))
            .rounded(px(2.5))
            .bg(colours.visor_foreground)
    };
    let mouth = match face {
        Face::Recording => div()
            .size(px(8.))
            .rounded_full()
            .bg(colours.visor_recording)
            .into_any_element(),
        Face::Paused => div()
            .w(px(10.))
            .h(px(3.))
            .rounded_full()
            .bg(colours.visor_accent)
            .into_any_element(),
        Face::Waiting => div().h(px(8.)).into_any_element(),
        Face::Working => div()
            .debug_selector(|| "visor-mouth-glyph".into())
            .text_size(px(9.))
            .line_height(px(10.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(colours.visor_accent)
            .child("Bro_")
            .into_any_element(),
        Face::Attention | Face::Failed => div()
            .debug_selector(|| "visor-mouth-glyph".into())
            .text_size(px(9.))
            .line_height(px(10.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(if face == Face::Failed {
                colours.visor_recording
            } else {
                colours.visor_accent
            })
            .child("!")
            .into_any_element(),
        Face::Saved => svg()
            .path(assets::VISOR_SMILE)
            .w(px(14.))
            .h(px(6.))
            .text_color(colours.visor_accent)
            .into_any_element(),
    };
    div()
        .flex_none()
        .w(px(44.))
        .h(px(36.))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(4.))
        .child(div().flex().gap(px(6.)).child(eye()).child(eye()))
        .child(
            div()
                .debug_selector(|| "visor-mouth".into())
                .h(px(10.))
                .flex()
                .items_center()
                .child(mouth),
        )
}

/// The visor: the one dark panel, in both appearances, where a recording
/// shows its state. The caller adds the words (its own named nodes) as
/// children beside the face; they read in `visor-foreground`.
pub fn visor(face: Face, cx: &App) -> Div {
    let colours = brand(cx);
    div()
        .flex()
        .items_center()
        .gap_3()
        .px_4()
        .py_3()
        .rounded(px(28.))
        .border_2()
        .border_color(colours.visor_rim)
        .bg(colours.visor)
        .text_color(colours.visor_foreground)
        .text_role(TextRole::Visor)
        .child(visor_face(face, cx))
}

/// The mascot drawing at `size` points (decorative: no node; the view's
/// heading says what the screen is).
pub fn mascot(path: &'static str, size: f32) -> impl IntoElement {
    div()
        .size(px(size))
        .flex_none()
        .debug_selector(move || path.into())
        .child(img(path).size_full())
}

#[cfg(test)]
pub(crate) fn painted_primary_actions(window: &mut Window, cx: &mut App) -> Vec<String> {
    window.draw(cx).clear(cx);
    let primary: gpui::Background = cx.theme().tokens.button_primary.into();
    let scale = window.scale_factor();
    let quads = window.painted_quads();
    gpui::base::test_support::snapshots(window)
        .iter()
        .filter(|node| node.role() == Some(Role::Button))
        .filter(|node| {
            let bounds = node.bounds().scale(scale);
            quads
                .iter()
                .any(|quad| quad.background == primary && quad.bounds == bounds)
        })
        .filter_map(|node| node.path().last().map(ToString::to_string))
        .collect()
}

#[cfg(test)]
pub(crate) mod dialog_gate {
    use super::*;
    use gpui::{Modifiers, VisualTestContext};

    pub fn draw(vcx: &mut VisualTestContext) {
        for _ in 0..2 {
            vcx.update(|window, cx| window.draw(cx).clear(cx));
        }
    }

    pub fn click(vcx: &mut VisualTestContext, id: &str, role: Role) {
        draw(vcx);
        let center = vcx.update(|window, _| {
            gpui::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| {
                    node.role() == Some(role)
                        && node
                            .path()
                            .last()
                            .is_some_and(|part| part.to_string() == id)
                })
                .unwrap_or_else(|| panic!("missing {role:?} {id}"))
                .bounds()
                .center()
        });
        vcx.simulate_click(center, Modifiers::none());
        vcx.run_until_parked();
        draw(vcx);
    }

    pub struct Expected<'a> {
        pub title: &'a str,
        pub cancel_id: &'a str,
        pub action_id: &'a str,
        pub action: &'a str,
        pub lines: &'a [(&'a str, &'a str)],
        pub headings: &'a [&'a str],
    }

    /// Exercise both platforms' orders and the two cancelling shortcuts with
    /// the view's own opener. The same named buttons and lines must render in
    /// both layouts, and the first Tab must stay inside the modal.
    pub fn cancel_both_ways(
        vcx: &mut VisualTestContext,
        opener_id: &str,
        expected: Expected<'_>,
        mut open: impl FnMut(&mut VisualTestContext),
    ) {
        use broscribe_platform::fakes::FakeDialogButtonOrder;
        let opener_focused = |vcx: &mut VisualTestContext| {
            vcx.update(|window, _| {
                gpui::base::test_support::snapshots(window)
                    .iter()
                    .any(|node| {
                        node.path()
                            .last()
                            .is_some_and(|part| part.to_string() == opener_id)
                            && node.focused() == Some(true)
                    })
            })
        };
        for (order, shortcut) in [
            (DialogButtonOrder::ActionFirst, "escape"),
            (DialogButtonOrder::CancelFirst, "secondary-."),
        ] {
            let _platform = FakeDialogButtonOrder::new(order);
            open(vcx);
            vcx.run_until_parked();
            draw(vcx);
            vcx.update(|window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                let dialogs: Vec<_> = tree
                    .nodes
                    .iter()
                    .enumerate()
                    .filter(|(_, node)| node.role == "AlertDialog")
                    .collect();
                let [(dialog, node)] = dialogs.as_slice() else {
                    panic!("one alert dialog: {dialogs:?}");
                };
                assert_eq!(node.name, expected.title);
                let subtree = tree.subtree(*dialog);
                for (id, words) in expected.lines {
                    let line = tree.find(id);
                    assert!(subtree.contains(&line), "{id} belongs to the dialog");
                    assert_eq!(tree.read(line), ("Label", *words));
                    assert!(
                        node.description.contains(words),
                        "{id} is in the dialog description"
                    );
                }
                for heading in expected.headings {
                    assert!(
                        subtree
                            .iter()
                            .any(|&ix| tree.read(ix) == ("Heading", *heading)),
                        "missing {heading}"
                    );
                }
                let snapshots = gpui::base::test_support::snapshots(window);
                let button = |id: &str| {
                    snapshots
                        .iter()
                        .find(|node| {
                            node.role() == Some(Role::Button)
                                && node
                                    .path()
                                    .last()
                                    .is_some_and(|part| part.to_string() == id)
                        })
                        .unwrap_or_else(|| panic!("missing button {id}"))
                };
                let cancel = button(expected.cancel_id);
                let action = button(expected.action_id);
                assert_eq!(
                    tree.read(tree.find(expected.cancel_id)),
                    ("Button", "Cancel")
                );
                assert_eq!(
                    tree.read(tree.find(expected.action_id)),
                    ("Button", expected.action)
                );
                let bounds = action.bounds().scale(window.scale_factor());
                let danger: gpui::Background = cx.theme().tokens.button_danger.into();
                assert!(
                    window
                        .painted_quads()
                        .iter()
                        .any(|quad| { quad.bounds == bounds && quad.background == danger }),
                    "the confirming button has solid danger emphasis"
                );
                assert_eq!(cancel.focused(), Some(true));
                let (leading, trailing) = match order {
                    DialogButtonOrder::ActionFirst => (action, cancel),
                    DialogButtonOrder::CancelFirst => (cancel, action),
                };
                assert_eq!(
                    trailing.bounds().left() - leading.bounds().right(),
                    window.rem_size() * GROUP_GAP.0
                );
            });
            vcx.simulate_keystrokes("tab");
            vcx.run_until_parked();
            assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
            assert!(!opener_focused(vcx));
            vcx.simulate_keystrokes(shortcut);
            vcx.run_until_parked();
            draw(vcx);
            assert!(
                !vcx.update(|window, cx| window.has_active_dialog(cx)),
                "{shortcut} closes the dialog"
            );
            assert!(
                opener_focused(vcx),
                "{shortcut} restores focus to {opener_id}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, Context, Render, TestAppContext, Window};

    use super::*;

    /// A primary action is violet; disabled, it is muted (not a faded
    /// violet) and keeps the same full focus ring an enabled one has.
    #[gpui::test]
    fn a_disabled_primary_is_muted_and_keeps_its_focus_ring(cx: &mut TestAppContext) {
        struct Probe(FocusHandle, bool);
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div().p_8().child(
                    disable_action(
                        primary(Button::new("probe").label("Save"), self.1, cx)
                            .track_focus(&self.0),
                        self.1,
                        cx,
                    )
                    .debug_selector(|| "probe".into()),
                )
            }
        }
        cx.update(crate::ui::theme::init);
        let mut rings = Vec::new();
        for disabled in [false, true] {
            let (view, visual) = cx.add_window_view(|_, cx| Probe(cx.focus_handle(), disabled));
            visual.update(|window, cx| {
                let focus = view.read(cx).0.clone();
                window.focus(&focus, cx);
                window.draw(cx).clear(cx);
            });
            let bounds = visual.debug_bounds("probe").unwrap();
            visual.update(|window, cx| {
                let theme = cx.theme();
                let scale = window.scale_factor();
                let quads = window.painted_quads();
                let fill = quads
                    .iter()
                    .find(|quad| quad.bounds == bounds.scale(scale))
                    .map(|quad| quad.background)
                    .unwrap();
                let expected: gpui::Background = if disabled {
                    theme.muted.into()
                } else {
                    theme.tokens.button_primary.into()
                };
                assert_eq!(fill, expected, "disabled: {disabled}");
                assert!(
                    !quads.iter().any(|quad| {
                        quad.background == theme.tokens.button_primary.opacity(0.15).into()
                    }),
                    "no pastel violet"
                );
                rings.push(
                    quads
                        .iter()
                        .filter(|quad| {
                            quad.bounds != bounds.scale(scale)
                                && quad.bounds.intersects(&bounds.scale(scale))
                                && quad.border_color.h == theme.ring.h
                        })
                        .map(|quad| (quad.border_color, quad.border_widths.top))
                        .collect::<Vec<_>>(),
                );
            });
        }
        assert!(!rings[0].is_empty());
        assert_eq!(rings[0], rings[1]);
    }

    /// Named by you is teal with a check, a suggestion dashed on no fill,
    /// an attention badge (Unresolved) on the warning-soft fill.
    #[gpui::test]
    fn badges_take_their_state_roles(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::ui::theme::init(cx);
            let colours = brand(cx);
            let theme = cx.theme();
            let mut strong = badge(BadgeTone::Strong, "Named by you", cx);
            let style = strong.style();
            assert_eq!(style.background, Some(colours.success_soft.into()));
            assert_eq!(style.border_color, Some(colours.success_border));
            assert_eq!(style.text.color, Some(colours.success));
            let mut suggested = badge(BadgeTone::Suggested, "Suggested", cx);
            let style = suggested.style();
            assert_eq!(style.background, Some(gpui::transparent_black().into()));
            assert_eq!(style.border_color, Some(theme.input));
            assert_eq!(style.border_style, Some(gpui::BorderStyle::Dashed));
            assert_eq!(style.text.color, Some(theme.foreground));
            let mut unresolved = badge(BadgeTone::Attention, "Unresolved", cx);
            let style = unresolved.style();
            assert_eq!(style.background, Some(colours.warning_soft.into()));
            assert_eq!(style.text.color, Some(theme.warning));
            let mut count = count_badge(3, cx);
            let style = count.style();
            assert_eq!(style.background, Some(colours.badge.into()));
            assert_eq!(style.text.color, Some(colours.badge_foreground));
            assert_eq!(style.text.font_weight, Some(FontWeight::SEMIBOLD));
            assert_eq!(
                banner_colours(StatusKind::Failed, cx),
                (colours.danger_soft, colours.danger_border)
            );
            assert_eq!(
                banner_colours(StatusKind::Pending, cx),
                (theme.muted, theme.muted)
            );
        });
    }

    /// A selected row has a violet bar on its leading edge; what an AI
    /// wrote has a violet rule; the record dot is red on a secondary
    /// button and the label colour on a primary one.
    #[gpui::test]
    fn selection_ai_results_and_the_record_dot_use_their_roles(cx: &mut TestAppContext) {
        struct Probe;
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div()
                    .w(px(400.))
                    .child(
                        list_row("row", true, false, cx)
                            .debug_selector(|| "row".into())
                            .child("Selected"),
                    )
                    .child(as_ai_result(div().child("Summary"), cx))
                    .child(
                        div()
                            .debug_selector(|| "secondary".into())
                            .child(record_dot(false, false, cx)),
                    )
                    .child(
                        div()
                            .debug_selector(|| "on-primary".into())
                            .child(record_dot(true, false, cx)),
                    )
            }
        }
        cx.update(crate::ui::theme::init);
        let (_, visual) = cx.add_window_view(|_, _| Probe);
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let row = visual.debug_bounds("row").unwrap();
        let rule = visual.debug_bounds("ai-rule").unwrap();
        let dots = ["secondary", "on-primary"].map(|id| visual.debug_bounds(id).unwrap());
        visual.update(|window, cx| {
            let colours = brand(cx);
            let theme = cx.theme();
            let scale = window.scale_factor();
            let quads = window.painted_quads();
            assert!(quads.iter().any(|quad| {
                quad.background == colours.primary.into()
                    && quad.bounds.size.width == px(3.).scale(scale)
                    && quad.bounds.origin.x == row.origin.x.scale(scale)
            }));
            assert!(quads.iter().any(|quad| {
                quad.background == theme.list_active.into() && quad.bounds == row.scale(scale)
            }));
            assert!(quads.iter().any(|quad| {
                quad.bounds == rule.scale(scale) && quad.background == colours.ai_border.into()
            }));
            for (parent, colour) in dots
                .iter()
                .zip([colours.recording, theme.primary_foreground])
            {
                assert!(quads.iter().any(|quad| {
                    quad.bounds.intersects(&parent.scale(scale)) && quad.background == colour.into()
                }));
            }
        });
    }

    /// A known person is a disc in their hue with on-person initials, Me a
    /// slate disc, and anyone unconfirmed a dashed ring in foreground ink.
    #[gpui::test]
    fn discs_follow_who_they_stand_for(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::ui::theme::init(cx);
            let colours = brand(cx);
            assert_eq!(
                disc_style(Disc::Person(3), cx),
                (Some(colours.person_hue(3)), colours.on_person, false)
            );
            assert_eq!(
                disc_style(Disc::Me, cx),
                (Some(colours.me), colours.on_person, false)
            );
            for unconfirmed in [Disc::Suggested, Disc::Voice(Some('A'))] {
                assert_eq!(
                    disc_style(unconfirmed, cx),
                    (None, cx.theme().foreground, true)
                );
            }
            let mut me = broscribe_core::schema::Person::new(
                broscribe_core::PersonId::parse("p-a1a1a1").unwrap(),
                "Synthetic Me",
            );
            me.is_self = true;
            assert_eq!(Disc::for_person(Some(&me)), Disc::Me);
            me.is_self = false;
            me.hue = Some(5);
            assert_eq!(Disc::for_person(Some(&me)), Disc::Person(5));
            assert_eq!(Disc::for_person(None), Disc::Voice(None));
        });
    }
    use crate::ui::{a11y_name, spoken_log};

    /// A fact row in a container `width` rem wide.
    struct FactProbe {
        width: f32,
        value: &'static str,
    }

    impl Render for FactProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().w(rems(self.width)).child(fact_row(
                "probe-fact",
                "Library folder",
                self.value,
                cx,
            ))
        }
    }

    #[gpui::test]
    fn rows_fields_sections_and_empty_states_keep_their_layout_and_labels(cx: &mut TestAppContext) {
        const TITLE: &str =
            "A long synthetic meeting title that does not fit between the time and duration";
        struct Probe;
        impl gpui::Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                stack()
                    .w(rems(22.))
                    .child(
                        list_row("sample-row", true, true, cx)
                            .role(Role::ListItem)
                            .aria_label("Synthetic meeting")
                            .debug_selector(|| "entry".into())
                            .tooltip(title_tooltip(TITLE))
                            .child(row_leading(
                                rems(3.),
                                div().debug_selector(|| "leading".into()).child("12:00"),
                            ))
                            .child(row_body(TITLE, Some("Synthetic facts"), true, cx))
                            .child(row_trailing(
                                div()
                                    .w(rems(3.5))
                                    .debug_selector(|| "trailing".into())
                                    .child("1 min"),
                            )),
                    )
                    .child(
                        section(
                            "sample-section",
                            "Options",
                            false,
                            field(
                                "sample-label",
                                "Meeting language",
                                Some(("sample-help".into(), "Choose a language.".into())),
                                |label| {
                                    Button::new("sample-control")
                                        .label("Choose")
                                        .accessibility_label(label)
                                },
                                cx,
                            ),
                        )
                        .debug_selector(|| "section".into()),
                    )
                    .child(empty_state(
                        "sample-empty",
                        "No meetings yet",
                        super::super::text_line(
                            "sample-empty-body",
                            Role::Status,
                            "Record a meeting.",
                        ),
                        false,
                        Some(Button::new("empty-primary").label("Record")),
                        Some(Button::new("empty-secondary").label("Import")),
                        cx,
                    ))
            }
        }
        cx.update(crate::ui::theme::init);
        let clock = cx.executor().clone();
        let (_, v) = cx.add_window_view(|_, _| Probe);
        for rem in [16., 20.] {
            v.update(|window, cx| {
                window.set_rem_size(px(rem));
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("sample-control")),
                    ("Button", "Meeting language")
                );
                assert_eq!(
                    tree.read(tree.find("sample-help")),
                    ("Label", "Choose a language.")
                );
                assert_eq!(
                    tree.read(tree.find("sample-empty")),
                    ("Heading", "No meetings yet")
                );
                assert_eq!(
                    tree.read(tree.find("sample-empty-body")),
                    ("Status", "Record a meeting.")
                );
                assert_eq!(tree.read(tree.find("empty-primary")), ("Button", "Record"));
                assert_eq!(
                    tree.read(tree.find("empty-secondary")),
                    ("Button", "Import")
                );
                assert_eq!(
                    tree.nodes
                        .iter()
                        .filter(|node| node.role == "Button")
                        .count(),
                    3
                );
                let json: serde_json::Value =
                    serde_json::from_str(&window.debug_a11y_tree_json().unwrap()).unwrap();
                assert!(json["nodes"].as_object().unwrap().values().any(|node| {
                    node["aria"]["label"] == "Synthetic meeting" && node["aria"]["selected"] == true
                }));
            });
            let leading = v.debug_bounds("leading").unwrap();
            let trailing = v.debug_bounds("trailing").unwrap();
            let entry = v.debug_bounds("entry").unwrap();
            assert_eq!(leading.size.width, px(rem * 3.));
            assert_eq!(trailing.size.width, px(rem * 3.5));
            assert!(entry.size.height >= px(rem * 3.5));
            let section = v.debug_bounds("section").unwrap();
            assert_eq!(section.top() - entry.bottom(), px(rem * 2.));
        }
        let center = v.debug_bounds("entry").unwrap().center();
        v.simulate_mouse_move(center, None, gpui::Modifiers::none());
        v.run_until_parked();
        clock.advance_clock(Duration::from_secs(1));
        v.run_until_parked();
        v.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(
                tree.nodes.iter().any(|node| node.role == "Tooltip"),
                "{tree:?}"
            );
        });
    }

    #[gpui::test]
    fn visor_mouth_clears_the_eyes_even_at_double_text_size(cx: &mut TestAppContext) {
        struct FaceProbe(Face, f32);
        impl Render for FaceProbe {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                window.set_rem_size(px(self.1));
                visor(self.0, cx)
            }
        }
        cx.update(gpui::init);
        for face in [Face::Working, Face::Attention, Face::Failed] {
            for rem in [16., 32.] {
                let window = cx.add_window(|_, _| FaceProbe(face, rem));
                let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
                dialog_gate::draw(&mut visual);
                let eye = visual.debug_bounds("visor-eye").unwrap();
                let mouth = visual.debug_bounds("visor-mouth").unwrap();
                let glyph = visual.debug_bounds("visor-mouth-glyph").unwrap();
                assert!(eye.bottom() < mouth.top(), "{face:?} at {rem}");
                assert!(
                    glyph.top() >= mouth.top() && glyph.bottom() <= mouth.bottom(),
                    "{face:?} at {rem}: {glyph:?} in {mouth:?}"
                );
            }
        }
    }

    #[gpui::test]
    fn the_mascot_sizes_and_card_and_badge_surfaces_are_painted(cx: &mut TestAppContext) {
        struct Probe;
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                stack()
                    .w(rems(24.))
                    .child(
                        card("card", cx)
                            .debug_selector(|| "card".into())
                            .child(super::super::heading("section-title", "Selected speaker")),
                    )
                    .child(
                        div().bg(cx.theme().list_active).child(
                            badge(BadgeTone::Strong, "Confirmed", cx)
                                .debug_selector(|| "strong-badge".into()),
                        ),
                    )
                    .child(mascot(assets::MASCOT_IDLE, EMPTY_MASCOT_SIZE))
                    .child(mascot(assets::TILE_HERO, ABOUT_TILE_SIZE))
            }
        }
        cx.update(crate::ui::theme::init);
        let (_, visual) = cx.add_window_view(|_, _| Probe);
        visual.update(|window, cx| {
            window.set_rem_size(px(16.));
            window.draw(cx).clear(cx);
        });
        let card_bounds = visual.debug_bounds("card").unwrap();
        let badge_bounds = visual.debug_bounds("strong-badge").unwrap();
        assert_eq!(
            visual.debug_bounds(assets::MASCOT_IDLE).unwrap().size,
            gpui::size(px(128.), px(128.))
        );
        assert_eq!(
            visual.debug_bounds(assets::TILE_HERO).unwrap().size,
            gpui::size(px(64.), px(64.))
        );
        visual.update(|window, cx| {
            let theme = cx.theme();
            let scale = window.scale_factor();
            let quads = window.painted_quads();
            assert!(
                quads
                    .iter()
                    .any(|quad| quad.bounds == card_bounds.scale(scale)
                        && quad.border_color == theme.border
                        && quad.corner_radii.top_left
                            == super::super::brand::RADIUS_CARD.scale(scale))
            );
            assert!(
                quads
                    .iter()
                    .any(|quad| quad.bounds == card_bounds.scale(scale)
                        && quad.background == brand(cx).card.into())
            );
            // Named by you is done teal (success-soft), not ink on paper.
            let colours = brand(cx);
            assert!(
                quads
                    .iter()
                    .any(|quad| quad.bounds == badge_bounds.scale(scale)
                        && quad.background == colours.success_soft.into())
            );
            assert!(
                quads
                    .iter()
                    .any(|quad| quad.bounds == badge_bounds.scale(scale)
                        && quad.border_color == colours.success_border)
            );
        });
    }

    #[test]
    fn review_rows_have_no_outer_bordered_frame() {
        let source = include_str!("../views/review.rs");
        let list = source
            .split_once("let list = div()")
            .unwrap()
            .1
            .split_once(".children(rows)")
            .unwrap()
            .0;
        assert!(!list.contains(".border_"));
        assert!(source.contains("kit::selected_row("));
    }

    /// SYS-40: one `Label` node named "label: value" (its parts write none),
    /// "None" for an empty value, and the label beside the value from 32 rem
    /// of width, above it below that.
    #[gpui::test]
    fn a_fact_row_is_one_label_node_and_stacks_below_32_rem(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        let (row, named) = spoken_log::while_building(|| {
            cx.update(|cx| fact_row("probe-fact", "Library folder", "/ux/TEST-library", cx))
        });
        let expected = (
            "probe-fact".to_owned(),
            Role::Label,
            "Library folder: /ux/TEST-library".to_owned(),
        );
        assert_eq!(named, std::slice::from_ref(&expected));
        assert_eq!(a11y_name(&row), Some(expected));
        let (_, empty) = spoken_log::while_building(|| {
            cx.update(|cx| fact_row_mono("probe-empty", "Other names", "  ", cx))
        });
        assert_eq!(
            empty,
            [(
                "probe-empty".to_owned(),
                Role::Label,
                "Other names: None".to_owned()
            )]
        );

        let place = |width: f32, cx: &mut TestAppContext| {
            let window = cx.add_window(move |_, _| FactProbe {
                width,
                value: "/ux/TEST-library",
            });
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
            vcx.run_until_parked();
            (
                vcx.debug_bounds("probe-fact-label")
                    .expect("the label is drawn"),
                vcx.debug_bounds("probe-fact-value")
                    .expect("the value is drawn"),
            )
        };
        let rem = 16.;
        let (label, value) = place(40., cx);
        assert_eq!(label.size.width, px(FACT_LABEL_REMS * rem));
        assert_eq!(value.left(), label.right(), "beside: {label:?} {value:?}");
        assert_eq!(value.top(), label.top());
        let (label, value) = place(32., cx);
        assert_eq!(
            value.left(),
            label.right(),
            "32 rem fits: {label:?} {value:?}"
        );
        let (label, value) = place(31.5, cx);
        assert_eq!(value.left(), label.left(), "stacked: {label:?} {value:?}");
        assert!(value.top() >= label.bottom(), "{label:?} {value:?}");
        assert_eq!(value.size.width, px(31.5 * rem));
    }

    #[gpui::test]
    fn radio_arrows_wrap_skip_disabled_and_focus_the_activated_option(cx: &mut TestAppContext) {
        struct Probe {
            selected: usize,
            changes: Vec<usize>,
            first: FocusHandle,
        }
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let mut group = ChoiceGroup::new(stack(), "choice-probe", "Recording file");
                for (index, label) in ["Copy", "Move", "Keep in place"].into_iter().enumerate() {
                    let option = radio_option(
                        ("pick", index),
                        label,
                        self.selected == index,
                        index != 1,
                        cx,
                        move |this, _, cx| {
                            this.selected = index;
                            this.changes.push(index);
                            cx.notify();
                        },
                    );
                    group = group.option(if index == 0 {
                        option.track_focus(&self.first)
                    } else {
                        option
                    });
                }
                stack()
                    .child(group)
                    .child(Button::new("after-choice").label("After choice"))
            }
        }
        cx.update(crate::ui::theme::init);
        let (probe, visual) = cx.add_window_view(|_, cx| Probe {
            selected: 0,
            changes: Vec::new(),
            first: cx.focus_handle(),
        });
        let first = probe.read_with(visual, |probe, _| probe.first.clone());
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            first.focus(window, cx);
            window.draw(cx).clear(cx);
        });
        let check = |visual: &mut gpui::VisualTestContext, selected, expected: &[usize]| {
            assert_eq!(
                probe.read_with(visual, |probe, _| probe.changes.clone()),
                expected
            );
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                let nodes = gpui::base::test_support::snapshots(window);
                assert_eq!(
                    nodes
                        .iter()
                        .filter(|node| node.role() == Some(Role::RadioButton)
                            && node.checked() == Some(true))
                        .count(),
                    1
                );
                let node = nodes
                    .iter()
                    .find(|node| node.label() == Some(selected))
                    .unwrap();
                assert_eq!(node.checked(), Some(true));
                assert_eq!(node.focused(), Some(true));
            });
        };
        visual.simulate_keystrokes("left");
        check(visual, "Keep in place", &[2]);
        visual.simulate_keystrokes("right");
        check(visual, "Copy", &[2, 0]);
        visual.update(|window, cx| window.focus_next(cx));
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let nodes = gpui::base::test_support::snapshots(window);
            assert_eq!(
                nodes
                    .iter()
                    .find(|node| node.label() == Some("After choice"))
                    .unwrap()
                    .focused(),
                Some(true)
            );
        });
    }

    #[gpui::test]
    fn checkbox_activation_updates_the_checked_state(cx: &mut TestAppContext) {
        use gpui::component::checkbox::Checkbox;
        struct Probe(bool);
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                Checkbox::new("keep-example")
                    .label("Keep as a meeting")
                    .checked(self.0)
                    .on_change(cx.listener(|this, checked, _, cx| {
                        this.0 = *checked;
                        cx.notify();
                    }))
            }
        }
        cx.update(crate::ui::theme::init);
        let (_, visual) = cx.add_window_view(|_, _| Probe(false));
        let check = |visual: &mut gpui::VisualTestContext, checked| {
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                let nodes = gpui::base::test_support::snapshots(window);
                let checkbox = nodes
                    .iter()
                    .find(|node| {
                        node.role() == Some(Role::CheckBox)
                            && node.label() == Some("Keep as a meeting")
                    })
                    .unwrap();
                assert_eq!(checkbox.checked(), Some(checked));
                checkbox.bounds().center()
            })
        };
        let center = check(visual, false);
        visual.simulate_click(center, Default::default());
        check(visual, true);
    }

    #[test]
    fn the_meter_covers_sixty_decibels() {
        assert_eq!(meter_fraction(-90.0), 0.0);
        assert_eq!(meter_fraction(-60.0), 0.0);
        assert_eq!(meter_fraction(-30.0), 0.5);
        assert_eq!(meter_fraction(0.0), 1.0);
        assert_eq!(meter_fraction(6.0), 1.0);
    }

    #[test]
    fn only_a_failure_is_an_alert() {
        for kind in [
            StatusKind::Info,
            StatusKind::Pending,
            StatusKind::Done,
            StatusKind::Attention,
            StatusKind::Paused,
        ] {
            assert_eq!(kind.role(), Role::Status);
        }
        assert_eq!(StatusKind::Failed.role(), Role::Alert);
    }

    #[gpui::test]
    fn status_glyphs_live_roles_and_banner_fills_match_their_kind(cx: &mut TestAppContext) {
        struct Probe;
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let kinds = [
                    StatusKind::Info,
                    StatusKind::Pending,
                    StatusKind::Done,
                    StatusKind::Attention,
                    StatusKind::Failed,
                    StatusKind::Paused,
                ];
                stack()
                    .children(kinds.into_iter().enumerate().map(|(index, kind)| {
                        status_line(("status", index), kind, format!("Synthetic {kind:?}"), cx)
                    }))
                    .child(status_line_role(
                        "interrupting",
                        StatusKind::Attention,
                        Role::Alert,
                        "Disk space is low",
                        cx,
                    ))
                    .child(banner(
                        "notice",
                        StatusKind::Attention,
                        Role::Status,
                        "Synthetic notice",
                        cx,
                    ))
                    .child(banner(
                        "tip",
                        StatusKind::Info,
                        Role::Note,
                        "Synthetic tip",
                        cx,
                    ))
            }
        }
        // Each state has its own shape, never only its colour: failed is a
        // circle with ×, attention a triangle with !, done a check, pending
        // a dashed circle.
        {
            use gpui::AssetSource as _;
            let kinds = [
                StatusKind::Info,
                StatusKind::Pending,
                StatusKind::Done,
                StatusKind::Attention,
                StatusKind::Failed,
                StatusKind::Paused,
            ];
            let drawings: Vec<_> = kinds
                .iter()
                .map(|kind| {
                    crate::ui::assets::Assets
                        .load(kind.glyph_path())
                        .unwrap()
                        .unwrap_or_else(|| panic!("{kind:?} has no glyph"))
                })
                .collect();
            for (a, first) in kinds.iter().zip(&drawings) {
                for (b, second) in kinds.iter().zip(&drawings) {
                    if a != b {
                        assert_ne!(first, second, "{a:?} and {b:?} draw the same glyph");
                    }
                }
            }
            assert_eq!(StatusKind::Attention.glyph_path(), assets::STATUS_ATTENTION);
            assert_eq!(StatusKind::Failed.glyph_path(), assets::STATUS_FAILED);
        }
        cx.update(crate::ui::theme::init);
        let window = cx.add_window(|_, _| Probe);
        cx.update_window(window.into(), |_, window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            for index in 0..6 {
                let id = format!("status-{index}");
                let kind = ["Info", "Pending", "Done", "Attention", "Failed", "Paused"][index];
                let role = if kind == "Failed" { "Alert" } else { "Status" };
                assert_eq!(
                    tree.read(tree.find(&id)),
                    (role, format!("Synthetic {kind}").as_str())
                );
            }
            assert_eq!(
                tree.read(tree.find("interrupting")),
                ("Alert", "Disk space is low")
            );
            assert_eq!(tree.read(tree.find("tip")), ("Note", "Synthetic tip"));
            // Banners sit on their state's soft fill with its edge, never
            // on the full-strength state colour.
            let quads = window.painted_quads();
            let theme = cx.theme();
            let colours = brand(cx);
            for (fill, edge) in [
                (colours.warning_soft, colours.warning_border),
                (colours.info_soft, colours.info_border),
            ] {
                assert!(quads.iter().any(|quad| quad.background == fill.into()));
                assert!(quads.iter().any(|quad| quad.border_color == edge));
            }
            assert!(!quads.iter().any(|quad| {
                quad.background == theme.warning.into() || quad.background == theme.danger.into()
            }));
        })
        .unwrap();
    }

    #[gpui::test]
    fn a_long_banner_wraps_beside_its_action_in_a_narrow_window_at_largest(
        cx: &mut TestAppContext,
    ) {
        struct Probe;
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(
                    banner("tip", StatusKind::Info, Role::Note, crate::ui::TRAY_TIP, cx).child(
                        gpui::component::button::Button::new("tip-dismiss")
                            .label("Dismiss")
                            .ghost(),
                    ),
                )
            }
        }
        cx.update(crate::ui::theme::init);
        let window = cx.add_window(|_, _| Probe);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(gpui::size(gpui::px(480.), gpui::px(600.)));
        // No component Root maps Text size to the rem here: use the
        // Largest (200 %) rem directly.
        visual.update(|window, _| window.set_rem_size(gpui::px(32.)));
        dialog_gate::draw(&mut visual);
        visual.update(|window, _| {
            let width = window.viewport_size().width;
            let nodes = gpui::base::test_support::snapshots(window);
            let tip = nodes
                .iter()
                .find(|node| {
                    node.path()
                        .last()
                        .is_some_and(|part| part.to_string() == "tip")
                })
                .expect("the banner text was laid out")
                .bounds();
            let dismiss = nodes
                .iter()
                .find(|node| node.label() == Some("Dismiss"))
                .expect("the banner action was laid out")
                .bounds();
            assert!(tip.right() <= width, "{tip:?} in {width:?}");
            assert!(dismiss.right() <= width, "{dismiss:?} in {width:?}");
        });
    }

    #[gpui::test]
    fn retry_reloads_only_its_list_and_replaces_failure_with_pending(cx: &mut TestAppContext) {
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Load {
            Failed,
            Pending,
            Done,
        }

        struct Probe([Load; 2]);
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                stack().children(["models", "microphones"].into_iter().enumerate().map(
                    |(index, id)| {
                        match self.0[index] {
                            Load::Failed => retry_line(
                                id,
                                format!("Couldn't load {id}."),
                                cx,
                                move |this, _, cx| {
                                    this.0[index] = Load::Pending;
                                    cx.notify();
                                },
                            )
                            .into_any_element(),
                            Load::Pending => status_line(
                                id,
                                StatusKind::Pending,
                                format!("Looking for {id}…"),
                                cx,
                            )
                            .into_any_element(),
                            Load::Done => {
                                status_line(id, StatusKind::Done, format!("Loaded {id}."), cx)
                                    .into_any_element()
                            }
                        }
                    },
                ))
            }
        }
        cx.update(crate::ui::theme::init);
        let window = cx.add_window(|_, _| Probe([Load::Failed; 2]));
        let vcx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        dialog_gate::click(vcx, "models-retry", Role::Button);
        vcx.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("models")),
                ("Status", "Looking for models…")
            );
            assert!(tree.all("models-retry").is_empty());
            assert_eq!(
                tree.read(tree.find("microphones")),
                ("Alert", "Couldn't load microphones.")
            );
        });
        window
            .update(vcx, |probe, _, cx| {
                assert_eq!(probe.0, [Load::Pending, Load::Failed]);
                probe.0[0] = Load::Done;
                cx.notify();
            })
            .unwrap();
        vcx.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(tree.read(tree.find("models")), ("Status", "Loaded models."));
            assert_eq!(
                tree.read(tree.find("microphones-retry")),
                ("Button", "Try again: Couldn't load microphones.")
            );
        });
    }

    #[gpui::test]
    fn action_groups_render_natural_widths_sizes_and_gap_tokens(cx: &mut TestAppContext) {
        struct Probe;
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                stack()
                    .w_full()
                    .gap(GROUP_GAP)
                    .child(button("view-action", "Save", true, cx, |_, _, _| {}).primary())
                    .child(
                        h_flex()
                            .gap(CONTROL_GAP)
                            .child(button("row-first", "Edit", true, cx, |_, _, _| {}).small())
                            .child(
                                button("row-second", "Duplicate", true, cx, |_, _, _| {}).small(),
                            ),
                    )
            }
        }
        cx.update(crate::ui::theme::init);
        let window = cx.add_window(|_, _| Probe);
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let nodes = gpui::base::test_support::snapshots(window);
            let bounds = |id: &str| {
                nodes
                    .iter()
                    .find(|node| {
                        node.path()
                            .last()
                            .is_some_and(|part| part.to_string() == id)
                    })
                    .unwrap()
                    .bounds()
            };
            let view = bounds("view-action");
            let edit = bounds("row-first");
            let duplicate = bounds("row-second");
            assert_eq!(view.size.height, px(32.));
            assert_eq!(edit.size.height, px(24.));
            assert_eq!(duplicate.size.height, px(24.));
            assert!(view.size.width < window.viewport_size().width / 2.);
            assert!(duplicate.size.width > edit.size.width);
            assert_eq!(duplicate.left() - edit.right(), px(8.));
            assert_eq!(edit.top() - view.bottom(), px(12.));
        })
        .unwrap();
    }

    #[gpui::test]
    fn disabled_action_keeps_focus_and_its_reason_tooltip(cx: &mut TestAppContext) {
        const REASON: &str = "The recording is unavailable.";
        struct Probe(usize);
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                stack()
                    .child(
                        button("play", "Play", false, cx, |this, _, _| this.0 += 1)
                            .tooltip(REASON)
                            .tooltip_show_delay(Duration::from_millis(100)),
                    )
                    .child(super::super::text_line("play-reason", Role::Label, REASON))
                    .child(button("retry", "Try again", true, cx, |this, _, _| {
                        this.0 += 1
                    }))
            }
        }
        cx.update(crate::ui::theme::init);
        let clock = cx.executor().clone();
        let probe = cx.new(|_| Probe(0));
        let window =
            cx.add_window(|window, cx| gpui::component::Root::new(probe.clone(), window, cx));
        let vcx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, _| window.activate_a11y_for_tests());
        dialog_gate::draw(vcx);
        vcx.update(|window, cx| window.focus_next(cx));
        dialog_gate::draw(vcx);
        let activate = |vcx: &mut gpui::VisualTestContext| {
            for key in ["enter", "space"] {
                let keystroke = vcx.update(|_, cx| {
                    gpui::Keystroke::parse_with_os(key, cx.platform_info().os).unwrap()
                });
                vcx.simulate_event(gpui::KeyDownEvent {
                    keystroke: keystroke.clone(),
                    is_held: false,
                    prefer_character_input: false,
                });
                vcx.simulate_event(gpui::KeyUpEvent { keystroke });
            }
        };
        let play = vcx.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(tree.nodes[tree.find("play")].description, REASON);
            let native: serde_json::Value =
                serde_json::from_str(&window.debug_a11y_tree_json().unwrap()).unwrap();
            let focus = native["gpui_focus"]
                .as_str()
                .unwrap_or_else(|| panic!("keyboard focus was not assigned: {native}"));
            assert_eq!(native["nodes"][focus]["aria"]["label"], "Play");
            let play = gpui::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| {
                    node.path()
                        .last()
                        .is_some_and(|part| part.to_string() == "play")
                })
                .expect("disabled button");
            assert_eq!(play.disabled(), Some(true));
            play.bounds().center()
        });
        activate(vcx);
        assert_eq!(probe.read_with(vcx, |probe, _| probe.0), 0);
        vcx.update(|window, cx| window.focus_next(cx));
        dialog_gate::draw(vcx);
        activate(vcx);
        assert_eq!(probe.read_with(vcx, |probe, _| probe.0), 2);
        vcx.simulate_mouse_move(play, None, gpui::Modifiers::none());
        vcx.run_until_parked();
        clock.advance_clock(Duration::from_millis(600));
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(tree.read(tree.find("play-reason")), ("Label", REASON));
            assert!(
                tree.nodes.iter().any(|node| node.role == "Tooltip"),
                "{tree:?}"
            );
        });
    }

    #[gpui::test]
    fn determinate_progress_exposes_percent_and_range(cx: &mut TestAppContext) {
        struct Probe;
        impl gpui::Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                progress("model-progress", "Downloading the model", 0.42)
            }
        }

        cx.update(crate::ui::theme::init);
        let window = cx.add_window(|_, _| Probe);
        cx.update_window(window.into(), |_, window, cx| {
            window.activate_a11y_for_tests();
            window.draw(cx).clear(cx);
            let tree: serde_json::Value =
                serde_json::from_str(&window.debug_a11y_tree_json().unwrap()).unwrap();
            let node = tree["nodes"]
                .as_object()
                .unwrap()
                .values()
                .find(|node| node["aria"]["role"] == "ProgressIndicator")
                .expect("native progress node");
            assert_eq!(node["aria"]["label"], "Downloading the model");
            assert_eq!(node["aria"]["numeric_value"], 42.);
            assert_eq!(node["aria"]["min_numeric_value"], 0.);
            assert_eq!(node["aria"]["max_numeric_value"], 100.);
        })
        .unwrap();
    }

    #[gpui::test]
    fn a_pending_line_gets_a_separate_timer_after_two_seconds(cx: &mut TestAppContext) {
        struct Probe(bool);
        impl gpui::Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let kind = if self.0 {
                    StatusKind::Pending
                } else {
                    StatusKind::Done
                };
                status_line("waiting", kind, "Checking models…", cx)
            }
        }

        cx.update(crate::ui::theme::init);
        let window = cx.add_window(|_, _| Probe(true));
        let tree = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| {
                crate::ui::a11y_tree::draw_and_read(window, cx)
            })
            .unwrap()
        };
        assert!(tree(cx).all("pending-elapsed").is_empty());
        cx.executor().advance_clock(Duration::from_secs(1));
        assert!(tree(cx).all("pending-elapsed").is_empty());
        cx.executor().advance_clock(Duration::from_secs(1));
        let shown = tree(cx);
        let timers = shown.all("pending-elapsed");
        assert_eq!(timers.len(), 1);
        let timer = &shown.nodes[timers[0]];
        assert_eq!((&*timer.role, &*timer.name), ("Timer", "Waiting time"));
        // The spoken value is kept outside the live Status; another second
        // advances it without changing the Status words.
        let before = cx
            .update_window(window.into(), |_, window, cx| {
                super::super::spoken_log::while_building(|| window.draw(cx).clear(cx)).1
            })
            .unwrap();
        cx.executor().advance_clock(Duration::from_secs(1));
        let after = cx
            .update_window(window.into(), |_, window, cx| {
                super::super::spoken_log::while_building(|| window.draw(cx).clear(cx)).1
            })
            .unwrap();
        assert!(
            after.iter().any(|(id, role, words)| id == "pending-elapsed"
                && *role == Role::Timer
                && words.contains('3')),
            "{before:?} {after:?}"
        );
        window
            .update(cx, |view, _, cx| {
                view.0 = false;
                cx.notify();
            })
            .unwrap();
        assert!(tree(cx).all("pending-elapsed").is_empty());
        window
            .update(cx, |view, _, cx| {
                view.0 = true;
                cx.notify();
            })
            .unwrap();
        assert!(tree(cx).all("pending-elapsed").is_empty());
    }

    #[gpui::test]
    fn a_hidden_window_drops_the_wait_ticker_until_it_is_visible(cx: &mut TestAppContext) {
        struct Probe;
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }
        cx.update(crate::ui::theme::init);
        let window = cx.add_window(|_, _| Probe);
        let clock = window
            .update(cx, |_, window, cx| cx.new(|cx| WaitClock::new(window, cx)))
            .unwrap();
        assert!(clock.read_with(cx, |clock, _| clock.ticker.is_some()));
        cx.simulate_window_visibility_change(window.into(), gpui::WindowVisibility::Hidden);
        cx.run_until_parked();
        assert!(clock.read_with(cx, |clock, _| clock.ticker.is_none()));
        cx.executor().advance_clock(Duration::from_secs(30));
        assert!(clock.read_with(cx, |clock, _| clock.ticker.is_none()));
        cx.simulate_window_visibility_change(window.into(), gpui::WindowVisibility::Visible);
        cx.run_until_parked();
        assert!(clock.read_with(cx, |clock, _| clock.ticker.is_some()));
    }

    #[gpui::test]
    fn confirm_actions_orders_buttons_by_platform_and_keeps_right_alignment_and_ids(
        cx: &mut gpui::TestAppContext,
    ) {
        use broscribe_platform::fakes::FakeDialogButtonOrder;
        use gpui::base::TestSupportExt as _;
        use gpui::component::button::ButtonVariants as _;
        use gpui::{Context, Render, Window, size};

        struct RowHarness;
        impl Render for RowHarness {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let safe = Button::new("confirm-safe-id")
                    .label("Cancel")
                    .outline()
                    .debug_selector(|| "safe-btn".to_owned());
                let act = Button::new("confirm-act-id")
                    .label("Delete 3 recordings")
                    .danger()
                    .debug_selector(|| "act-btn".to_owned());
                div()
                    .id("row-wrap")
                    .test_support()
                    .w(px(400.))
                    .debug_selector(|| "row-wrap".to_owned())
                    .child(confirm_actions(safe, act, cx))
            }
        }

        cx.update(crate::ui::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| RowHarness);
        cx.simulate_resize(size(px(440.), px(200.)));

        let measure = |cx: &mut gpui::VisualTestContext, order: DialogButtonOrder| {
            let _fake = FakeDialogButtonOrder::new(order);
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            let wrap = cx.debug_bounds("row-wrap").expect("row-wrap painted");
            let safe = cx.debug_bounds("safe-btn").expect("safe-btn painted");
            let act = cx.debug_bounds("act-btn").expect("act-btn painted");
            let ids = cx.update(|window, _| {
                let mut buttons: Vec<_> = gpui::base::test_support::snapshots(window)
                    .into_iter()
                    .filter(|node| node.role() == Some(Role::Button))
                    .collect();
                buttons.sort_by_key(|node| (node.bounds().origin.y, node.bounds().origin.x));
                buttons
                    .into_iter()
                    .map(|node| node.path().last().expect("identified button").to_string())
                    .collect::<Vec<_>>()
            });
            (wrap, safe, act, ids)
        };

        let (wrap, safe, act, ids) = measure(cx, DialogButtonOrder::CancelFirst);
        assert!(
            safe.origin.x < act.origin.x,
            "CancelFirst must render safe before act: safe={safe:?} act={act:?}"
        );
        assert!(
            (act.right() - wrap.right()).abs() <= px(1.),
            "CancelFirst must be right-aligned: act={act:?} wrap={wrap:?}"
        );
        assert_eq!(ids, ["confirm-safe-id", "confirm-act-id"]);

        let (wrap, safe, act, ids) = measure(cx, DialogButtonOrder::ActionFirst);
        assert!(
            act.origin.x < safe.origin.x,
            "ActionFirst must render act before safe: act={act:?} safe={safe:?}"
        );
        assert!(
            (safe.right() - wrap.right()).abs() <= px(1.),
            "ActionFirst must be right-aligned: safe={safe:?} wrap={wrap:?}"
        );
        assert_eq!(ids, ["confirm-act-id", "confirm-safe-id"]);
    }
}
