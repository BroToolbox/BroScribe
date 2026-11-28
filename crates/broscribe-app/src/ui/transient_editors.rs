//! ACC-24: every transient editor, popover, dialog and inline form closes on
//! Escape and returns focus to its opener.
//!
//! The test reads the app's shipping sources with `syn` and finds every place
//! that opens one: `Popover::new`, `window.open_dialog`,
//! `window.open_alert_dialog`, `kit::confirm_dialog`, text fields and menus.
//! [`SURFACES`] records each opener and its Escape behavior. A new call to a
//! tracked constructor fails the test until the registry accounts for it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use syn::visit::{self, Visit};

/// What opens the surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Opener {
    /// gpui-component `Popover::new`.
    Popover,
    /// `window.open_dialog`.
    Dialog,
    /// `window.open_alert_dialog`.
    AlertDialog,
    /// `kit::confirm_dialog`: Cancel focused, Escape and ⌘. cancel.
    Confirm,
    /// A text field (`InputState::new`).
    Field,
    /// A menu, including submenus and context menus.
    Menu,
}

use Opener::{AlertDialog, Confirm, Dialog, Field, Menu, Popover};

/// Each field is either persistent, dismissed by the named parent, or
/// dismissed by an editor-specific handler. Dialogs and popovers use their
/// component's Escape behavior unless a view overrides it.
#[derive(Clone, Copy)]
enum Escape {
    Component,
    SharedConfirmation,
    In(&'static str),
    Persistent,
}

use Escape::{Component, In, Persistent, SharedConfirmation};

type Surface = (&'static str, &'static str, Opener, usize, Escape);

#[rustfmt::skip]
const SURFACES: &[Surface] = &[
    ("menu.rs", "show_shortcuts", Dialog, 1, Component),
    ("ui.rs", "open_main_menu", Menu, 1, Component),
    ("ui.rs", "processing_status", Popover, 1, Component),
    ("ui.rs", "sync_import_dialog", Dialog, 1, Component),
    ("ui.rs", "toolbar", Popover, 1, Component),
    ("ui/kit.rs", "confirm_dialog", AlertDialog, 1, Component),
    ("ui/kit.rs", "open", Menu, 1, Component),
    ("views/delete_meeting.rs", "present", Confirm, 1, SharedConfirmation),
    ("views/generate.rs", "credit_instructions", Popover, 1, Component),
    ("views/generate.rs", "new", Field, 2, In("Run options")),
    ("views/generate.rs", "open_options", Dialog, 1, Component),
    ("views/generate.rs", "options_trigger", Popover, 1, Component),
    ("views/import_sheet.rs", "new", Field, 2, In("Import dialog")),
    ("views/integrations.rs", "account_menu", Menu, 1, Component),
    ("views/integrations.rs", "ensure_direct_inputs", Field, 1, Persistent),
    ("views/integrations.rs", "open_catalog", Dialog, 1, Component),
    ("views/integrations.rs", "open_confirmation", Confirm, 1, SharedConfirmation),
    ("views/integrations.rs", "open_delegated", Dialog, 1, Component),
    ("views/integrations.rs", "open_delegated", Field, 2, In("delegated calendar dialog")),
    ("views/integrations.rs", "secret_input", Field, 1, Persistent),
    ("views/library.rs", "filters_button", Popover, 1, Component),
    ("views/library.rs", "import_run_row", Menu, 1, Component),
    ("views/library.rs", "new", Field, 1, Persistent),
    ("views/library.rs", "open_row_menu", Menu, 1, Component),
    ("views/library.rs", "render_row", Menu, 1, Component),
    ("views/library.rs", "toolbar_controls", Menu, 1, Component),
    ("views/meeting.rs", "meeting_tabs", Menu, 1, Component),
    ("views/meeting.rs", "new", Field, 2, In("Add tag and Find in transcript")),
    ("views/meeting.rs", "present_export_from", Dialog, 1, Component),
    ("views/meeting.rs", "start_rename_from", Field, 1, In("Rename meeting")),
    ("views/meeting/header.rs", "more_menu", Menu, 1, Component),
    ("views/meeting/overview.rs", "action_controls", Menu, 1, Component),
    ("views/meeting/overview.rs", "action_controls", Popover, 1, Component),
    ("views/meeting/overview.rs", "overview_actions", Menu, 2, Component),
    ("views/meeting/saved_transcript.rs", "line_naming_popover", Popover, 1, Component),
    ("views/meeting/saved_transcript.rs", "open_transcript_menu", Menu, 1, Component),
    ("views/meeting/speakers.rs", "open_speaker_menu", Menu, 2, Component),
    ("views/panel/view.rs", "live_row", Menu, 1, Component),
    ("views/panel/view.rs", "more_actions", Popover, 1, Component),
    ("views/panel/view.rs", "new", Field, 2, Persistent),
    ("views/panel/view.rs", "open_transcript_menu", Menu, 1, Component),
    ("views/panel/view.rs", "present_discard", Confirm, 1, SharedConfirmation),
    ("views/panel/view.rs", "present_journal_resolution", Confirm, 1, SharedConfirmation),
    ("views/panel/view.rs", "render", Popover, 1, Component),
    ("views/panel/view.rs", "sync_titles", Field, 1, In("recording title editor")),
    ("views/people.rs", "ask_forget_from", Confirm, 1, SharedConfirmation),
    ("views/people.rs", "directory_popover", Popover, 1, Component),
    ("views/people.rs", "new", Field, 1, In("directory popover")),
    ("views/people.rs", "new", Field, 1, Persistent),
    ("views/people.rs", "open_editor", Field, 4, In("person editor")),
    ("views/person_picker.rs", "new_unfocused", Field, 1, In("person picker")),
    ("views/providers.rs", "ask_to_remove_key", Confirm, 1, SharedConfirmation),
    ("views/providers.rs", "masked_key_input", Field, 1, Persistent),
    ("views/providers.rs", "open_http_form", Dialog, 1, Component),
    ("views/providers.rs", "open_http_form", Field, 2, In("provider dialog")),
    ("views/providers.rs", "show_terms", Confirm, 1, SharedConfirmation),
    ("views/settings.rs", "confirm_enrollment_delete", Confirm, 1, SharedConfirmation),
    ("views/settings.rs", "confirm_library", Confirm, 1, SharedConfirmation),
    ("views/settings.rs", "confirm_model_removal", Confirm, 1, SharedConfirmation),
    ("views/settings.rs", "new", Field, 4, Persistent),
    ("views/settings.rs", "open_retention_confirm_dialog", Confirm, 1, SharedConfirmation),
    ("views/settings.rs", "request_model", Confirm, 1, SharedConfirmation),
    ("views/settings.rs", "should_close", AlertDialog, 1, Component),
    ("views/settings/speaker_grouping.rs", "new", Field, 2, In("Custom speaker grouping")),
    ("views/settings/uninstall.rs", "open_uninstall", AlertDialog, 1, Component),
    ("views/setup.rs", "all_steps", Menu, 1, Component),
    ("views/setup.rs", "new", Field, 5, In("Setup Assistant")),
    ("views/templates.rs", "confirm_template_change", Confirm, 1, SharedConfirmation),
    ("views/templates.rs", "mount_editor", Dialog, 1, Component),
    ("views/templates.rs", "open_editor", Field, 4, In("template editor dialog")),
    ("views/templates.rs", "render", Menu, 1, Component),
    ("views/templates.rs", "request_editor_close", Confirm, 1, SharedConfirmation),
    ("views/update.rs", "open_replacement", Confirm, 1, SharedConfirmation),
];

/// One opener found in the sources: file, enclosing function, kind.
type Found = (String, String, Opener);

#[derive(Default)]
struct Openers {
    functions: Vec<String>,
    found: Vec<(String, Opener)>,
}

/// Whether `attrs` hold a `#[cfg(…)]` that only holds under `test`.
fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg") && {
            let tokens = attr
                .meta
                .require_list()
                .map(|list| list.tokens.to_string())
                .unwrap_or_default();
            tokens.contains("test") && !tokens.contains("not")
        }
    })
}

