//! About BroScribe (SH24): the running version and architecture, the build's
//! number and target, how it is packaged and signed, and the third-party
//! licences it ships; and "Copy diagnostics" (PL39).
//!
//! Every fact comes from the platform's About reader
//! ([`PlatformAboutReader`]), run off the UI loop; this view only words it.
//! A development signature is never presented as distribution trust, and
//! notarization is stated only as the reader established it.
//!
//! Diagnostics are the counts-only report of `broscribe diagnostics` for the
//! running app's library, through the [`CliRunner`] the caller hands over.
//! It runs only when the user asks and goes to the clipboard only; a window
//! opened without a runner (a launch that failed before it had a library)
//! offers no diagnostics rather than guessing a library.

use std::sync::Arc;

use broscribe_platform::about::{
    AboutDocumentKind, AboutFacts, BuildInfoState, LicenceInventoryState, LocationNote, Mismatch,
    Notarization, Packaging, PlatformAboutReader, SealState, SignatureFacts, SignatureKind,
};
use gpui::component::button::Button;
use gpui::component::{ActiveTheme as _, Disableable as _, Root};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Bounds, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Role, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Task, TitlebarOptions, Window, WindowBounds,
    WindowHandle, WindowOptions, div,
};

use crate::jobs::{ChildKind, CliRunner, core_user_text, failure_text};
use crate::ui::kit::fact_row;
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::ui::{text_line, title, windows};

/// The About window's title: in the native title bar and, where the window
/// system leaves decorations to the app, in the bar BroScribe draws
/// ([`crate::ui::chrome::framed`]).
const WINDOW_TITLE: &str = "About BroScribe";

/// This executable's version, given to the platform's About reader.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How the build is signed and whether the signature still seals it.
pub fn signature_line(signature: &SignatureFacts) -> String {
    let team = |team: &Option<String>| {
        team.as_deref()
            .map(|team| format!(" (team {team})"))
            .unwrap_or_default()
    };
    let kind = match &signature.kind {
        SignatureKind::Unsigned => "Not signed".to_owned(),
        SignatureKind::AdHoc => "Ad-hoc signed: a local build without a signing certificate".into(),
        SignatureKind::AppleDevelopment { team: t } => format!(
            "Apple Development certificate{}: for development only, not distribution",
            team(t)
        ),
        SignatureKind::DeveloperId { team: t } => format!("Developer ID certificate{}", team(t)),
        SignatureKind::AppleOther { team: t } => {
            format!("Apple-issued certificate of another type{}", team(t))
        }
        SignatureKind::Authenticode {
            fingerprint: Some(fingerprint),
        } => format!("Authenticode signed (leaf certificate SHA-256 {fingerprint})"),
        SignatureKind::Authenticode { fingerprint: None } => "Authenticode signed".into(),
        SignatureKind::ManifestOnly => {
            "No operating-system code signature on this system; releases are verified by their signed manifest".into()
        }
        SignatureKind::Unknown { reason } => format!("Signature not established: {reason}"),
    };
    match signature.seal {
        SealState::Intact => format!("{kind}. The signature matches the app's files."),
        SealState::Broken { .. } => {
            format!("{kind}. The signature no longer matches the app's files.")
        }
        SealState::NotChecked => format!("{kind}. The file match wasn't checked."),
    }
}

/// Whether Apple notarized the build, as far as the reader established.
pub fn notarization_line(notarization: Notarization) -> &'static str {
    match notarization {
        Notarization::NotApplicable => "Not applicable on this system",
        Notarization::NotEligible => "Not eligible: Apple notarizes only Developer ID builds",
        Notarization::Unknown => "Unknown: the signature alone doesn't show notarization",
    }
}

pub fn packaging_line(packaging: Packaging, notes: &[LocationNote]) -> String {
    let mut line = match packaging {
        Packaging::AppBundle => "App bundle",
        Packaging::InstallFolder => "Install folder",
        Packaging::Unbundled => "Not installed: a development build run in place",
    }
    .to_owned();
    for note in notes {
        line.push_str(match note {
            LocationNote::Translocated => {
                "; running from a temporary location (move it to Applications)"
            }
            LocationNote::ReadOnlyVolume => "; on a read-only volume",
            LocationNote::NetworkVolume => "; on a network volume",
            LocationNote::Temporary => "; in a temporary folder",
        });
    }
    line
}

pub fn mismatch_line(mismatch: &Mismatch) -> String {
    match mismatch {
        Mismatch::AppVersion {
            running,
            build_info,
        } => format!("This app is version {running}, but its build information says {build_info}."),
        Mismatch::BundleVersion { bundle, build_info } => format!(
            "The bundle says version {bundle}, but the build information says {build_info}."
        ),
        Mismatch::BundleBuild { bundle, build_info } => {
            format!("The bundle says build {bundle}, but the build information says {build_info}.")
        }
        Mismatch::Target { .. } => "The build information names a different platform.".into(),
        Mismatch::InventoryTarget { .. } => {
            "The licence inventory was made for a different build platform.".into()
        }
    }
}

/// Where "Copy diagnostics" stands.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DiagnosticsCopy {
    Idle,
    Running,
    Copied,
    Failed(String),
}

