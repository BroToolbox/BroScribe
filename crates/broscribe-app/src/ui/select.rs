use std::cell::RefCell;
use std::rc::Rc;

use broscribe_core::names::folded_name;
use gpui::component::IndexPath;
use gpui::component::searchable_list::{SearchableListDelegate, SearchableListItem};
use gpui::component::select::{Select, SelectEvent, SelectState};
use gpui::{
    App, AppContext as _, Context, ElementId, Entity, Focusable as _, IntoElement,
    ParentElement as _, RenderOnce, SharedString, Styled as _, Subscription, Task, Window, div,
};

use super::format::{LANGUAGE_NAMES, language_name};
use super::type_scale::{TextRole, TextRoleExt as _};

#[derive(Clone, PartialEq)]
pub(crate) struct OptionItem {
    pub value: String,
    pub label: SharedString,
    pub disabled: bool,
    search_terms: Vec<String>,
    detail: Option<SharedString>,
}

impl OptionItem {
    pub(crate) fn new(value: impl Into<String>, label: impl Into<SharedString>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            disabled: false,
            search_terms: Vec::new(),
            detail: None,
        }
    }

    pub(crate) fn search_terms(mut self, terms: impl IntoIterator<Item = String>) -> Self {
        self.search_terms = terms.into_iter().map(|term| folded_name(&term)).collect();
        self
    }

    pub(crate) fn detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail.map(Into::into);
        self
    }
}

impl SearchableListItem for OptionItem {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &String {
        &self.value
    }

    fn disabled(&self) -> bool {
        self.disabled
    }

    fn matches(&self, query: &str) -> bool {
        (self.value.is_empty() && query == "auto")
            || query.split_whitespace().all(|term| {
                folded_name(&self.label).contains(term)
                    || self.value.to_ascii_lowercase().starts_with(term)
                    || self.search_terms.iter().any(|key| key.contains(term))
            })
    }

    fn render(&self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        use gpui::component::ActiveTheme as _;
        div()
            .flex()
            .flex_col()
            .child(self.label.clone())
            .children(self.detail.iter().map(|detail| {
                div()
                    .text_role(TextRole::Caption)
                    .text_color(cx.theme().muted_foreground)
                    .child(detail.clone())
            }))
    }
}

struct Options {
    all: Vec<OptionItem>,
    shown: Vec<OptionItem>,
    language_codes: bool,
}

impl Options {
    fn new(all: Vec<OptionItem>, language_codes: bool) -> Self {
        Self {
            shown: all.clone(),
            all,
            language_codes,
        }
    }

    fn search(&mut self, query: &str) {
        let code = query.trim();
        let folded = folded_name(code);
        self.shown = self
            .all
            .iter()
            .filter(|item| item.matches(&folded))
            .cloned()
            .collect();
        if self.language_codes
            && !code.eq_ignore_ascii_case("auto")
            && valid_code(code)
            && !self
                .shown
                .iter()
                .any(|item| item.value.eq_ignore_ascii_case(code))
        {
            self.shown
                .push(OptionItem::new(code, "Use this language code"));
        }
    }
}

fn valid_code(value: &str) -> bool {
    value.len() <= broscribe_ipc::live::MAX_LANGUAGE_BYTES
        && crate::views::settings::valid_language_tag(value)
}

impl SearchableListDelegate for Options {
    type Item = OptionItem;

    fn items_count(&self, _: usize) -> usize {
        self.shown.len()
    }