impl Openers {
    fn record(&mut self, opener: Opener) {
        let function = self.functions.last().cloned().unwrap_or_default();
        self.found.push((function, opener));
    }
}

impl<'ast> Visit<'ast> for Openers {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !test_only(&item.attrs) {
            visit::visit_item_mod(self, item);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if !test_only(&item.attrs) {
            visit::visit_item_impl(self, item);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if !test_only(&item.attrs) {
            self.functions.push(item.sig.ident.to_string());
            visit::visit_item_fn(self, item);
            self.functions.pop();
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if !test_only(&item.attrs) {
            self.functions.push(item.sig.ident.to_string());
            visit::visit_impl_item_fn(self, item);
            self.functions.pop();
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*call.func {
            let names: Vec<String> = path
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            let tail: Vec<&str> = names.iter().rev().take(2).map(String::as_str).collect();
            match tail.as_slice() {
                ["new", "Popover"] => self.record(Opener::Popover),
                ["new", "InputState" | "TextareaState"] => self.record(Opener::Field),
                ["build", "PopupMenu"] => self.record(Opener::Menu),
                ["confirm_dialog", ..] => self.record(Opener::Confirm),
                _ => {}
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        match call.method.to_string().as_str() {
            "open_dialog" => self.record(Opener::Dialog),
            "open_alert_dialog" => self.record(Opener::AlertDialog),
            "context_menu" | "dropdown_menu" | "dropdown_menu_with_anchor" | "submenu" => {
                self.record(Opener::Menu);
            }
            _ => {}
        }
        visit::visit_expr_method_call(self, call);
    }
}

/// The app's shipping sources, relative to `src/`, without test files.
fn sources() -> Vec<(String, String)> {
    fn collect(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, found);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect(&src, &mut files);
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let name = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (name, std::fs::read_to_string(&path).unwrap())
        })
        .filter(|(name, _)| {
            !name.ends_with("_tests.rs")
                && name != "test_support.rs"
                && name != "ui/transient_editors.rs"
        })
        .collect()
}

/// Every opener in the shipping sources.
fn found_openers() -> Vec<Found> {
    let mut all = Vec::new();
    for (name, source) in sources() {
        let file = syn::parse_file(&source).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut openers = Openers::default();
        openers.visit_file(&file);
        all.extend(
            openers
                .found
                .into_iter()
                .map(|(function, opener)| (name.clone(), function, opener)),
        );
    }
    all
}

fn count(items: impl IntoIterator<Item = Found>) -> BTreeMap<Found, usize> {
    let mut counts = BTreeMap::new();
    for item in items {
        *counts.entry(item).or_default() += 1;
    }
    counts
}

#[test]
fn every_transient_editor_closes_on_escape() {
    let found = count(found_openers());
    let listed = count(
        SURFACES
            .iter()
            .flat_map(|&(file, function, opener, n, escape)| {
                match (opener, escape) {
                    (Field, In(parent)) => assert!(!parent.is_empty()),
                    (Field, Persistent)
                    | (Confirm, SharedConfirmation)
                    | (Dialog | AlertDialog | Popover | Menu, Component) => {}
                    _ => panic!("Missing Escape behavior for {file}::{function}"),
                }
                std::iter::repeat_n((file.to_owned(), function.to_owned(), opener), n)
            }),
    );
    let mut missing = Vec::new();
    for (key, n) in &found {
        let have = listed.get(key).copied().unwrap_or(0);
        if have < *n {
            missing.push(format!(
                "    surface({:?}, {:?}, {:?}, …), // {} unlisted",
                key.0,
                key.1,
                key.2,
                n - have
            ));
        }
    }
    let stale: Vec<_> = listed
        .iter()
        .filter(|(key, n)| found.get(*key).copied().unwrap_or(0) < **n)
        .map(|(key, _)| format!("{key:?}"))
        .collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "every transient surface needs Escape and a test that proves it.\n\
         Unlisted openers:\n{}\nListed but not found:\n{}",
        missing.join("\n"),
        stale.join("\n")
    );
}

/// Mounted checks shared by the Escape tests here and in the views.
pub(crate) mod check {
    use gpui::component::WindowExt as _;
    use gpui::{FocusHandle, VisualTestContext};