pub struct AboutView {
    /// `None` while the platform reads the facts; a failure holds its cause
    /// in words (`core_user_text`), never the error itself.
    facts: Option<Result<AboutFacts, String>>,
    _load: Task<()>,
    /// The app's CLI for its library; `None`: no diagnostics.
    diagnostics: Option<CliRunner>,
    agent: Option<Entity<crate::daemon::DaemonClient>>,
    copy: DiagnosticsCopy,
    licence_counts_open: bool,
    help: Option<HelpDestination>,
    diagnostics_focus: gpui::FocusHandle,
    licences_focus: gpui::FocusHandle,
    scroll: gpui::ScrollHandle,
    _copy: Option<Task<()>>,
}

#[derive(Clone, Copy)]
enum HelpDestination {
    Diagnostics,
    Licences,
}

impl AboutView {
    /// `diagnostics`: the running app's CLI runner (its library), or `None`
    /// when there is none, which leaves "Copy diagnostics" unavailable.
    pub fn new(
        reader: Arc<dyn PlatformAboutReader>,
        diagnostics: Option<CliRunner>,
        cx: &mut Context<Self>,
    ) -> Self {
        let read = cx.background_executor().spawn(async move {
            reader.read().map_err(|e| {
                tracing::warn!(code = e.code(), "reading the About facts");
                core_user_text(&e)
            })
        });
        let load = cx.spawn(async move |this, cx| {
            let facts = read.await;
            this.update(cx, |this, cx| {
                this.facts = Some(facts);
                cx.notify();
            })
            .ok();
        });
        Self {
            facts: None,
            _load: load,
            diagnostics,
            agent: None,
            copy: DiagnosticsCopy::Idle,
            licence_counts_open: false,
            help: None,
            diagnostics_focus: cx.focus_handle(),
            licences_focus: cx.focus_handle(),
            scroll: gpui::ScrollHandle::new(),
            _copy: None,
        }
    }

    /// The facts once read.
    pub fn facts(&self) -> Option<&Result<AboutFacts, String>> {
        self.facts.as_ref()
    }

    fn watch_agent(&mut self, agent: Entity<crate::daemon::DaemonClient>, cx: &mut Context<Self>) {
        cx.observe(&agent, |_, _, cx| cx.notify()).detach();
        self.agent = Some(agent);
    }