    fn item(&self, index: IndexPath) -> Option<&OptionItem> {
        self.shown.get(index.row)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        OptionItem: SearchableListItem<Value = V>,
        V: PartialEq,
    {
        self.shown
            .iter()
            .position(|item| SearchableListItem::value(item) == value)
            .map(|row| IndexPath::default().row(row))
    }

    fn perform_search(&mut self, query: &str, _: &mut Window, _: &mut App) -> Task<()> {
        self.search(query);
        Task::ready(())
    }
}

type Choose = Rc<dyn Fn(&str, &mut Window, &mut App)>;

struct PickerState {
    select: Entity<SelectState<Options>>,
    options: Vec<OptionItem>,
    selected: String,
    choose: Rc<RefCell<Choose>>,
    _subscription: Subscription,
}

/// A controlled Select. Its owner retains the accepted value when a write
/// fails, and changing the option list reconciles by stable value.
#[derive(IntoElement)]
pub(crate) struct Picker {
    id: ElementId,
    label: SharedString,
    options: Vec<OptionItem>,
    selected: String,
    disabled: bool,
    language_codes: bool,
    clearable: bool,
    placeholder: Option<SharedString>,
    focus_on_mount: bool,
    focus_target: Option<Rc<RefCell<Option<gpui::FocusHandle>>>>,
    choose: Choose,
}

impl Picker {
    pub(crate) fn new<V: 'static>(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        options: Vec<OptionItem>,
        selected: impl Into<String>,
        cx: &Context<V>,
        choose: impl Fn(&mut V, &str, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let weak = cx.weak_entity();
        Self::with_handler(
            id,
            label,
            options,
            selected,
            Rc::new(move |value, window, cx| {
                let _ = weak.update(cx, |this, cx| choose(this, value, window, cx));
            }),
        )
    }

    pub(crate) fn with_handler(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        options: Vec<OptionItem>,
        selected: impl Into<String>,
        choose: Choose,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            options,
            selected: selected.into(),
            disabled: false,
            language_codes: false,
            clearable: false,
            placeholder: None,
            focus_on_mount: false,
            focus_target: None,
            choose,
        }
    }

    pub(crate) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub(crate) fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }

    pub(crate) fn clearable(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.clearable = true;
        self.placeholder = Some(placeholder.into());
        self
    }

    pub(crate) fn focus_on_mount(mut self, focus: bool) -> Self {
        self.focus_on_mount = focus;
        self
    }

    pub(crate) fn track_control_focus(
        mut self,
        target: Rc<RefCell<Option<gpui::FocusHandle>>>,
    ) -> Self {
        self.focus_target = Some(target);
        self
    }

    pub(crate) fn languages<V: 'static>(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        preferred: &[String],
        selected: &str,
        cx: &Context<V>,
        choose: impl Fn(&mut V, &str, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let selected = if selected.eq_ignore_ascii_case("auto") {
            ""
        } else {
            selected
        };
        let options = language_options(preferred, selected);
        let selected = options
            .iter()
            .find(|option| option.value.eq_ignore_ascii_case(selected))
            .map(|option| option.value.clone())
            .unwrap_or_default();
        let mut picker = Self::new(id, label, options, selected, cx, choose);
        picker.language_codes = true;
        picker
    }
}

impl RenderOnce for Picker {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let initial = self
            .options
            .iter()
            .position(|item| item.value == self.selected)
            .map(|row| IndexPath::default().row(row));
        let state = window.use_keyed_state(self.id.clone(), cx, |window, cx| {
            let select = cx.new(|cx| {
                SelectState::new(
                    Options::new(self.options.clone(), self.language_codes),
                    initial,
                    window,
                    cx,
                )
                .searchable(true)
            });
            let choose = Rc::new(RefCell::new(self.choose.clone()));
            let on_confirm = choose.clone();
            let clearable = self.clearable;
            let subscription = cx.subscribe_in(&select, window, move |_, _, event, window, cx| {
                let SelectEvent::Confirm(value) = event;
                if value.is_some() || clearable {
                    let callback = on_confirm.borrow().clone();
                    callback(value.as_deref().unwrap_or_default(), window, cx);
                }
            });
            PickerState {
                select,
                options: self.options.clone(),
                selected: self.selected.clone(),
                choose,
                _subscription: subscription,
            }
        });
        let current = state.read(cx);
        *current.choose.borrow_mut() = self.choose;
        if current.options != self.options
            || current.selected != self.selected
            || current.select.read(cx).selected_value() != initial.map(|_| &self.selected)
        {
            state.update(cx, |state, cx| {
                state.select.update(cx, |select, cx| {
                    if state.options != self.options {
                        select.set_items(
                            Options::new(self.options.clone(), self.language_codes),
                            window,
                            cx,
                        );
                    }
                    select.set_selected_value(&self.selected, window, cx);
                });
                state.options = self.options;
                state.selected = self.selected;
            });
        }
        let control = &state.read(cx).select;
        if let Some(target) = self.focus_target {
            use gpui::Focusable as _;
            *target.borrow_mut() = Some(control.focus_handle(cx));
        }
        let select = Select::new(control)
            .id(self.id)
            .accessibility_label(self.label)
            .w_full()
            .min_w_0()
            .cleanable(self.clearable)
            .disabled(self.disabled);
        if self.focus_on_mount {
            let focus = state.read(cx).select.focus_handle(cx);
            window.on_next_frame(move |window, cx| focus.focus(window, cx));
        }
        let select = match self.placeholder {
            Some(placeholder) => select.placeholder(placeholder),
            None => select,
        };
        if self.language_codes {
            select.search_placeholder("Type a language name or code, such as nl or ja")
        } else {
            select
        }
    }
}