    /// A dialog is open; Escape closes it and focus is back on `opener`.
    pub(crate) fn escape_closes_dialog(vcx: &mut VisualTestContext, opener: &FocusHandle) {
        vcx.run_until_parked();
        assert!(
            vcx.update(|window, cx| window.has_active_dialog(cx)),
            "the dialog is open"
        );
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            assert!(!window.has_active_dialog(cx), "Escape closed the dialog");
            assert!(opener.is_focused(window), "focus is back on the opener");
        });
    }

    /// The focused element's ID as the accessibility tree records it
    /// (`Name("tag")`, `NamedInteger("tag", 0)`), if it has one.
    pub(crate) fn focused_element(vcx: &mut VisualTestContext) -> Option<String> {
        vcx.update(|window, cx| {
            window.activate_a11y_for_tests();
            window.draw(cx).clear(cx);
            let tree: serde_json::Value =
                serde_json::from_str(&window.debug_a11y_tree_json()?).ok()?;
            let focus = tree["gpui_focus"].as_str()?;
            tree["nodes"][focus]["element_id"]
                .as_str()
                .map(str::to_owned)
        })
    }

    /// Presses Tab until the element named `Name(id)` has focus.
    pub(crate) fn tab_to(vcx: &mut VisualTestContext, id: &str) {
        let wanted = format!("Name({id:?})");
        for _ in 0..80 {
            if focused_element(vcx).as_deref() == Some(wanted.as_str()) {
                return;
            }
            vcx.simulate_keystrokes("tab");
            vcx.run_until_parked();
        }
        panic!("Tab never reached {id}");
    }