    /// Runs `broscribe diagnostics` for the app's library off the UI loop
    /// and copies its report to the clipboard. Nothing without a runner or
    /// while a copy runs.
    pub fn copy_diagnostics(&mut self, cx: &mut Context<Self>) {
        let Some(cli) = &self.diagnostics else {
            return;
        };
        if self.copy == DiagnosticsCopy::Running {
            return;
        }
        let run = cli.run(
            vec!["diagnostics".into()],
            ChildKind::Command,
            cx.background_executor(),
        );
        self.copy = DiagnosticsCopy::Running;
        self._copy = Some(cx.spawn(async move |this, cx| {
            let report = run.await;
            this.update(cx, |this, cx| {
                this.copy = match report.map_err(|e| e.user_text()).and_then(|v| {
                    serde_json::to_string_pretty(&v).map_err(|e| {
                        tracing::warn!(error = %e, "formatting diagnostics");
                        String::new()
                    })
                }) {
                    Ok(text) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        DiagnosticsCopy::Copied
                    }
                    Err(why) => DiagnosticsCopy::Failed(why),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// "Copy diagnostics", what it copies and where it stands.
    fn diagnostics_section(&self, cx: &Context<Self>) -> Stateful<gpui::Div> {
        let available = self.diagnostics.is_some();
        let running = self.copy == DiagnosticsCopy::Running;
        let note = if available {
            "Copies a technical report of counts and states only: no names, titles, text, \
             paths or account details."
        } else {
            "Diagnostics are unavailable: BroScribe did not start with a library."
        };
        div()
            .id("about-diagnostics")
            .role(Role::Group)
            .aria_label("Diagnostics")
            .flex()
            .flex_col()
            .gap_1()
            .child(crate::ui::heading(
                "about-diagnostics-heading",
                "Diagnostics",
            ))
            .child(
                text_line("about-diagnostics-note", Role::Label, note).text_role(TextRole::Caption),
            )
            .child(
                Button::new("about-copy-diagnostics")
                    .label("Copy diagnostics")
                    .track_focus(&self.diagnostics_focus)
                    .self_start()
                    .disabled(!available || running)
                    .on_click(cx.listener(|this, _, _, cx| this.copy_diagnostics(cx))),
            )
            .children(self.diagnostics_status(cx))
    }

    /// Where "Copy diagnostics" stands, its message as its accessible text:
    /// a Status while it runs and once copied, an Alert when it failed.
    fn diagnostics_status(&self, cx: &App) -> Option<Stateful<gpui::Div>> {
        let (error, text) = match &self.copy {
            DiagnosticsCopy::Idle => return None,
            DiagnosticsCopy::Running => (false, "Collecting diagnostics…".to_owned()),
            DiagnosticsCopy::Copied => (false, "Diagnostics copied.".to_owned()),
            DiagnosticsCopy::Failed(why) => {
                (true, failure_text("Couldn't collect diagnostics.", why, ""))
            }
        };
        Some(crate::ui::kit::status_line(
            "about-diagnostics-status",
            if error {
                crate::ui::kit::StatusKind::Failed
            } else if matches!(self.copy, DiagnosticsCopy::Running) {
                crate::ui::kit::StatusKind::Pending
            } else {
                crate::ui::kit::StatusKind::Done
            },
            text,
            cx,
        ))
    }

    /// Every line of About above its buttons, in order: the name, the
    /// facts, and the loading, failure, mismatch and licence states. Each
    /// is an identified node named by its own text (a fact by its field
    /// and value).
    fn lines(&self, cx: &App) -> Vec<Stateful<gpui::Div>> {
        let alert = |id: SharedString, text: String| {
            crate::ui::kit::status_line(id, crate::ui::kit::StatusKind::Failed, text, cx)
        };
        // One "Field: value" node per fact (SYS-40).
        let fact =
            |id: &'static str, label: &'static str, value: String| fact_row(id, label, value, cx);
        let mut lines = vec![crate::ui::heading(
            "about-build-heading",
            "Build and signing",
        )];
        let facts = match &self.facts {
            None => {
                lines.push(crate::ui::kit::status_line(
                    "about-loading",
                    crate::ui::kit::StatusKind::Pending,
                    "Reading build, signing and licence information…",
                    cx,
                ));
                return lines;
            }
            Some(Err(why)) => {
                lines.push(alert(
                    "about-unavailable".into(),
                    failure_text(
                        "Couldn't read the build, signing and licence information.",
                        why,
                        "",
                    ),
                ));
                return lines;
            }
            Some(Ok(facts)) => facts,
        };
        let architecture = &facts.architecture;
        lines.extend([
            fact(
                "about-architecture",
                "Architecture",
                match (architecture.os.as_str(), architecture.arch.as_str()) {
                    ("macos", "aarch64") => "Apple silicon (arm64)".into(),
                    ("macos", "x86_64") => "Intel (x86-64)".into(),
                    (_, "aarch64") => "Arm (arm64)".into(),
                    (_, "x86_64") => "x86-64".into(),
                    (_, "x86") => "x86".into(),
                    _ => "Unknown".into(),
                },
            ),
            fact(
                "about-translation",
                "Translation",
                match architecture.translated {
                    Some(true) => "Running translated",
                    Some(false) => "Running natively",
                    None => "Unknown",
                }
                .to_owned(),
            ),
            fact(
                "about-build",
                "Build",
                match &facts.build {
                    BuildInfoState::Present(info) => info.build.to_string(),
                    BuildInfoState::Missing => "No build information".to_owned(),
                    BuildInfoState::Invalid { reason } => {
                        format!("Unreadable build information: {reason}")
                    }
                },
            ),
            fact(
                "about-installation",
                "Installation",
                packaging_line(facts.packaging, &facts.location_notes),
            ),
            fact("about-signing", "Signing", signature_line(&facts.signature)),
            fact(
                "about-notarization",
                "Notarization",
                notarization_line(facts.signature.notarization).to_owned(),
            ),
        ]);
        lines.extend(facts.mismatches().iter().enumerate().map(|(i, mismatch)| {
            alert(
                format!("about-mismatch-{i}").into(),
                mismatch_line(mismatch),
            )
        }));
        lines.push(crate::ui::heading(
            "about-licences-heading",
            "Third-party licences",
        ));
        match &facts.licences {
            LicenceInventoryState::Present(inventory) => {
                lines.push(
                    text_line(
                        "about-licences",
                        Role::Status,
                        format!("Third-party licences · {} components", inventory.components),
                    )
                    .font_weight(FontWeight::SEMIBOLD),
                );
                if !inventory.complete {
                    lines.push(alert(
                        "about-licences-incomplete".into(),
                        format!(
                            "The licence inventory is incomplete: {} unresolved.",
                            inventory.unresolved
                        ),
                    ));
                }
            }
            LicenceInventoryState::Missing => lines.push(alert(
                "about-licences-missing".into(),
                "This build carries no licence inventory.".into(),
            )),
            LicenceInventoryState::Invalid { reason } => lines.push(alert(
                "about-licences-invalid".into(),
                format!("The licence inventory is unreadable: {reason}"),
            )),
        }
        lines
    }
}

fn open_button(facts: &AboutFacts, kind: AboutDocumentKind) -> Button {
    let (id, label) = match kind {
        AboutDocumentKind::Licence => ("about-open-licence", "Open licence"),
        AboutDocumentKind::Notice => ("about-open-notice", "Open notice"),
        AboutDocumentKind::ThirdPartyNotices => ("about-open-notices", "Open third-party notices"),
    };
    let path = facts
        .documents
        .iter()
        .find(|document| document.kind == kind)
        .map(|document| document.path().to_path_buf());
    Button::new(id)
        .label(label)
        .disabled(path.is_none())
        .on_click(move |_, _, cx: &mut App| {
            if let Some(path) = &path {
                cx.open_with_system(path);
            }
        })
}

impl Render for AboutView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.help {
            Some(HelpDestination::Diagnostics) => {
                self.help = None;
                self.scroll.scroll_to_bottom();
                let focus = self.diagnostics_focus.clone();
                window.on_next_frame(move |window, cx| focus.focus(window, cx));
            }
            Some(HelpDestination::Licences) if self.facts.is_some() => {
                self.help = None;
                if let Some(doc) = self
                    .facts
                    .as_ref()
                    .and_then(|facts| facts.as_ref().ok())
                    .and_then(|facts| {
                        facts
                            .documents
                            .iter()
                            .find(|doc| doc.kind == AboutDocumentKind::ThirdPartyNotices)
                    })
                {
                    let path = doc.path().to_path_buf();
                    cx.defer(move |cx| cx.open_with_system(&path));
                } else {
                    self.licence_counts_open = true;
                    self.scroll.scroll_to_bottom();
                    let focus = self.licences_focus.clone();
                    window.on_next_frame(move |window, cx| focus.focus(window, cx));
                }
            }
            _ => {}
        }
        let diagnostics = self.diagnostics_section(cx);
        let theme = cx.theme();
        let header = div()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .pb_3()
            .child(crate::ui::kit::mascot(
                crate::ui::assets::TILE_HERO,
                crate::ui::kit::ABOUT_TILE_SIZE,
            ))
            .child(title("about-name", "BroScribe"))
            .child(
                text_line(
                    "about-version",
                    Role::Label,
                    format!("Version {APP_VERSION}"),
                )
                .text_role(TextRole::Caption)
                .text_color(theme.muted_foreground),
            )
            .children(
                self.agent
                    .as_ref()
                    .and_then(|agent| agent.read(cx).incompatible_agent)
                    .map(|protocol| {
                        text_line(
                            "about-agent-protocol",
                            Role::Label,
                            format!(
                                "BroScribe Agent protocol {} · App protocol {}",
                                protocol.map_or_else(
                                    || "unknown".into(),
                                    |protocol| protocol.to_string()
                                ),
                                broscribe_ipc::PROTOCOL_VERSION
                            ),
                        )
                        .text_role(TextRole::Caption)
                    }),
            )
            .child(
                text_line(
                    "about-license",
                    Role::Label,
                    "Licensed under the Apache License 2.0",
                )
                .text_role(TextRole::Caption)
                .text_color(theme.muted_foreground),
            );
        let view = div()
            .id("about")
            .text_role(TextRole::Body)
            .role(Role::Main)
            .aria_label("About BroScribe")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(header)
            // Build and signing, and the licences, are cards on the desk.
            .child(crate::ui::kit::card("about-build-card", cx).children(self.lines(cx)))
            .when_some(
                self.facts.as_ref().and_then(|facts| facts.as_ref().ok()),
                |el, facts| {
                    let disclosure = match &facts.licences {
                        LicenceInventoryState::Present(inventory) => Some(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(crate::ui::kit::disclosure(
                                    "about-licence-toggle",
                                    "Licence counts",
                                    self.licence_counts_open,
                                    cx,
                                    |this, _, cx| {
                                        this.licence_counts_open = !this.licence_counts_open;
                                        cx.notify();
                                    },
                                ))
                                .when(self.licence_counts_open, |el| {
                                    el.children(inventory.by_licence.iter().enumerate().map(
                                        |(i, (licence, count))| {
                                            text_line(
                                                ("about-licence", i),
                                                Role::Label,
                                                format!("{licence}: {count}"),
                                            )
                                            .text_role(TextRole::Caption)
                                        },
                                    ))
                                }),
                        ),
                        _ => None,
                    };
                    el.child(
                        crate::ui::kit::card("about-third-party-licences", cx)
                            .role(Role::Group)
                            .aria_label("Third-party licences")
                            .track_focus(&self.licences_focus)
                            .children(disclosure)
                            .child(
                                div().flex().flex_wrap().gap_2().children(
                                    [
                                        AboutDocumentKind::ThirdPartyNotices,
                                        AboutDocumentKind::Licence,
                                        AboutDocumentKind::Notice,
                                    ]
                                    .map(|kind| open_button(facts, kind)),
                                ),
                            ),
                    )
                },
            )
            .child(diagnostics);
        crate::ui::chrome::framed(WINDOW_TITLE, view, window)
    }
}

/// Where and how About opens.
fn about_window_options(cx: &App) -> WindowOptions {
    let (bounds, display_id) = match crate::ui_state::about_window_bounds(windows::ABOUT_SIZE, cx) {
        Some((bounds, display)) => (bounds, Some(display)),
        None => (
            Bounds::centered(None, windows::px_size(windows::ABOUT_SIZE), cx),
            None,
        ),
    };
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        display_id,
        window_min_size: Some(windows::px_size(windows::ABOUT_MIN)),
        is_minimizable: false,
        titlebar: Some(TitlebarOptions {
            title: Some(crate::ui::windows::title(WINDOW_TITLE).into()),
            ..Default::default()
        }),
        ..crate::ui::assets::window_options()
    }
}

/// Opens the About window (wrapped in gpui-component's [`Root`]).
/// `diagnostics`: the running app's CLI runner for its library; `None`
/// (no library, e.g. a failed launch) leaves "Copy diagnostics" unavailable.
///
/// 520×560 at the position it was last left at, when that is still on a
/// present display (SHL-39), else centred; at least 440×480 (SHL-11). It
/// can't be minimized (SHL-53) and stays resizable, since mismatch alerts
/// change its height.
pub fn open_about_window(
    reader: Arc<dyn PlatformAboutReader>,
    diagnostics: Option<CliRunner>,
    agent: Option<Entity<crate::daemon::DaemonClient>>,
    cx: &mut App,
) -> gpui::Result<WindowHandle<Root>> {
    let handle = cx.open_window(about_window_options(cx), |window, cx| {
        crate::ui::theme::follow_appearance(window, cx);
        let view = cx.new(|cx| {
            cx.observe_window_bounds(window, |_, window, cx| {
                crate::ui_state::remember_about_window(window, cx);
            })
            .detach();
            cx.observe_window_activation(window, |_, _, cx| {
                crate::menu::window_activation_changed(cx)
            })
            .detach();
            let mut view = AboutView::new(reader, diagnostics, cx);
            if let Some(agent) = agent {
                view.watch_agent(agent, cx);
            }
            view
        });
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    windows::track_regular_window(handle.window_id(), cx);
    crate::menu::window_activation_changed(cx);
    Ok(handle)
}

pub fn help_destination(window: WindowHandle<Root>, diagnostics: bool, cx: &mut App) {
    cx.defer(move |cx| {
        let _ = window.update(cx, |root, _, cx| {
            if let Ok(view) = root.view().clone().downcast::<AboutView>() {
                view.update(cx, |view, cx| {
                    view.help = Some(if diagnostics {
                        HelpDestination::Diagnostics
                    } else {
                        HelpDestination::Licences
                    });
                    cx.notify();
                });
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use gpui::px;
    use std::path::Path;

    use broscribe_platform::about::{
        FakeAboutReader, INVENTORY_FILE, InspectedInstallation, LICENCE_FILE, NOTICES_FILE,
        collect_about,
    };
    use broscribe_platform::update::BUILD_INFO_FILE;
    use gpui::TestAppContext;

    use super::*;
    use crate::ui::a11y_name;

    /// SHL-53, SHL-11, SHL-39: About can't be minimized, stays resizable,
    /// keeps at least 440×480 and reopens at its default size where it was
    /// left, when that is still on a display.
    #[gpui::test]
    fn about_is_not_minimizable_and_returns_to_where_it_was_left(cx: &mut TestAppContext) {
        use crate::ui_state::{FILE_NAME, UiState};

        cx.update(gpui::init);
        let data = tempfile::tempdir().unwrap();
        cx.update(|cx| crate::ui_state::install(data.path(), cx));
        let options = cx.update(|cx| about_window_options(cx));
        assert!(!options.is_minimizable);
        assert!(options.is_resizable);
        assert_eq!(
            options.window_min_size,
            Some(gpui::size(px(440.), px(480.)))
        );
        let reader = || -> Arc<dyn PlatformAboutReader> {
            Arc::new(FakeAboutReader::failing("synthetic failure"))
        };
        let about = cx
            .update(|cx| open_about_window(reader(), None, None, cx))
            .unwrap();
        let (minimizable, bounds) = about
            .update(cx, |_, window, _| {
                (window.is_minimizable(), window.bounds())
            })
            .unwrap();
        assert!(!minimizable);
        assert_eq!(
            bounds,
            Bounds::new(
                gpui::point(px(700.), px(260.)),
                gpui::size(px(520.), px(560.))
            )
        );
        // Taller after a mismatch alert, then closed: its position is kept.
        cx.simulate_window_resize(about.into(), gpui::size(px(520.), px(700.)));
        about
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        let saved = UiState::load(&data.path().join(FILE_NAME))
            .about_window
            .expect("About was remembered");
        assert_eq!((saved.frame.x, saved.frame.y), (700., 260.));

        // Moved to the corner on an earlier run: it opens there, at 520×560.
        let key = cx.update(|cx| crate::ui_state::displays(cx)[0].0.key.clone());
        std::fs::write(
            data.path().join(FILE_NAME),
            format!(
                r#"{{"about_window":{{"mode":"windowed","frame":{{"x":20,"y":30,"width":600,"height":900}},"display":"{key}"}}}}"#
            ),
        )
        .unwrap();
        cx.update(|cx| crate::ui_state::install(data.path(), cx));
        let about = cx
            .update(|cx| open_about_window(reader(), None, None, cx))
            .unwrap();
        assert_eq!(
            about.update(cx, |_, window, _| window.bounds()).unwrap(),
            Bounds::new(
                gpui::point(px(20.), px(30.)),
                gpui::size(px(520.), px(560.))
            )
        );
    }

    #[test]
    fn development_signatures_never_read_as_distribution_trust() {
        let development = SignatureFacts::apple(
            SignatureKind::AppleDevelopment {
                team: Some("TEAM123456".into()),
            },
            Some("org.example.synthetic".into()),
            SealState::Intact,
        );
        assert!(signature_line(&development).contains("not distribution"));
        assert!(signature_line(&development).contains("matches the app's files"));
        assert_eq!(
            notarization_line(development.notarization),
            "Not eligible: Apple notarizes only Developer ID builds"
        );

        let ad_hoc = SignatureFacts::apple(SignatureKind::AdHoc, None, SealState::Intact);
        assert!(notarization_line(ad_hoc.notarization).starts_with("Not eligible"));

        let developer_id = SignatureFacts::apple(
            SignatureKind::DeveloperId {
                team: Some("TEAM123456".into()),
            },
            Some("org.example.synthetic".into()),
            SealState::Intact,
        );
        assert!(
            notarization_line(developer_id.notarization)
                == "Unknown: the signature alone doesn't show notarization",
            "a Developer ID signature alone does not prove notarization"
        );

        let broken = SignatureFacts::apple(
            SignatureKind::DeveloperId { team: None },
            None,
            SealState::Broken { status: -67061 },
        );
        assert!(signature_line(&broken).contains("no longer matches"));

        let linux = SignatureFacts::without_notarization(
            SignatureKind::ManifestOnly,
            SealState::NotChecked,
        );
        assert_eq!(
            notarization_line(linux.notarization),
            "Not applicable on this system"
        );
    }

    /// An installation folder with build information for `version`, a
    /// licence and an inventory; no notice.
    fn facts(dir: &Path, version: &str) -> AboutFacts {
        std::fs::write(
            dir.join(BUILD_INFO_FILE),
            serde_json::json!({
                "schema": 1, "product": "BroScribe", "version": version,
                "build": 42, "target": "synthetic-target",
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.join(LICENCE_FILE), "synthetic licence").unwrap();
        std::fs::write(dir.join(NOTICES_FILE), "synthetic notices").unwrap();
        std::fs::write(
            dir.join(INVENTORY_FILE),
            serde_json::json!({
                "schema": 1, "target": "synthetic-target", "complete": false,
                "unresolved": ["synthetic-c 1.0.0: no text"],
                "packages": [
                    { "name": "synthetic-a", "version": "1.0.0", "license": "MIT" },
                    { "name": "synthetic-c", "version": "1.0.0", "license": null },
                ],
                "native": [],
            })
            .to_string(),
        )
        .unwrap();
        collect_about(
            APP_VERSION,
            InspectedInstallation {
                packaging: Packaging::InstallFolder,
                resources: Some(dir.to_path_buf()),
                location_notes: vec![LocationNote::Temporary],
                bundle: None,
                signature: SignatureFacts::without_notarization(
                    SignatureKind::ManifestOnly,
                    SealState::NotChecked,
                ),
                translated: None,
            },
        )
    }

    #[gpui::test]
    fn about_reads_the_facts_off_the_ui_loop(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        let dir = tempfile::tempdir().unwrap();
        let reader: Arc<dyn PlatformAboutReader> =
            Arc::new(FakeAboutReader::new(facts(dir.path(), APP_VERSION)));
        let window = cx.add_window(|_, cx| AboutView::new(reader, None, cx));
        window
            .update(cx, |about, _, _| assert!(about.facts().is_none()))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |about, _, _| {
                let facts = about.facts().unwrap().as_ref().unwrap();
                assert!(matches!(&facts.build, BuildInfoState::Present(info) if info.build == 42));
                assert!(
                    matches!(&facts.licences, LicenceInventoryState::Present(i) if !i.complete && i.unresolved == 1)
                );
                let documents: Vec<_> = facts.documents.iter().map(|d| d.kind).collect();
                assert!(documents.contains(&AboutDocumentKind::Licence));
                assert!(!documents.contains(&AboutDocumentKind::Notice));
                assert!(
                    packaging_line(facts.packaging, &facts.location_notes)
                        .contains("temporary folder")
                );
            })
            .unwrap();

        let failing: Arc<dyn PlatformAboutReader> =
            Arc::new(FakeAboutReader::failing("synthetic failure"));
        let window = cx.add_window(|_, cx| AboutView::new(failing, None, cx));
        cx.run_until_parked();
        window
            .update(cx, |about, _, _| {
                // The cause in words; the error's own text stays out.
                assert!(matches!(about.facts(), Some(Err(why))
                        if why == "BroScribe couldn't read or write a file."))
            })
            .unwrap();
    }

    #[gpui::test]
    fn help_targets_diagnostics_and_the_licences_fallback_without_copying(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut facts = facts(dir.path(), APP_VERSION);
        facts
            .documents
            .retain(|doc| doc.kind != AboutDocumentKind::ThirdPartyNotices);
        let window = cx.update(|cx| {
            gpui::init(cx);
            let runner =
                crate::test_support::runner(dir.path().join("no-cli"), dir.path().join("library"));
            open_about_window(
                Arc::new(FakeAboutReader::new(facts)),
                Some(runner),
                None,
                cx,
            )
            .unwrap()
        });
        cx.run_until_parked();
        for diagnostics in [true, false] {
            cx.update(|cx| help_destination(window, diagnostics, cx));
            for _ in 0..2 {
                cx.update_window(window.into(), |_, window, cx| {
                    window.draw(cx).clear(cx);
                    window.simulate_next_frame(cx);
                })
                .unwrap();
                cx.run_until_parked();
            }
            window
                .update(cx, |root, window, cx| {
                    let about = root.view().clone().downcast::<AboutView>().unwrap();
                    let about = about.read(cx);
                    let target = if diagnostics {
                        &about.diagnostics_focus
                    } else {
                        &about.licences_focus
                    };
                    assert!(target.is_focused(window));
                    assert_eq!(about.copy, DiagnosticsCopy::Idle);
                })
                .unwrap();
            assert!(cx.read_from_clipboard().is_none());
        }
    }

    /// Diagnostics run only on an explicit copy, through the injected runner
    /// for the app's library, and reach the clipboard; without a runner the
    /// action is unavailable and runs nothing.
    #[cfg(unix)]
    #[gpui::test]
    fn diagnostics_are_copied_only_when_asked(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;

        cx.update(gpui::init);
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let cli = dir.path().join("synthetic-cli");
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' '{{\"schema\":1,\"agent\":{{\"availability\":\"not_running\"}}}}'\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let library = dir.path().join("library");
        let reader: Arc<dyn PlatformAboutReader> =
            Arc::new(FakeAboutReader::new(facts(dir.path(), APP_VERSION)));

        let window = cx.add_window({
            let reader = Arc::clone(&reader);
            let runner = crate::test_support::runner(cli.clone(), library.clone());
            move |_, cx| AboutView::new(reader, Some(runner), cx)
        });
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert!(!log.exists(), "opening About ran diagnostics");
        assert!(cx.read_from_clipboard().is_none());

        window
            .update(cx, |about, _, cx| about.copy_diagnostics(cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |about, _, cx| {
                assert_eq!(about.copy, DiagnosticsCopy::Copied);
                let status = about.diagnostics_status(cx).expect("a copy status");
                let (id, role, label) = a11y_name(&status).expect("an accessible status");
                assert_eq!(
                    (id.as_str(), role),
                    ("about-diagnostics-status", Role::Status)
                );
                assert!(label.contains("copied"), "{label}");
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let args = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            args.trim(),
            format!("--library {} --json diagnostics", library.display())
        );
        let copied: serde_json::Value = serde_json::from_str(
            &cx.read_from_clipboard()
                .and_then(|c| c.text())
                .expect("copied text"),
        )
        .unwrap();
        assert_eq!(copied["agent"]["availability"], "not_running");

        // No runner (a launch without a library): nothing runs.
        let without = cx.add_window(move |_, cx| AboutView::new(reader, None, cx));
        cx.run_until_parked();
        without
            .update(cx, |about, _, cx| {
                about.copy_diagnostics(cx);
                assert_eq!(about.copy, DiagnosticsCopy::Idle);
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(without.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    }

    /// SHL-03: About draws its title bar only where the window system
    /// leaves decorations to the app, with named window controls, and the
    /// bar takes no focus.
    #[gpui::test]
    fn about_draws_its_title_bar_only_under_client_side_decorations(cx: &mut TestAppContext) {
        use crate::ui::chrome::testing::{chrome, report_client_decorations, title_heading};
        use crate::ui::chrome::{CLOSE_LABEL, MAXIMIZE_LABEL, MINIMIZE_LABEL};

        cx.update(gpui::init);
        let dir = tempfile::tempdir().unwrap();
        let reader: Arc<dyn PlatformAboutReader> =
            Arc::new(FakeAboutReader::new(facts(dir.path(), APP_VERSION)));
        let open = |cx: &mut TestAppContext| {
            let reader = Arc::clone(&reader);
            cx.add_window(move |_, cx| AboutView::new(reader, None, cx))
        };

        let window = open(cx);
        let server = chrome(window.into(), cx);
        server.assert_none();
        assert_eq!(title_heading(window, cx), None);

        report_client_decorations(cx);
        let window = open(cx);
        let client = chrome(window.into(), cx);
        client.assert_first();
        assert_eq!(
            client.control_labels(),
            [MINIMIZE_LABEL, MAXIMIZE_LABEL, CLOSE_LABEL]
        );
        assert_eq!(title_heading(window, cx).as_deref(), Some(WINDOW_TITLE));
        assert_eq!(client.focused, server.focused, "the title bar took focus");
    }

    /// Every line About shows is an identified accessible node whose label
    /// carries its actual text: loading, each fact with its field and
    /// value, and the incomplete-licence and failure alerts.
    #[gpui::test]
    fn about_lines_reach_assistive_technology_with_their_values(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(broscribe_platform::about::NOTICE_FILE),
            "synthetic notice",
        )
        .unwrap();
        let mut loaded = facts(dir.path(), "0.0.1");
        loaded.architecture.translated = Some(true);
        loaded.bundle = Some(broscribe_platform::about::BundleMetadata {
            identifier: None,
            short_version: Some("0.0.2".into()),
            bundle_version: Some("43".into()),
        });
        loaded.location_notes.extend([
            LocationNote::ReadOnlyVolume,
            LocationNote::NetworkVolume,
            LocationNote::Translocated,
        ]);
        if let LicenceInventoryState::Present(inventory) = &mut loaded.licences {
            inventory.target = "synthetic-other-target".into();
        }
        loaded.signature = SignatureFacts::apple(
            SignatureKind::AppleDevelopment {
                team: Some("TEAM123456".into()),
            },
            Some("org.example.synthetic".into()),
            SealState::Broken { status: -1 },
        );
        let reader: Arc<dyn PlatformAboutReader> = Arc::new(FakeAboutReader::new(loaded));
        let window = cx.add_window(|_, cx| AboutView::new(reader, None, cx));
        cx.update_window(window.into(), |_, window, cx| {
            crate::ui::a11y_tree::draw_and_read(window, cx).assert_title("about-name");
        })
        .unwrap();
        let nodes = |window: WindowHandle<AboutView>, cx: &mut TestAppContext| {
            window
                .update(cx, |about, _, cx| {
                    about
                        .lines(cx)
                        .iter()
                        .map(|line| a11y_name(line).expect("every About line is a node"))
                        .collect::<Vec<_>>()
                })
                .unwrap()
        };
        let loading = nodes(window, cx);
        assert!(
            loading.iter().any(|(id, role, label)| id == "about-loading"
                && *role == Role::Status
                && !label.is_empty()),
            "{loading:?}"
        );

        cx.run_until_parked();
        let shown = nodes(window, cx);
        let ids: std::collections::BTreeSet<&str> =
            shown.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(ids.len(), shown.len(), "IDs are unique: {shown:?}");
        assert!(shown.iter().all(|(_, _, label)| !label.trim().is_empty()));
        let find = |id: &str| {
            shown
                .iter()
                .find(|(i, _, _)| i == id)
                .unwrap_or_else(|| panic!("no {id} in {shown:?}"))
        };
        let (_, role, build) = find("about-build");
        assert_eq!(*role, Role::Label);
        assert!(
            build.contains("Build") && build.contains("42") && !build.contains("synthetic-target")
        );
        let (_, _, signing) = find("about-signing");
        assert!(signing.contains("Signing") && signing.contains("not distribution"));
        let (_, role, incomplete) = find("about-licences-incomplete");
        assert_eq!(*role, Role::Alert);
        assert!(incomplete.contains('1'), "{incomplete}");

        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let summary = [
            ("about-name", "BroScribe"),
            ("about-license", "Licensed under the Apache License 2.0"),
            ("about-build-heading", "Build and signing"),
            ("about-translation", "Translation: Running translated"),
            ("about-licences-heading", "Third-party licences"),
            ("about-licence-toggle", "Licence counts"),
            ("about-diagnostics-heading", "Diagnostics"),
            (
                "about-diagnostics-note",
                "Diagnostics are unavailable: BroScribe did not start with a library.",
            ),
            ("about-copy-diagnostics", "Copy diagnostics"),
            ("about-open-licence", "Open licence"),
            ("about-open-notice", "Open notice"),
            ("about-open-notices", "Open third-party notices"),
        ];
        let toggle = visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            for (id, copy) in summary {
                assert_eq!(tree.read(tree.find(id)).1, copy, "{id}");
            }
            assert_eq!(
                tree.read(tree.find("about-version")).1,
                format!("Version {APP_VERSION}")
            );
            let architecture = tree.read(tree.find("about-architecture")).1;
            assert!(
                architecture.contains("Apple silicon")
                    || architecture.contains("arm64")
                    || architecture.contains("x86")
                    || architecture.contains("Unknown")
            );
            let installation = tree.read(tree.find("about-installation")).1;
            for note in [
                "Install folder",
                "temporary folder",
                "read-only volume",
                "network volume",
                "temporary location",
            ] {
                assert!(installation.contains(note), "{installation}");
            }
            assert!(
                tree.read(tree.find("about-signing"))
                    .1
                    .contains("no longer matches")
            );
            assert!(
                tree.read(tree.find("about-notarization"))
                    .1
                    .contains("Not eligible")
            );
            for index in 0..5 {
                assert_eq!(
                    tree.read(tree.find(&format!("about-mismatch-{index}"))).0,
                    "Alert"
                );
            }
            assert!(
                tree.read(tree.find("about-licences"))
                    .1
                    .contains("2 components")
            );
            assert!(tree.all("about-licence-0").is_empty());
            assert!(
                tree.nodes
                    .iter()
                    .all(|node| !node.name.contains(dir.path().to_str().unwrap()))
            );
            gpui::base::test_support::snapshots(window)
                .iter()
                .find(|node| node.label() == Some("Licence counts"))
                .expect("licence disclosure button")
                .bounds()
                .center()
        });
        visual.simulate_click(toggle, Default::default());
        window
            .update(cx, |about, window, cx| {
                let (_, spoken) =
                    crate::ui::spoken_log::while_building(|| about.render(window, cx));
                assert!(spoken.iter().any(|(_, _, name)| name == "MIT: 1"));
                assert!(
                    spoken
                        .iter()
                        .any(|(_, _, name)| name == "Translation: Running translated")
                );
            })
            .unwrap();

        let mut missing = facts(dir.path(), APP_VERSION);
        missing.build = BuildInfoState::Missing;
        missing.licences = LicenceInventoryState::Missing;
        let missing = cx
            .add_window(|_, cx| AboutView::new(Arc::new(FakeAboutReader::new(missing)), None, cx));
        cx.run_until_parked();
        cx.update_window(missing.into(), |_, window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("about-build")).1,
                "Build: No build information"
            );
            assert_eq!(
                tree.read(tree.find("about-translation")).1,
                "Translation: Unknown"
            );
            assert_eq!(tree.read(tree.find("about-licences-missing")).0, "Alert");
            assert_eq!(
                tree.read(tree.find("about-diagnostics-heading")).1,
                "Diagnostics"
            );
        })
        .unwrap();

        let failing: Arc<dyn PlatformAboutReader> =
            Arc::new(FakeAboutReader::failing("synthetic failure"));
        let window = cx.add_window(|_, cx| AboutView::new(failing, None, cx));
        cx.run_until_parked();
        let shown = nodes(window, cx);
        let (_, role, unavailable) = shown
            .iter()
            .find(|(id, _, _)| id == "about-unavailable")
            .expect("the failure is a node");
        assert_eq!(*role, Role::Alert);
        assert_eq!(
            unavailable,
            "Couldn't read the build, signing and licence information. \
             BroScribe couldn't read or write a file."
        );
        assert!(!unavailable.contains("synthetic"), "{unavailable}");
    }

    /// A diagnostics run that fails is an Alert carrying its reason.
    #[gpui::test]
    fn a_failed_diagnostics_copy_is_an_alert_with_its_reason(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        let dir = tempfile::tempdir().unwrap();
        let reader: Arc<dyn PlatformAboutReader> =
            Arc::new(FakeAboutReader::new(facts(dir.path(), APP_VERSION)));
        let runner =
            crate::test_support::runner(dir.path().join("no-such-cli"), dir.path().join("library"));
        let window = cx.add_window(move |_, cx| AboutView::new(reader, Some(runner), cx));
        cx.run_until_parked();
        window
            .update(cx, |about, _, cx| about.copy_diagnostics(cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |about, _, cx| {
                let DiagnosticsCopy::Failed(why) = about.copy.clone() else {
                    panic!("{:?}", about.copy);
                };
                let status = about.diagnostics_status(cx).expect("a copy status");
                let (id, role, label) = a11y_name(&status).expect("an accessible alert");
                assert_eq!(
                    (id.as_str(), role),
                    ("about-diagnostics-status", Role::Alert)
                );
                assert!(label.contains(&why), "{label}");
            })
            .unwrap();
        assert!(cx.read_from_clipboard().is_none());
    }

    #[test]
    fn a_build_information_version_other_than_the_app_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let facts = facts(dir.path(), "0.0.0-synthetic");
        let lines: Vec<String> = facts.mismatches().iter().map(mismatch_line).collect();
        assert!(
            lines
                .iter()
                .any(|line| line.contains(APP_VERSION) && line.contains("0.0.0-synthetic")),
            "{lines:?}"
        );
    }
}