fn language_options(preferred: &[String], selected: &str) -> Vec<OptionItem> {
    let mut options = vec![OptionItem::new("", "Detect automatically")];
    for code in preferred
        .iter()
        .map(String::as_str)
        .filter(|code| valid_code(code))
        .chain(LANGUAGE_NAMES.iter().map(|(code, _)| *code))
        .chain(std::iter::once(selected))
    {
        if !options
            .iter()
            .any(|item| item.value.eq_ignore_ascii_case(code))
        {
            options.push(OptionItem::new(code, language_name(code)));
        }
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn searchable_select_commits_a_typed_code_and_exposes_its_name(cx: &mut gpui::TestAppContext) {
        use gpui::{Render, Role};
        struct Probe(String);
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                Picker::languages(
                    "test-language",
                    "Meeting language",
                    &[],
                    &self.0,
                    cx,
                    |this, value, _, cx| {
                        this.0 = value.to_owned();
                        cx.notify();
                    },
                )
            }
        }
        cx.update(crate::ui::theme::init);
        let (view, visual) = cx.add_window_view(|_, _| Probe(String::new()));
        let inspect = |visual: &mut gpui::VisualTestContext| {
            visual.update(|window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("test-language")),
                    ("ComboBox", "Meeting language")
                );
                let nodes = gpui::base::test_support::snapshots(window);
                let select = nodes
                    .iter()
                    .find(|node| node.role() == Some(Role::ComboBox))
                    .unwrap();
                (select.bounds().center(), select.value().map(str::to_owned))
            })
        };
        let (point, value) = inspect(visual);
        assert_eq!(value.as_deref(), Some("Detect automatically"));
        // The combo box fills its width. Click the first line, where its
        // trigger lives, then type a code that wasn't in the original list.
        visual.simulate_click(gpui::point(point.x, gpui::px(12.)), Default::default());
        visual.simulate_input("yue-Hant");
        visual.simulate_keystrokes("enter");
        assert_eq!(view.read_with(visual, |view, _| view.0.clone()), "yue-Hant");
        assert_eq!(inspect(visual).1.as_deref(), Some("Other language"));
    }

    #[test]
    fn language_search_prefers_model_languages_and_accepts_typed_codes() {
        let mut options = Options::new(language_options(&["ja".into(), "pt-BR".into()], ""), true);
        assert_eq!(options.shown[0].title(), "Detect automatically");
        assert_eq!(options.shown[1].title(), "日本語 · Japanese");
        assert_eq!(
            options.shown[2].title(),
            "Português (Brasil) · Portuguese (Brazil)"
        );
        options.search("pt-BR");
        assert_eq!(options.shown.len(), 1);
        assert_eq!(options.shown[0].value, "pt-BR");
        options.search("Portuguese");
        assert_eq!(options.shown.len(), 3);
        options.search("zh-Hant-TW");
        assert_eq!(options.shown[0].value, "zh-Hant-TW");
        assert_eq!(options.shown[0].title(), "Use this language code");
        options.search("wrong code");
        assert!(options.shown.is_empty());
        assert_eq!(language_name("und"), "Other language");
    }
}