    /// Opens the surface with the keyboard from the button `trigger` (Tab to
    /// it, Return), checks `content` is drawn, then that Escape closes it and
    /// leaves focus on `trigger`.
    pub(crate) fn keyboard_open_then_escape(
        vcx: &mut VisualTestContext,
        trigger: &str,
        content: &str,
    ) {
        tab_to(vcx, trigger);
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(drawn(vcx, content), "{trigger} opened {content}");
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(!drawn(vcx, content), "Escape closed {content}");
        assert_eq!(
            focused_element(vcx),
            Some(format!("Name({trigger:?})")),
            "focus is back on {trigger}"
        );
    }

    /// Whether an element with the ID `id` is drawn, by the accessibility
    /// tree (element IDs as the app writes them: `tag`, `tag-0`).
    pub(crate) fn drawn(vcx: &mut VisualTestContext, id: &str) -> bool {
        vcx.update(|window, cx| {
            !crate::ui::a11y_tree::draw_and_read(window, cx)
                .all(id)
                .is_empty()
        })
    }
}

/// A focusable opener for surfaces opened outside any view.
struct OpenerView {
    focus: gpui::FocusHandle,
}

impl gpui::Render for OpenerView {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
        gpui::div()
            .id("opener")
            .track_focus(&self.focus)
            .size_full()
            .child("Synthetic opener")
    }
}

/// The Keyboard shortcuts dialog closes on Escape and gives focus back.
#[gpui::test]
fn keyboard_shortcuts_dialog_closes_on_escape(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.set_platform_info(gpui::PlatformInfo {
        os: gpui::OperatingSystem::Mac,
        target: gpui::PlatformTarget::Native,
    });
    cx.update(|cx| {
        super::theme::init(cx);
        cx.bind_keys(super::keymap::key_bindings_for(
            broscribe_platform::HostPlatform::MacOs,
            cx,
        ));
    });
    let opener = cx.update(|cx| cx.focus_handle());
    let (_, vcx) = cx.add_window_view({
        let opener = opener.clone();
        move |window, cx| {
            let view = cx.new(|_| OpenerView { focus: opener });
            gpui::component::Root::new(view, window, cx)
        }
    });
    vcx.update(|window, cx| {
        opener.focus(window, cx);
        crate::menu::show_shortcuts(window.window_handle(), cx);
    });
    vcx.run_until_parked();
    assert!(check::drawn(vcx, "shortcuts-title"));
    check::escape_closes_dialog(vcx, &opener);

    vcx.update(|window, cx| crate::menu::show_shortcuts(window.window_handle(), cx));
    vcx.run_until_parked();
    assert!(check::drawn(vcx, "shortcuts-title"));
    vcx.simulate_keystrokes("cmd-.");
    vcx.run_until_parked();
    vcx.update(|window, _| assert!(opener.is_focused(window)));
    assert!(!check::drawn(vcx, "shortcuts-title"));
}
