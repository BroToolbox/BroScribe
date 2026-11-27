//! AI providers: the one place a provider is enabled (only after the human
//! accepts the registry's own consent text), disabled (which withdraws that
//! consent), tested, or given an API key. Settings shows every purpose; the
//! setup assistant's Summaries step shows the LLM providers only
//! ([`ProviderScope`]).
//!
//! Every action is a `broscribe providers …` CLI child. An API key goes to
//! `providers set-key` on stdin only (never argv or logs) from a masked draft
//! bound to its provider; opening another form, saving, removing, cancelling
//! and [`ProvidersView::reset_drafts`] drop the draft. When a command ends,
//! [`AppState`] re-reads `config.toml` whether or not this view still exists,
//! so a command finishing after its view closed never leaves stale policy in
//! memory. Commands are tracked per provider and operation
//! ([`ProviderOp::may_start`]): one provider's command never disables
//! another provider's controls. Each control of a row (consent, API key,
//! test) shows its wait and last outcome on its own line
//! ([`ProviderControl`]), so one control's command never hides another's.
//!
//! Mounting: `cx.new(|_| ProvidersView::new(state, scope))`, then
//! [`ProvidersView::reload`] whenever it is shown.

use std::collections::BTreeMap;
use std::ffi::OsString;

use broscribe_core::config::Config;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::{Input, InputEvent, InputState};
use gpui::component::{Disableable as _, Sizable as _, WindowExt as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, px,
};
use serde_json::Value;

use crate::jobs::{ChildKind, failure_text};
use crate::state::AppState;
use crate::ui::kit::{self, StatusKind};
use crate::ui::text_line;
use crate::views::settings::{args, muted, outcome, reload_config};

/// Which providers a [`ProvidersView`] lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderScope {
    /// Every provider, plus configured ones the registry no longer lists.
    All,
    /// Summary (LLM) providers; text-embedding providers are chosen with
    /// the models.
    Llm,
}

impl ProviderScope {
    fn includes(self, row: &ProviderRow) -> bool {
        match self {
            Self::All => true,
            Self::Llm => row.purpose.as_deref() == Some("llm"),
        }
    }
}

/// One provider from `broscribe providers list --json` (fields read
/// leniently).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProviderLocation {
    Local,
    Cloud,
    Cli,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRow {
    pub id: String,
    /// The registry's display name (the id when it has none).
    pub name: String,
    /// `llm` or `text_embedding`; `None` when unknown.
    pub purpose: Option<String>,
    pub enabled: bool,
    pub consented: bool,
    /// Detection of an agent CLI (`ready`, `not_installed`, …), if any.
    pub status: Option<String>,
    /// The registry's consent text; enabling is refused without it.
    pub consent_text: Option<String>,
    /// An OpenAI-compatible HTTP provider, which needs an API key.
    pub http: bool,
    pub location: ProviderLocation,
    /// Whether the secret store holds its API key (`None`: unknown).
    pub has_key: Option<bool>,
}

impl ProviderRow {
    fn not_installed(&self) -> bool {
        !self.http && matches!(self.status.as_deref(), Some("not_installed" | "not_found"))
    }
}

/// The masked API-key draft of one HTTP provider. Opening a form (for any
/// provider), saving, removing or cancelling drops it, so a draft typed for
/// one provider can never be stored for another and no undo history keeps it.
struct KeyForm {
    provider: String,
    input: Entity<InputState>,
}

struct HttpForm {
    provider: String,
    address: Entity<InputState>,
    model: Entity<InputState>,
    key: Entity<InputState>,
    invalid_address: bool,
    _validation: gpui::Subscription,
}

pub(crate) fn http_configured(config: Option<&broscribe_core::config::ProviderConfig>) -> bool {
    config.is_some_and(|provider| {
        provider.base_url.as_deref().is_some_and(|address| {
            broscribe_core::provider_address::validate_provider_address(address).is_ok()
        }) && provider
            .model
            .as_deref()
            .is_some_and(|model| broscribe_core::config::validate_model_name(model).is_ok())
    })
}

fn summary_rows<'a>(rows: &'a [ProviderRow], config: &Config) -> Vec<&'a ProviderRow> {
    let mut rows: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.purpose.as_deref() == Some("llm")
                && config.provider(&row.id).is_some_and(|p| p.is_consented())
        })
        .collect();
    rows.sort_by(|a, b| {
        let priority = |row: &ProviderRow| {
            config
                .profile
                .llm
                .iter()
                .position(|id| id == &row.id)
                .unwrap_or(usize::MAX)
        };
        (priority(a), &a.id).cmp(&(priority(b), &b.id))
    });
    rows
}

fn key_store_note() -> String {
    format!(
        "Kept in your {} and sent only to this provider.",
        broscribe_platform_native::NativePlatform::host_platform().secret_store_name()
    )
}

/// One `providers <verb> <id>` command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderOp {
    Enable,
    Disable,
    Test,
    SetKey,
    DeleteKey,
}

/// A control of a provider row. Each shows its wait and last outcome on a
/// line of its own, so a newer outcome of one control is never replaced by
/// another control's wait or older outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProviderControl {
    /// Enable (after consent) and Disable: `provider-consent-result-<id>`.
    Consent,
    /// The API key form's Save key and Remove key:
    /// `provider-key-result-<id>`, beside the form.
    Key,
    /// Test: `provider-result-<id>`.
    Test,
}

impl ProviderControl {
    /// The ID of this control's line on provider `id`'s row.
    fn line_id(self, id: &str) -> SharedString {
        match self {
            Self::Consent => format!("provider-consent-result-{id}"),
            Self::Key => format!("provider-key-result-{id}"),
            Self::Test => format!("provider-result-{id}"),
        }
        .into()
    }
}

impl ProviderOp {
    /// The control whose line shows this command's wait and outcome.
    pub fn control(self) -> ProviderControl {
        match self {
            Self::Enable | Self::Disable => ProviderControl::Consent,
            Self::SetKey | Self::DeleteKey => ProviderControl::Key,
            Self::Test => ProviderControl::Test,
        }
    }

    /// The Pending words on its control's line while it runs for the
    /// provider shown as `name`.
    fn waiting(self, name: &str) -> String {
        match self {
            Self::Enable => format!("Enabling {name}…"),
            Self::Disable => format!("Disabling {name}…"),
            Self::Test => format!("Testing {name}…"),
            Self::SetKey => "Storing the API key…".into(),
            Self::DeleteKey => "Removing the API key…".into(),
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Test => "test",
            Self::SetKey => "set-key",
            Self::DeleteKey => "delete-key",
        }
    }

    /// The first sentence of its failure line.
    fn failed(self) -> &'static str {
        match self {
            Self::Enable => "Couldn't turn on this provider.",
            Self::Disable => "Couldn't turn off this provider.",
            Self::Test => "Couldn't test this provider.",
            Self::SetKey => "Couldn't save the API key.",
            Self::DeleteKey => "Couldn't delete the API key.",
        }
    }

    /// It changes the provider's config or stored key.
    fn changes(self) -> bool {
        self != Self::Test
    }

    /// Whether it may start while `running` run for the same provider: a
    /// change waits for the provider's other changes; a test waits for
    /// every command of that provider (it uses the settled config and runs
    /// once at a time). Other providers never matter.
    pub fn may_start(self, running: &[ProviderOp]) -> bool {
        if self.changes() {
            !running.iter().any(|op| op.changes())
        } else {
            running.is_empty()
        }
    }
}

pub struct ProvidersView {
    state: Entity<AppState>,
    scope: ProviderScope,
    loaded: bool,
    show_missing: bool,
    hide_unavailable: bool,
    http_form: Option<HttpForm>,
    enable_focus: BTreeMap<String, FocusHandle>,
    setup_focus: BTreeMap<String, FocusHandle>,
    remove_key_focus: BTreeMap<String, FocusHandle>,
    configuring: std::collections::BTreeSet<String>,
    rows: Vec<ProviderRow>,
    error: Option<String>,
    /// Bumped by every list; an older list's late answer is dropped.
    generation: u64,
    /// The provider whose consent text is shown, awaiting an explicit answer.
    consent_for: Option<String>,
    /// The open API-key form; at most one, bound to its provider.
    key_form: Option<KeyForm>,
    /// The commands running, by provider.
    running: BTreeMap<String, Vec<ProviderOp>>,
    /// The last outcome of each control, by provider.
    results: BTreeMap<(String, ProviderControl), std::result::Result<String, String>>,
}

impl ProvidersView {
    /// An empty view; [`Self::reload`] lists the providers.
    pub fn new(state: Entity<AppState>, scope: ProviderScope) -> Self {
        Self {
            state,
            scope,
            loaded: false,
            show_missing: false,
            hide_unavailable: false,
            http_form: None,
            enable_focus: BTreeMap::new(),
            setup_focus: BTreeMap::new(),
            remove_key_focus: BTreeMap::new(),
            configuring: Default::default(),
            rows: Vec::new(),
            error: None,
            generation: 0,
            consent_for: None,
            key_form: None,
            running: BTreeMap::new(),
            results: BTreeMap::new(),
        }
    }

    pub(crate) fn in_setup(mut self) -> Self {
        self.hide_unavailable = true;
        self
    }

    #[cfg(test)]
    pub(crate) fn scope(&self) -> ProviderScope {
        self.scope
    }

    /// The listed providers in this view's scope.
    pub fn rows(&self) -> impl Iterator<Item = &ProviderRow> {
        let scope = self.scope;
        self.rows.iter().filter(move |r| scope.includes(r))
    }

    /// The provider whose consent text is open, if any.
    pub fn pending_consent(&self) -> Option<&str> {
        self.consent_for.as_deref()
    }

    /// The provider whose API-key form is open, if any.
    pub fn key_form_provider(&self) -> Option<&str> {
        self.key_form.as_ref().map(|form| form.provider.as_str())
    }

    /// The commands running for `id`.
    pub fn running(&self, id: &str) -> &[ProviderOp] {
        self.running.get(id).map_or(&[], Vec::as_slice)
    }

    /// The last outcome of `control` for `id`.
    pub fn result(
        &self,
        id: &str,
        control: ProviderControl,
    ) -> Option<&std::result::Result<String, String>> {
        self.results.get(&(id.to_owned(), control))
    }

    /// Records the outcome of `control` for `id`, replacing only that
    /// control's last outcome.
    fn tell(
        &mut self,
        id: String,
        control: ProviderControl,
        outcome: std::result::Result<String, String>,
    ) {
        self.results.insert((id, control), outcome);
    }

    pub(crate) fn summary_rows(&self, config: &Config) -> Vec<&ProviderRow> {
        summary_rows(&self.rows, config)
    }

    pub(crate) fn prefer(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.configuring.insert(id.clone()) {
            return;
        }
        self.results.remove(&(id.clone(), ProviderControl::Consent));
        cx.notify();
        let state = self.state.clone();
        let task =
            state
                .read(cx)
                .run_cli(args(["providers", "prefer", &id]), ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            cx.update(|cx| reload_config(&state, cx)).await;
            let _ = this.update(cx, |this, cx| {
                this.configuring.remove(&id);
                this.tell(
                    id,
                    ProviderControl::Consent,
                    result
                        .map(|_| "First for automatic summaries.".into())
                        .map_err(|e| {
                            failure_text(
                                "Couldn’t choose a provider for automatic summaries.",
                                &e.user_text(),
                                "",
                            )
                        }),
                );
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn is_configuring(&self, id: &str) -> bool {
        self.configuring.contains(id)
    }

    fn show_terms(
        &mut self,
        provider: &ProviderRow,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reset_drafts(cx);
        let id = provider.id.clone();
        self.request_consent(id.clone(), cx);
        let mut spec = kit::ConfirmSpec::new(
            kit::confirm_line(
                "provider-terms-title",
                format!("Turn on {}?", provider.name),
            ),
            "provider-terms",
            "provider-terms-actions",
            "provider-terms-cancel",
            kit::confirm_line("provider-terms-accept", "Agree and turn on"),
        );
        spec.primary_action = true;
        spec.irreversible = false;
        if let Some(terms) = &provider.consent_text {
            spec.caveat = Some(kit::confirm_line("provider-terms-text", terms.clone()));
        } else {
            spec.status = Some((
                kit::confirm_line("provider-terms-missing", NO_CONSENT_TEXT),
                StatusKind::Failed,
            ));
            spec.action_visible = false;
        }
        spec.goes.push(kit::confirm_line(
            "provider-terms-sent",
            if provider.purpose.as_deref() == Some("text_embedding") {
                "Transcript text used to build the search index."
            } else {
                "Transcript text from meetings you summarise or run templates on."
            },
        ));
        spec.stays.push(kit::confirm_line(
            "provider-terms-kept",
            "Audio and voice data stay on this computer.",
        ));
        let cancel = cx.weak_entity();
        let accept = cancel.clone();
        kit::confirm_dialog(
            opener,
            window,
            cx,
            move |_| Some(spec.clone()),
            move |_, cx| {
                let _ = cancel.update(cx, |this, cx| this.decline_consent(cx));
                true
            },
            move |_, cx| {
                let _ = accept.update(cx, |this, cx| this.accept_consent(id.clone(), cx));
                true
            },
        );
    }

    fn ask_to_remove_key(
        &self,
        provider: &ProviderRow,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = provider.id.clone();
        let name = provider.name.clone();
        let success = self.setup_focus.get(&id).cloned();
        let view = cx.weak_entity();
        kit::confirm_dialog(
            opener,
            window,
            cx,
            move |_| {
                let mut spec = kit::ConfirmSpec::new(
                    kit::confirm_line(
                        "provider-remove-key-title",
                        format!("Remove the API key for {name}?"),
                    ),
                    "provider-remove-key",
                    "provider-remove-key-actions",
                    "provider-remove-key-cancel",
                    kit::confirm_line("provider-remove-key-confirm", "Remove key"),
                );
                spec.success_focus = success.clone();
                spec.caveat = Some(kit::confirm_line(
                    "provider-remove-key-consequence",
                    "You’ll need to paste it again to use this provider.",
                ));
                Some(spec)
            },
            |_, _| true,
            move |_, cx| {
                let _ = view.update(cx, |this, cx| this.remove_api_key(id.clone(), cx));
                true
            },
        );
    }

    fn open_http_form(
        &mut self,
        provider: &ProviderRow,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reset_drafts(cx);
        let config = self
            .state
            .read(cx)
            .config()
            .provider(&provider.id)
            .cloned()
            .unwrap_or_default();
        let address = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("http://localhost:11434/v1")
                .default_value(
                    config
                        .base_url
                        .filter(|address| {
                            broscribe_core::provider_address::validate_provider_address(address)
                                .is_ok()
                        })
                        .unwrap_or_default(),
                )
        });
        let model = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Model name, as the server lists it")
                .default_value(config.model.unwrap_or_default())
        });
        let key = masked_key_input(window, cx);
        let validation = cx.subscribe_in(&address, window, |this, input, event, _, cx| {
            if matches!(event, InputEvent::Blur)
                && let Some(form) = this.http_form.as_mut()
                && form.address == *input
            {
                form.invalid_address = broscribe_core::provider_address::validate_provider_address(
                    input.read(cx).value().as_ref(),
                )
                .is_err();
                cx.refresh_windows();
            }
        });
        self.http_form = Some(HttpForm {
            provider: provider.id.clone(),
            address: address.clone(),
            model,
            key,
            invalid_address: false,
            _validation: validation,
        });
        if window.root::<gpui::component::Root>().flatten().is_none() {
            return;
        }
        opener.focus(window, cx);
        let view = cx.entity();
        let title = format!("Set up {}", provider.name);
        let initial = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            // BroScribe's dialogs stand on the popover ground, not the desk.
            let dialog = dialog.bg(gpui::component::ActiveTheme::theme(cx).popover);
            if initial.replace(false) {
                address.focus_handle(cx).focus(window, cx);
            }
            let this = view.read(cx);
            let Some(form) = &this.http_form else {
                return dialog;
            };
            let key_label = match this
                .rows
                .iter()
                .find(|row| row.id == form.provider)
                .and_then(|row| row.has_key)
            {
                Some(true) => "Change API key (optional)",
                _ => "API key (optional)",
            };
            let cancel = view.downgrade();
            let save = cancel.clone();
            let finish = cancel.clone();
            dialog
                .title(title.clone())
                .width(px(480.))
                .entrance(gpui::component::dialog::DialogEntrance::None)
                .overlay_closable(false)
                .close_button(false)
                .on_ok(|_, _, _| false)
                .on_cancel(move |_, _, cx| {
                    let _ = cancel.update(cx, |this, cx| this.reset_drafts(cx));
                    true
                })
                .on_close({
                    let opener = opener.clone();
                    move |_, window, cx| opener.focus(window, cx)
                })
                .child(
                    kit::stack()
                        .w_full()
                        .child(kit::field(
                            "provider-address-title",
                            "Server address",
                            None,
                            |label| Input::new(&form.address).aria_label(label),
                            cx,
                        ))
                        .when(form.invalid_address, |form| {
                            form.child(kit::status_line(
                                "provider-address-error",
                                StatusKind::Failed,
                                "Use an https address, or http for this computer only.",
                                cx,
                            ))
                        })
                        .child(kit::field(
                            "provider-model-title",
                            "Model",
                            None,
                            |label| Input::new(&form.model).aria_label(label),
                            cx,
                        ))
                        .child(kit::field(
                            "provider-key-title",
                            key_label,
                            Some(("provider-secret-store".into(), key_store_note().into())),
                            |label| Input::new(&form.key).aria_label(label),
                            cx,
                        ))
                        .children(this.control_line(
                            &form.provider,
                            &title,
                            ProviderControl::Key,
                            cx,
                        )),
                )
                .footer(kit::confirm_actions(
                    Button::new("provider-setup-cancel")
                        .label("Cancel")
                        .outline()
                        .on_click(move |_, window, cx| {
                            let _ = finish.update(cx, |this, cx| this.reset_drafts(cx));
                            window.close_dialog(cx);
                        }),
                    Button::new("provider-setup-save")
                        .label("Save")
                        .map(|button| kit::primary(button, false, cx))
                        .on_click(move |_, window, cx| {
                            let _ = save.update(cx, |this, cx| this.save_http_form(window, cx));
                        }),
                    cx,
                ))
        });
    }

    fn save_http_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.http_form else {
            return;
        };
        let id = form.provider.clone();
        let address = form.address.read(cx).value().trim().to_owned();
        let model = form.model.read(cx).value().trim().to_owned();
        if broscribe_core::provider_address::validate_provider_address(&address).is_err() {
            if let Some(form) = &mut self.http_form {
                form.invalid_address = true;
            }
            cx.refresh_windows();
            return;
        }
        if broscribe_core::config::validate_model_name(&model).is_err() {
            self.tell(
                id,
                ProviderControl::Key,
                Err("Enter a valid model name as the server lists it.".into()),
            );
            cx.refresh_windows();
            return;
        }
        let key = form.key.read(cx).value().to_string();
        let key = if key.trim().is_empty() {
            None
        } else {
            match set_key_command(&id, &key) {
                Ok(key) => Some(key),
                Err(cause) => {
                    self.tell(id, ProviderControl::Key, Err(cause));
                    cx.refresh_windows();
                    return;
                }
            }
        };
        if !self.configuring.insert(id.clone()) {
            return;
        }
        self.reset_drafts(cx);
        window.close_dialog(cx);
        self.results.remove(&(id.clone(), ProviderControl::Key));
        let state = self.state.clone();
        let cli = state.read(cx).cli().clone();
        let command = args([
            "providers",
            "configure",
            &id,
            "--base-url",
            &address,
            "--model",
            &model,
        ]);
        let task = state.read(cx).run_cli(command, ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let mut result = task.await;
            if let Some(key) = key
                && result.is_ok()
            {
                result = cli
                    .run_with_stdin(
                        provider_args("set-key", &id),
                        ChildKind::Command,
                        key,
                        cx.background_executor(),
                    )
                    .await;
            }
            cx.update(|cx| reload_config(&state, cx)).await;
            let _ = this.update(cx, |this, cx| {
                this.configuring.remove(&id);
                this.tell(
                    id,
                    ProviderControl::Key,
                    result
                        .map(|_| "Server settings saved.".into())
                        .map_err(|e| {
                            failure_text("Couldn’t save the provider setup.", &e.user_text(), "")
                        }),
                );
                this.reload(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Lists the providers again (detection, consent and key state).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        if self.error.take().is_some() {
            self.loaded = false;
            cx.notify();
        }
        let task = self
            .state
            .read(cx)
            .run_cli(args(["providers", "list"]), ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loaded = true;
                match result {
                    Ok(v) => {
                        this.rows = parse_providers(&v);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.user_text()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Drops the API-key draft and the open consent question (leaving the
    /// page or closing the assistant); nothing typed or pending survives it.
    pub fn reset_drafts(&mut self, cx: &mut Context<Self>) {
        self.http_form = None;
        self.key_form = None;
        self.consent_for = None;
        cx.notify();
    }

    /// Shows the registry's consent text for `id` (or why it cannot be
    /// enabled); nothing is enabled until [`Self::accept_consent`].
    pub fn request_consent(&mut self, id: String, cx: &mut Context<Self>) {
        // A new action on the consent control: its last outcome goes, so
        // the dialog never sits above an older line with its own words.
        self.results.remove(&(id.clone(), ProviderControl::Consent));
        self.consent_for = Some(id);
        cx.notify();
    }

    pub fn decline_consent(&mut self, cx: &mut Context<Self>) {
        self.consent_for = None;
        cx.notify();
    }

    /// The human's "I agree" on the consent text open for `id`: runs
    /// `providers enable <id> --accept`. Refused unless that text is open
    /// and came from the registry.
    pub fn accept_consent(&mut self, id: String, cx: &mut Context<Self>) {
        if self.consent_for.as_deref() != Some(id.as_str()) {
            return;
        }
        self.consent_for = None;
        if self.registry_consent(&id).is_none() {
            self.tell(id, ProviderControl::Consent, Err(NO_CONSENT_TEXT.into()));
            cx.notify();
            return;
        }
        self.run(ProviderOp::Enable, id, None, cx);
    }

    /// `providers disable`, which also withdraws consent (`consented_at`).
    pub fn disable(&mut self, id: String, cx: &mut Context<Self>) {
        self.run(ProviderOp::Disable, id, None, cx);
    }

    pub fn test(&mut self, id: String, cx: &mut Context<Self>) {
        self.run(ProviderOp::Test, id, None, cx);
    }

    /// The registry consent text shown before enabling `id`, if the list has it.
    fn registry_consent(&self, id: &str) -> Option<&str> {
        self.rows
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.consent_text.as_deref())
    }

    /// Runs `providers <verb> <id>`; `stdin` carries the API key for
    /// `set-key`. Refused while a conflicting command of the same provider
    /// runs ([`ProviderOp::may_start`]); other providers stay usable.
    fn run(&mut self, op: ProviderOp, id: String, stdin: Option<Vec<u8>>, cx: &mut Context<Self>) {
        if !op.may_start(self.running(&id)) {
            return;
        }
        self.consent_for = None;
        // The control's earlier outcome goes: its line shows this command's
        // wait, then its outcome. Other controls keep theirs.
        self.results.remove(&(id.clone(), op.control()));
        self.running.entry(id.clone()).or_default().push(op);
        cx.notify();
        let a = provider_args(op.verb(), &id);
        let task = match stdin {
            Some(input) => self.state.read(cx).cli().run_with_stdin(
                a,
                ChildKind::Command,
                input,
                cx.background_executor(),
            ),
            None => self.state.read(cx).run_cli(a, ChildKind::Command, cx),
        };
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            // The child may have changed `config.toml`: the app takes it in
            // even when this view (or the assistant showing it) is gone.
            cx.update(|cx| reload_config(&state, cx)).await;
            this.update(cx, |this, cx| {
                if let Some(ops) = this.running.get_mut(&id) {
                    if let Some(ix) = ops.iter().position(|o| *o == op) {
                        ops.remove(ix);
                    }
                    if ops.is_empty() {
                        this.running.remove(&id);
                    }
                }
                let name = this
                    .rows
                    .iter()
                    .find(|r| r.id == id)
                    .map_or_else(|| id.clone(), |r| r.name.clone());
                let tested = Tested {
                    id: &id,
                    name: &name,
                };
                let text = match result {
                    Ok(v) => provider_summary(op.verb(), &tested, &v),
                    // A test the CLI refused or that the provider failed:
                    // the CLI exits with an error, never `ok: false`.
                    Err(e) if op == ProviderOp::Test => {
                        Err(tested.failure(&test_failure_cause(&e, &id)))
                    }
                    Err(e) => Err(failure_text(op.failed(), &e.user_text(), "")),
                };
                if op == ProviderOp::Disable && text.is_ok() {
                    // The Test button goes with the provider: so does its
                    // last outcome.
                    this.results.remove(&(id.clone(), ProviderControl::Test));
                }
                this.tell(id, op.control(), text);
                this.reload(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens a fresh, empty API-key form for `id`, discarding any other draft.
    /// Reopening the same provider's form keeps its draft.
    pub fn open_key_form(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.key_form_provider() == Some(id.as_str()) {
            return;
        }
        // A new action on the key control: its last outcome goes, as the
        // consent control's does when its dialog opens, so the form never
        // sits beside an older line.
        self.results.remove(&(id.clone(), ProviderControl::Key));
        self.key_form = Some(KeyForm {
            provider: id,
            input: masked_key_input(window, cx),
        });
        cx.notify();
    }

    /// Closes the API-key form, dropping its draft.
    pub fn close_key_form(&mut self, cx: &mut Context<Self>) {
        self.key_form = None;
        cx.notify();
    }

    /// Stores the draft typed for `id` with `providers set-key` (key on stdin
    /// only), then drops the form. A draft typed for another provider is
    /// never sent; while another change of `id` runs the draft is kept and
    /// nothing is sent.
    pub fn save_api_key(&mut self, id: String, cx: &mut Context<Self>) {
        if !ProviderOp::SetKey.may_start(self.running(&id)) {
            return;
        }
        let key = match &self.key_form {
            Some(form) if form.provider == id => form.input.read(cx).value().to_string(),
            _ => String::new(),
        };
        match set_key_command(&id, &key) {
            Ok(stdin) => {
                self.key_form = None;
                self.run(ProviderOp::SetKey, id, Some(stdin), cx);
            }
            Err(message) => {
                self.tell(id, ProviderControl::Key, Err(message));
                cx.notify();
            }
        }
    }

    /// Deletes the stored key for `id` and drops any open draft.
    pub fn remove_api_key(&mut self, id: String, cx: &mut Context<Self>) {
        if !ProviderOp::DeleteKey.may_start(self.running(&id)) {
            return;
        }
        self.key_form = None;
        self.run(ProviderOp::DeleteKey, id, None, cx);
    }

    /// The rows to show: the scope's listed providers, then (all purposes
    /// only) configured providers the list does not have.
    fn visible_rows(&self, config: &Config) -> Vec<ProviderRow> {
        let mut rows: Vec<ProviderRow> = self.rows().cloned().collect();
        if self.scope == ProviderScope::All {
            for (id, pc) in &config.providers {
                if !self.rows.iter().any(|r| &r.id == id) {
                    rows.push(ProviderRow {
                        id: id.clone(),
                        name: id.clone(),
                        purpose: None,
                        enabled: pc.enabled,
                        consented: pc.consented_at.is_some(),
                        status: None,
                        consent_text: None,
                        http: false,
                        location: ProviderLocation::Unknown,
                        has_key: None,
                    });
                }
            }
        }
        rows
    }

    /// The line of `control` on provider `id`'s row (shown as `name`): a
    /// Pending wait while its command runs, else its last outcome (Done or
    /// Failed), else nothing. Outcomes are words and a role (Status when
    /// done, Alert when failed), not only a colour, and stay until the next
    /// action on the same control; another control's wait or outcome never
    /// replaces them.
    fn control_line(
        &self,
        id: &str,
        name: &str,
        control: ProviderControl,
        cx: &App,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let line = control.line_id(id);
        if let Some(op) = self.running(id).iter().find(|op| op.control() == control) {
            return Some(kit::status_line(
                line,
                StatusKind::Pending,
                op.waiting(name),
                cx,
            ));
        }
        let (kind, text) = outcome(self.result(id, control)?.clone());
        Some(kit::status_line(line, kind, text, cx))
    }
}

/// Shown instead of a consent dialog when the registry gave no consent text.
const NO_CONSENT_TEXT: &str =
    "This provider's terms aren't available, so it can't be turned on here.";

pub(crate) fn parse_providers(v: &Value) -> Vec<ProviderRow> {
    let rows = v["providers"].as_array().or_else(|| v.as_array());
    rows.map(|rows| {
        rows.iter()
            .filter_map(|r| {
                let id = r["id"].as_str()?.to_owned();
                Some(ProviderRow {
                    name: r["name"]
                        .as_str()
                        .map(str::trim)
                        .filter(|n| !n.is_empty())
                        .unwrap_or(&id)
                        .to_owned(),
                    purpose: r["purpose"].as_str().map(str::to_owned),
                    enabled: r["enabled"].as_bool().unwrap_or(false),
                    consented: r["consented"].as_bool().unwrap_or(false)
                        || r["consented_at"].is_string(),
                    status: r["detection"]["status"].as_str().map(str::to_owned),
                    consent_text: r["consent_text"]
                        .as_str()
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(str::to_owned),
                    http: r["runtime"].as_str() == Some("openai-compatible-http"),
                    location: match r["location"].as_str() {
                        Some("local") => ProviderLocation::Local,
                        Some("cloud") => ProviderLocation::Cloud,
                        Some("cli") => ProviderLocation::Cli,
                        _ => ProviderLocation::Unknown,
                    },
                    has_key: r["has_key"].as_bool(),
                    id,
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

/// `providers <verb> <id>`; enabling states the human's acceptance.
fn provider_args(verb: &str, id: &str) -> Vec<OsString> {
    let mut a = args(["providers", verb, id]);
    if verb == "enable" {
        a.push("--accept".into());
    }
    a
}

/// A fresh masked field for an API key.
fn masked_key_input(window: &mut Window, cx: &mut App) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("API key")
            .masked(true)
    })
}

/// The stdin of `providers set-key <id>`: the key never goes into argv.
fn set_key_command(id: &str, key: &str) -> std::result::Result<Vec<u8>, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("Type the API key for {id} first."));
    }
    Ok(key.as_bytes().to_vec())
}

/// The provider a command ran for: its id and the name it is shown by.
struct Tested<'a> {
    id: &'a str,
    name: &'a str,
}

impl Tested<'_> {
    /// “Couldn't use <provider>. {Cause}.”: a failed provider test. The
    /// CLI's “<id> returned <what>” reads “It returned <what>”, so the
    /// provider is named once; a cause that starts with an identifier (the
    /// id, a path, a model) keeps its case.
    fn failure(&self, cause: &str) -> String {
        let cause = cause.trim();
        let cause = match cause
            .strip_prefix(self.id)
            .and_then(|rest| rest.strip_prefix(" returned "))
        {
            Some(what) => format!("It returned {what}"),
            None => cause.to_owned(),
        };
        let keep_case = first_word(&cause) == self.id;
        match sentence(&cause, keep_case) {
            Some(cause) => format!("Couldn't use {}. {cause}", self.name),
            None => format!("Couldn't use {}.", self.name),
        }
    }
}

/// Why a refused `providers test` failed, in words (WRT-04). The CLI's own
/// verdict on the reply (`inference`, "inference error: <id> returned
/// <what>", written by BroScribe and holding no path) is kept so the row
/// says what came back; any other model or provider failure is a fixed
/// sentence, and every other refusal [`CliError::user_text`]'s words.
///
/// [`CliError::user_text`]: crate::jobs::CliError::user_text
fn test_failure_cause(e: &crate::jobs::CliError, id: &str) -> String {
    if e.code != "inference" {
        return e.user_text();
    }
    // raw-text-allowed: only the CLI's own verdict below is shown, BroScribe's words (SET-63).
    let answered: &str = &e.message;
    let message = answered
        .strip_prefix("inference error: ")
        .unwrap_or(answered);
    match message.strip_prefix(id) {
        Some(rest) if rest.starts_with(" returned ") => message.to_owned(),
        _ => "It didn't give a usable answer.".to_owned(),
    }
}

/// The outcome of `providers <verb>` for the provider `tested`. A passed
/// test names a model only when the reply does: `providers test` gives the
/// one the provider reported, else the one it was told to use, and none
/// when it ran its own default (an agent CLI whose help lacks the model
/// flag is never given the configured model). The configured model is
/// never assumed. A test that answers `ok: false` is a failure (SET-63):
/// its row shows a Failed line, announced as an alert, never a status like
/// a pass.
fn provider_summary(
    verb: &str,
    tested: &Tested<'_>,
    v: &Value,
) -> std::result::Result<String, String> {
    match verb {
        "enable" => return Ok("Enabled.".into()),
        "disable" => return Ok("Disabled; enabling it again asks for consent again.".into()),
        "set-key" => return Ok("API key stored.".into()),
        "delete-key" => {
            return Ok(if v["deleted"].as_bool() == Some(false) {
                "No API key was stored.".into()
            } else {
                "API key removed.".into()
            });
        }
        _ => {}
    }
    let field = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| v[*k].as_str().map(str::trim).filter(|d| !d.is_empty()))
            .map(str::to_owned)
    };
    match v["ok"].as_bool() {
        Some(true) => Ok(match field(&["model"]) {
            Some(model) => format!("Test passed: {model}"),
            None => "Test passed.".into(),
        }),
        Some(false) => Err(tested.failure(&field(&["message", "detail"]).unwrap_or_default())),
        None => Ok("Test finished.".into()),
    }
}

/// The first word of `text`, without the punctuation closing it.
fn first_word(text: &str) -> &str {
    text.split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_end_matches([':', ',', ';', '.', '!', '?', '…'])
}

/// `cause` as a sentence: capitalised and closed with a full stop; `None`
/// when there is nothing to say.
fn cause_sentence(cause: &str) -> Option<String> {
    sentence(cause, false)
}

/// `text` closed with a full stop, its first letter capitalised unless
/// `keep_case` or it starts with an identifier (a word with a digit or a
/// sign such as `-`, `_`, `/` or `.`), whose case is part of it; `None` when
/// there is nothing to say.
fn sentence(text: &str, keep_case: bool) -> Option<String> {
    let text = text.trim();
    let mut chars = text.chars();
    let first = chars.next()?;
    let identifier = first_word(text)
        .chars()
        .any(|c| !c.is_alphabetic() && !matches!(c, '\'' | '’'));
    let mut sentence: String = if keep_case || identifier {
        text.to_owned()
    } else {
        first.to_uppercase().chain(chars).collect()
    };
    if !sentence.ends_with(['.', '!', '?', '…']) {
        sentence.push('.');
    }
    Some(sentence)
}

/// The Failed line when the provider list can't be read (`cause` is
/// [`CliError::user_text`]'s words).
///
/// [`CliError::user_text`]: crate::jobs::CliError::user_text
fn list_failure(scope: ProviderScope, cause: &str) -> String {
    match scope {
        ProviderScope::All => failure_text(
            "Couldn't detect providers.",
            &cause_sentence(cause).unwrap_or_default(),
            "Only configured providers are listed.",
        ),
        ProviderScope::Llm => failure_text(
            "Couldn't list the providers.",
            &cause_sentence(cause).unwrap_or_default(),
            "",
        ),
    }
}

impl Render for ProvidersView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let config = self.state.read(cx).config().clone();
        let mut group = kit::stack()
            .w_full()
            .id("providers")
            .role(Role::Group)
            .aria_label("AI providers");
        if let Some(cause) = &self.error {
            group = group.child(kit::retry_line(
                "providers-error",
                list_failure(self.scope, cause),
                cx,
                |this, _, cx| this.reload(cx),
            ));
        }
        if !self.loaded && self.error.is_none() {
            return group.child(kit::status_line(
                "providers-loading",
                StatusKind::Pending,
                "Looking for providers…",
                cx,
            ));
        }
        let rows = self.visible_rows(&config);
        let summaries: Vec<_> = rows
            .iter()
            .filter(|row| row.purpose.as_deref() != Some("text_embedding"))
            .collect();
        let (missing, available): (Vec<_>, Vec<_>) =
            summaries.into_iter().partition(|row| row.not_installed());
        let mut llm = kit::stack().w_full().when(!self.hide_unavailable, |el| {
            el.child(crate::ui::heading(
                "provider-summaries",
                "Summaries and templates",
            ))
        });
        for provider in available {
            llm = llm.child(self.provider_row(provider, &config, cx));
        }
        if !missing.is_empty() {
            llm = llm.child(kit::disclosure(
                "provider-not-installed",
                if self.hide_unavailable {
                    format!("Not found on this computer ({})", missing.len())
                } else {
                    format!("Not installed ({})", missing.len())
                },
                self.show_missing,
                cx,
                |this, _, cx| {
                    this.show_missing = !this.show_missing;
                    cx.notify();
                },
            ));
            if self.show_missing {
                if self.hide_unavailable {
                    llm = llm
                        .children(missing.into_iter().map(|provider| {
                            muted(
                                format!("provider-unavailable-{}", provider.id),
                                provider.name.clone(),
                                cx,
                            )
                        }))
                        .child(muted(
                            "providers-install-help",
                            "Install one, then come back to turn it on.",
                            cx,
                        ));
                } else {
                    llm = llm.children(
                        missing
                            .into_iter()
                            .map(|provider| self.provider_row(provider, &config, cx)),
                    );
                }
            }
        }
        group = group.child(llm);
        if self.scope == ProviderScope::All {
            group = group.child(kit::stack().w_full().child(crate::ui::heading("provider-search", "Search"))
                .child(muted("provider-search-help", "Finds meetings by meaning on a server you set. BroScribe sends transcript text to that server.", cx))
                .children(rows.iter().filter(|row| row.purpose.as_deref() == Some("text_embedding"))
                    .map(|provider| self.provider_row(provider, &config, cx))));
        }
        if rows.is_empty() {
            group = group.child(muted("providers-none", "No provider is available.", cx));
        }
        group
    }
}

impl ProvidersView {
    fn provider_row(
        &mut self,
        provider: &ProviderRow,
        config: &Config,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id = provider.id.as_str();
        let pc = config.provider(id);
        let on = pc.map_or(provider.enabled && provider.consented, |p| p.is_consented());
        let missing = provider.not_installed();
        let needs_setup = provider.http && !http_configured(pc);
        let running = self.running(id);
        let configuring = self.is_configuring(id);
        let changing = configuring || !ProviderOp::Disable.may_start(running);
        let state = match (
            on,
            missing,
            self.hide_unavailable && provider.has_key == Some(true),
        ) {
            (true, true, _) => "On · not installed",
            (true, false, true) => "On · API key stored",
            (true, false, false) => "On",
            (false, true, _) => "Off · not installed",
            (false, false, _) => "Off",
        };
        let mut actions = div()
            .flex()
            .items_center()
            .gap_1()
            .flex_none()
            .min_w(px(176.))
            .justify_end();
        if on {
            let id = id.to_owned();
            let test_id = id.clone();
            actions = actions
                .child(
                    Button::new(format!("provider-disable-{id}"))
                        .small()
                        .label("Turn off")
                        .disabled(changing)
                        .on_click(cx.listener(move |this, _, _, cx| this.disable(id.clone(), cx))),
                )
                .child(
                    Button::new(format!("provider-test-{test_id}"))
                        .small()
                        .label("Test")
                        .disabled(
                            needs_setup || configuring || !ProviderOp::Test.may_start(running),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.test(test_id.clone(), cx)),
                        ),
                );
        } else {
            let opener = self
                .enable_focus
                .entry(id.to_owned())
                .or_insert_with(|| cx.focus_handle())
                .clone();
            let terms_provider = provider.clone();
            actions = actions.child(
                Button::new(format!("provider-enable-{id}"))
                    .small()
                    .label("Turn on")
                    .track_focus(&opener)
                    .disabled(changing || needs_setup || missing)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.show_terms(&terms_provider, opener.clone(), window, cx)
                    })),
            );
        }
        if provider.http {
            let setup = provider.clone();
            let opener = self
                .setup_focus
                .entry(id.to_owned())
                .or_insert_with(|| cx.focus_handle())
                .clone();
            actions = actions.child(
                Button::new(format!("provider-key-{id}"))
                    .small()
                    .label("Set up…")
                    .track_focus(&opener)
                    .disabled(changing)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_http_form(&setup, opener.clone(), window, cx)
                    })),
            );
        }
        let line = kit::list_row(format!("provider-row-{id}"), false, false, cx)
            .items_center()
            .tooltip(kit::title_tooltip(provider.name.clone()))
            .child(
                text_line(
                    format!("provider-name-{id}"),
                    Role::Label,
                    provider.name.clone(),
                )
                .flex_1()
                .min_w_0()
                .truncate(),
            )
            .child(if self.hide_unavailable {
                muted(format!("provider-state-{id}"), state, cx)
            } else {
                kit::status_line(format!("provider-state-{id}"), StatusKind::Info, state, cx)
                    .w(px(116.))
                    .flex_none()
            })
            .child(kit::row_trailing(actions));
        let preferred = id.to_owned();
        kit::stack()
            .w_full()
            .id(format!("provider-settings-{id}"))
            .child(line)
            .when(
                provider.http && !(self.hide_unavailable && on && provider.has_key == Some(true)),
                |el| {
                    el.child(muted(
                        format!("provider-key-state-{id}"),
                        match provider.has_key {
                            Some(true) => "API key stored",
                            Some(false) => "No API key",
                            None => "API key status unknown",
                        },
                        cx,
                    ))
                },
            )
            .when(provider.http && provider.has_key == Some(true), |el| {
                let remove = provider.clone();
                let remove_focus = self
                    .remove_key_focus
                    .entry(id.to_owned())
                    .or_insert_with(|| cx.focus_handle())
                    .clone();
                el.child(
                    Button::new(format!("provider-key-delete-{id}"))
                        .label("Remove key")
                        .small()
                        .ghost()
                        .track_focus(&remove_focus)
                        .disabled(changing)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.ask_to_remove_key(&remove, remove_focus.clone(), window, cx)
                        })),
                )
            })
            .when(needs_setup, |el| {
                el.child(kit::status_line(
                    format!("provider-setup-needed-{id}"),
                    StatusKind::Attention,
                    "Set the server address and model first.",
                    cx,
                ))
            })
            .when(
                on && provider.purpose.as_deref() == Some("llm")
                    && !config.profile.llm.is_empty()
                    && !config.profile.llm.iter().any(|entry| entry == id),
                |el| {
                    el.child(kit::status_line(
                        format!("provider-preference-{id}"),
                        StatusKind::Attention,
                        "Not used for automatic summaries.",
                        cx,
                    ))
                    .child(
                        Button::new(format!("provider-use-for-summaries-{id}"))
                            .label("Use for automatic summaries")
                            .ghost()
                            .small()
                            .disabled(changing)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.prefer(preferred.clone(), cx)
                            })),
                    )
                },
            )
            .when(configuring, |el| {
                el.child(kit::status_line(
                    format!("provider-saving-{id}"),
                    StatusKind::Pending,
                    "Saving…",
                    cx,
                ))
            })
            .children(
                [
                    ProviderControl::Key,
                    ProviderControl::Consent,
                    ProviderControl::Test,
                ]
                .into_iter()
                .filter_map(|control| self.control_line(id, &provider.name, control, cx)),
            )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use broscribe_core::Library;
    use broscribe_core::config::ProviderConfig;
    use broscribe_platform::fakes::FakePaths;
    use broscribe_platform::resolve_paths;
    use gpui::TestAppContext;

    use super::*;

    use crate::test_support::{ps_path, write_cli};

    /// The synthetic agent CLI the fake registry lists with consent text.
    pub(crate) const AGENT: &str = "synthetic-agent";

    /// A synthetic `broscribe` (argv logged to `args.log`, a `set-key` stdin
    /// to `stdin.bin`) whose registry lists [`AGENT`] (LLM, registry consent
    /// text, not found), `remote` (LLM over HTTP, no consent text) and `embed`
    /// (text embedding). `enable` installs `staged/config.toml` (when
    /// present) as the library's config, as the real CLI saves its consent.
    /// `test` answers `test-reply.json`, or fails with `test-error.json`,
    /// when present. Nothing reaches a vendor.
    pub(crate) fn synthetic_providers_cli(dir: &Path, data_dir: &Path) -> PathBuf {
        let list = dir.join("providers.json");
        std::fs::write(
            &list,
            serde_json::json!({"providers": [
                {"id": AGENT, "name": "Synthetic Agent", "runtime": "agent-cli",
                 "purpose": "llm", "enabled": false, "detection": {"status": "not_found"},
                 "consent_text": "Synthetic registry terms.", "has_key": null},
                {"id": "remote", "runtime": "openai-compatible-http", "purpose": "llm",
                 "enabled": false, "has_key": false},
                {"id": "embed", "name": "Synthetic Embeddings",
                 "runtime": "openai-compatible-http", "purpose": "text_embedding",
                 "enabled": false, "consent_text": "Embedding terms.", "has_key": false}
            ]})
            .to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(data_dir).unwrap();
        write_cli(
            &dir.join("synthetic-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{log}'\n\
                 if [ \"$5\" = set-key ]; then cat > '{stdin}'; fi\n\
                 if [ \"$5\" = enable ] && [ -f '{staged}' ]; then cp '{staged}' '{config}'; fi\n\
                 if [ \"$5\" = test ] && [ -f '{reply}' ]; then cat '{reply}'; exit 0; fi\n\
                 if [ \"$5\" = test ] && [ -f '{error}' ]; then cat '{error}'; exit 1; fi\n\
                 cat '{list}'\n",
                log = dir.join("args.log").display(),
                stdin = dir.join("stdin.bin").display(),
                staged = Config::path(&dir.join("staged")).display(),
                config = Config::path(data_dir).display(),
                reply = dir.join("test-reply.json").display(),
                error = dir.join("test-error.json").display(),
                list = list.display(),
            ),
            &format!(
                "fx_log_args {log};if((fx_arg 5) -ceq 'set-key'){{fx_save_stdin {stdin}}};if((fx_arg 5) -ceq 'enable' -and (fx_exists {staged})){{[IO.File]::Copy({staged},{config},$true)}};if((fx_arg 5) -ceq 'test' -and (fx_exists {reply})){{fx_cat {reply};exit 0}};if((fx_arg 5) -ceq 'test' -and (fx_exists {error})){{fx_cat {error};exit 1}};fx_cat {list}",
                log = ps_path(&dir.join("args.log")),
                stdin = ps_path(&dir.join("stdin.bin")),
                staged = ps_path(&Config::path(&dir.join("staged"))),
                config = ps_path(&Config::path(data_dir)),
                reply = ps_path(&dir.join("test-reply.json")),
                error = ps_path(&dir.join("test-error.json")),
                list = ps_path(&list),
            ),
        )
    }

    /// The config the synthetic CLI saves when [`AGENT`] is enabled.
    pub(crate) fn stage_enabled_agent(dir: &Path) {
        let mut config = Config::default();
        config.providers.insert(
            AGENT.into(),
            ProviderConfig {
                enabled: true,
                consented_at: Some("2026-11-14T00:00:00+00:00".parse().unwrap()),
                ..Default::default()
            },
        );
        config.save(&dir.join("staged")).unwrap();
    }

    pub(crate) fn args_log(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("args.log")).unwrap_or_default()
    }

    /// An [`AppState`] whose CLI is [`synthetic_providers_cli`].
    pub(crate) fn providers_state(cx: &mut TestAppContext, dir: &Path) -> Entity<AppState> {
        providers_state_with(cx, dir, None)
    }

    /// [`providers_state`] starting from `config` when given, also saved as
    /// the data directory's config.
    fn providers_state_with(
        cx: &mut TestAppContext,
        dir: &Path,
        mut config: Option<Config>,
    ) -> Entity<AppState> {
        let paths = resolve_paths(&FakePaths::new(dir)).unwrap();
        let cli = synthetic_providers_cli(dir, &paths.data_dir);
        if let Some(config) = &mut config {
            config.save(&paths.data_dir).unwrap();
        }
        let config = config.unwrap_or_default();
        let library = Library::open(dir.join("lib")).unwrap();
        cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(paths, config, library, cli),
                cx,
            )
        })
    }

    pub(crate) fn agent_consented(state: &Entity<AppState>, cx: &TestAppContext) -> bool {
        state.read_with(cx, |s, _| {
            s.config()
                .provider(AGENT)
                .is_some_and(ProviderConfig::is_consented)
        })
    }

    fn providers_window(
        cx: &mut TestAppContext,
        state: &Entity<AppState>,
        scope: ProviderScope,
    ) -> gpui::WindowHandle<ProvidersView> {
        cx.update(gpui::init);
        let state = state.clone();
        let window = cx.add_window(move |_, _| ProvidersView::new(state, scope));
        window.update(cx, |this, _, cx| this.reload(cx)).unwrap();
        cx.run_until_parked();
        window
    }

    fn type_key(this: &ProvidersView, text: &str, window: &mut Window, cx: &mut App) {
        let input = this.key_form.as_ref().unwrap().input.clone();
        input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    }

    /// The open form's provider and draft.
    fn key_draft(this: &ProvidersView, cx: &App) -> Option<(String, String)> {
        this.key_form.as_ref().map(|form| {
            (
                form.provider.clone(),
                form.input.read(cx).value().to_string(),
            )
        })
    }

    fn providers_root(
        cx: &mut TestAppContext,
        state: Entity<AppState>,
    ) -> (
        Entity<ProvidersView>,
        gpui::WindowHandle<gpui::component::Root>,
    ) {
        cx.update(crate::ui::theme::init);
        let mut entity = None;
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|_| ProvidersView::new(state, ProviderScope::All));
            entity = Some(view.clone());
            gpui::component::Root::new(view, window, cx)
        });
        let view = entity.unwrap();
        view.update(cx, |view, cx| view.reload(cx));
        cx.run_until_parked();
        (view, window)
    }

    #[gpui::test]
    fn provider_terms_and_key_removal_wait_for_explicit_confirmation(cx: &mut TestAppContext) {
        use crate::ui::kit::dialog_gate::{click, draw};
        let dir = tempfile::tempdir().unwrap();
        let state = providers_state(cx, dir.path());
        let (view, window) = providers_root(cx, state);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let open = |visual: &mut gpui::VisualTestContext| {
            visual.update(|window, cx| {
                view.update(cx, |this, cx| {
                    let row = this.rows().find(|row| row.id == AGENT).unwrap().clone();
                    this.show_terms(&row, cx.focus_handle(), window, cx);
                });
            })
        };
        open(&mut visual);
        draw(&mut visual);
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(
                tree.nodes
                    .iter()
                    .any(|node| node.role == "AlertDialog"
                        && node.name == "Turn on Synthetic Agent?")
            );
            tree.find("provider-terms-text");
            assert!(
                gpui::base::test_support::snapshots(window)
                    .iter()
                    .any(|node| node.label() == Some("Cancel") && node.focused() == Some(true))
            );
        });
        visual.simulate_keystrokes("escape");
        visual.run_until_parked();
        assert!(!args_log(dir.path()).contains("providers enable"));
        open(&mut visual);
        click(&mut visual, "provider-terms-accept", Role::Button);
        assert!(args_log(dir.path()).contains("providers enable synthetic-agent --accept"));
        visual.update(|window, cx| {
            view.update(cx, |this, cx| {
                let row = this.rows().find(|row| row.id == "remote").unwrap().clone();
                this.ask_to_remove_key(&row, cx.focus_handle(), window, cx);
            });
        });
        assert!(!args_log(dir.path()).contains("providers delete-key remote"));
        click(&mut visual, "provider-remove-key-confirm", Role::Button);
        assert!(args_log(dir.path()).contains("providers delete-key remote"));
        assert!(key_store_note().contains(
            broscribe_platform_native::NativePlatform::host_platform().secret_store_name()
        ));
    }

    #[gpui::test]
    fn provider_dialog_cancellation_restores_the_rendered_opener(cx: &mut TestAppContext) {
        use crate::ui::kit::dialog_gate::{click, draw};
        let dir = tempfile::tempdir().unwrap();
        let state = providers_state(cx, dir.path());
        let (view, window) = providers_root(cx, state);
        view.update(cx, |this, cx| {
            let agent = this.rows.iter_mut().find(|row| row.id == AGENT).unwrap();
            agent.status = Some("ready".into());
            let remote = this.rows.iter_mut().find(|row| row.id == "remote").unwrap();
            remote.has_key = Some(true);
            cx.notify();
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        for (opener, cancel) in [
            ("provider-enable-synthetic-agent", None),
            (
                "provider-key-delete-remote",
                Some("provider-remove-key-cancel"),
            ),
        ] {
            click(&mut visual, opener, Role::Button);
            draw(&mut visual);
            if let Some(cancel) = cancel {
                click(&mut visual, cancel, Role::Button);
            } else {
                visual.simulate_keystrokes("escape");
            }
            draw(&mut visual);
            visual.update(|window, _| {
                let nodes = gpui::base::test_support::snapshots(window);
                let button = nodes
                    .iter()
                    .find(|node| {
                        node.role() == Some(Role::Button)
                            && node
                                .path()
                                .last()
                                .is_some_and(|part| part.to_string() == opener)
                    })
                    .unwrap();
                assert_eq!(button.focused(), Some(true), "{opener}");
            });
        }
    }

    #[gpui::test]
    fn http_setup_rejects_remote_http_and_keeps_the_key_on_stdin(cx: &mut TestAppContext) {
        use crate::ui::kit::dialog_gate::draw;
        let dir = tempfile::tempdir().unwrap();
        let state = providers_state(cx, dir.path());
        let (view, window) = providers_root(cx, state);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            tree.find("provider-summaries");
            tree.find("provider-search");
            assert!(tree.nodes.iter().all(|node| node.name != "Synthetic Agent"));
            view.update(cx, |this, cx| {
                let row = this.rows().find(|row| row.id == "remote").unwrap().clone();
                this.open_http_form(&row, cx.focus_handle(), window, cx);
                let form = this.http_form.as_ref().unwrap();
                form.address.update(cx, |input, cx| {
                    input.set_value("http://example.invalid/v1", window, cx)
                });
                form.key.update(cx, |input, cx| {
                    input.set_value("synthetic-stdin-key", window, cx)
                });
                this.save_http_form(window, cx);
                assert!(this.http_form.as_ref().unwrap().invalid_address);
            });
        });
        draw(&mut visual);
        assert!(!args_log(dir.path()).contains("providers configure"));
        visual.update(|window, cx| {
            view.update(cx, |this, cx| {
                let form = this.http_form.as_ref().unwrap();
                form.address.update(cx, |input, cx| {
                    input.set_value("http://localhost:11434/v1", window, cx)
                });
                form.model.update(cx, |input, cx| {
                    input.set_value("invented-model", window, cx)
                });
                this.save_http_form(window, cx);
            })
        });
        visual.run_until_parked();
        let log = args_log(dir.path());
        assert!(log.contains(
            "providers configure remote --base-url http://localhost:11434/v1 --model invented-model"
        ));
        assert!(log.contains("providers set-key remote"));
        assert!(!log.contains("synthetic-stdin-key"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("stdin.bin"))
                .unwrap()
                .trim(),
            "synthetic-stdin-key"
        );
    }

    #[test]
    fn summary_choices_only_list_enabled_llm_providers_and_follow_profile_order() {
        let rows = parse_providers(&serde_json::json!({"providers": [
            {"id":"a", "name":"Acorn", "purpose":"llm"},
            {"id":"b", "name":"Beech", "purpose":"llm"},
            {"id":"c", "name":"Clover", "purpose":"text_embedding"},
            {"id":"d", "name":"Daisy", "purpose":"llm"}
        ]}));
        let mut config = Config::default();
        for id in ["a", "b", "c"] {
            config.providers.insert(
                id.into(),
                ProviderConfig {
                    enabled: true,
                    consented_at: Some(broscribe_core::schema::now()),
                    ..Default::default()
                },
            );
        }
        config.profile.llm = vec!["b".into(), "c".into(), "d".into(), "a".into()];
        assert_eq!(
            summary_rows(&rows, &config)
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
    }

    #[test]
    fn providers_list_parsing_keeps_registry_consent_name_status_and_key_state() {
        let v = serde_json::json!({"providers":[
            {"id":"agent","name":"Synthetic Agent","runtime":"agent-cli","purpose":"llm",
             "consent_text":"Registry terms for agent.","has_key":null,
             "detection":{"status":"not_found"}},
            {"id":"remote","runtime":"openai-compatible-http","purpose":"llm","has_key":true,
             "consent":"ignored","consented_at":"2026-11-14T00:00:00+00:00"},
            {"id":"blank","name":" ","purpose":"text_embedding","consent_text":"  "}
        ]});
        let rows = parse_providers(&v);
        assert_eq!(
            rows[0].consent_text.as_deref(),
            Some("Registry terms for agent.")
        );
        assert_eq!(rows[0].name, "Synthetic Agent");
        assert_eq!(rows[0].status.as_deref(), Some("not_found"));
        assert!(!rows[0].http && rows[0].has_key.is_none());
        // No registry text: no consent can be given.
        assert_eq!(rows[1].consent_text, None);
        assert!(rows[1].http && rows[1].consented && !rows[1].enabled);
        assert_eq!(rows[1].has_key, Some(true));
        // A blank display name falls back to the id.
        assert_eq!(rows[2].name, "blank");
        assert_eq!(rows[2].consent_text, None);
        // Summaries list LLM providers only; Settings lists every purpose.
        let llm: Vec<_> = rows
            .iter()
            .filter(|r| ProviderScope::Llm.includes(r))
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(llm, ["agent", "remote"]);
        assert!(rows.iter().all(|r| ProviderScope::All.includes(r)));

        // Only an accepted enable states acceptance.
        assert_eq!(
            provider_args("enable", "agent"),
            args(["providers", "enable", "agent", "--accept"])
        );
        assert_eq!(
            provider_args("disable", "agent"),
            args(["providers", "disable", "agent"])
        );
        assert!(set_key_command("remote", "  ").is_err());
        assert_eq!(
            set_key_command("remote", " sk-synthetic\n").unwrap(),
            b"sk-synthetic"
        );
        let remote = Tested {
            id: "remote",
            name: "Remote",
        };
        assert!(
            provider_summary("disable", &remote, &Value::Null)
                .unwrap()
                .contains("consent")
        );
    }

    /// SET-63: a failed test is an error, read as “Couldn't use <provider>.
    /// {Cause}.”, the provider named once and an identifier never
    /// capitalised; a pass names a model only when the reply names the one
    /// that ran.
    #[test]
    fn a_failed_provider_test_is_an_error_and_a_pass_says_what_answered() {
        let agent = Tested {
            id: AGENT,
            name: "Synthetic Agent",
        };
        let summary = |v: Value| provider_summary("test", &agent, &v);
        // An agent CLI that ran its own default: `providers test` names no
        // model, whatever model is configured.
        let pass = serde_json::json!({"id": AGENT, "ok": true, "detail": "a valid reply",
            "model": null, "requested_model": null, "model_reported": false,
            "elapsed_ms": 5});
        assert_eq!(summary(pass), Ok("Test passed.".to_owned()));
        assert_eq!(
            summary(serde_json::json!({"ok": true, "model": "  "})),
            Ok("Test passed.".to_owned())
        );
        // A model in the reply is the one that ran.
        assert_eq!(
            summary(serde_json::json!({"ok": true, "model": "synthetic-model",
                "requested_model": "synthetic-model", "model_reported": false})),
            Ok("Test passed: synthetic-model".to_owned())
        );
        // An `ok: false` answer is still a failure.
        assert_eq!(
            summary(serde_json::json!({"ok": false, "detail": "an unusable reply (synthetic)"})),
            Err("Couldn't use Synthetic Agent. An unusable reply (synthetic).".to_owned())
        );
        assert_eq!(
            summary(serde_json::json!({"ok": false})),
            Err("Couldn't use Synthetic Agent.".to_owned())
        );
        assert_eq!(summary(Value::Null), Ok("Test finished.".to_owned()));

        // The real CLI fails a test as `inference` with “<id> returned …”:
        // its verdict on the reply is kept, without the prefix.
        let refused = |code: &str, message: &str| crate::jobs::CliError::answered(code, message);
        let cause = |code: &str, message: &str| test_failure_cause(&refused(code, message), AGENT);
        assert_eq!(
            agent.failure(&cause(
                "inference",
                &format!("inference error: {AGENT} returned an unusable reply (synthetic)")
            )),
            "Couldn't use Synthetic Agent. It returned an unusable reply (synthetic)."
        );
        // Any other model failure is a fixed sentence: its words (a binary,
        // a status, a path) never reach the row.
        assert_eq!(
            agent.failure(&cause(
                "inference",
                "inference error: /Users/synthetic/bin/agent failed (exit status: 9)"
            )),
            "Couldn't use Synthetic Agent. It didn't give a usable answer."
        );
        // Every other refusal is `CliError::user_text`'s words, never a path.
        assert_eq!(
            agent.failure(&cause(
                "io",
                "/Users/synthetic/Library/BroScribe/config.toml: permission denied"
            )),
            "Couldn't use Synthetic Agent. BroScribe couldn't read or write a file."
        );
        assert_eq!(
            agent.failure(&cause("consent_required", "the agent timed out")),
            "Couldn't use Synthetic Agent. The agent timed out."
        );
        // A cause keeps an identifier's case and is otherwise a sentence.
        assert_eq!(
            agent.failure(&format!("{AGENT} is not installed")),
            format!("Couldn't use Synthetic Agent. {AGENT} is not installed.")
        );
        assert_eq!(
            agent.failure("gpt-synthetic refused the request"),
            "Couldn't use Synthetic Agent. gpt-synthetic refused the request."
        );
        assert_eq!(
            agent.failure("can't reach the provider"),
            "Couldn't use Synthetic Agent. Can't reach the provider."
        );
        assert_eq!(
            list_failure(ProviderScope::All, "synthetic cause"),
            "Couldn't detect providers. Synthetic cause. Only configured providers are listed."
        );
        assert_eq!(
            list_failure(ProviderScope::Llm, ""),
            "Couldn't list the providers."
        );
    }

    /// SET-05 and SET-63: each provider row's texts are named nodes; the
    /// wait for the list is a status, a list or test failure an alert beside
    /// its row, a pass a status.
    #[gpui::test]
    fn provider_lines_are_named_and_failures_are_alerts(cx: &mut TestAppContext) {
        use crate::ui::spoken_log::{self, assert_distinct, has, node};

        let dir = tempfile::tempdir().unwrap();
        let state = providers_state(cx, dir.path());
        let window = providers_window(cx, &state, ProviderScope::All);
        let named = |cx: &mut TestAppContext, change: &dyn Fn(&mut ProvidersView)| {
            window
                .update(cx, |this, window, cx| {
                    change(this);
                    spoken_log::while_building(|| Render::render(this, window, cx)).1
                })
                .unwrap()
        };

        let loading = named(cx, &|this| this.loaded = false);
        assert_eq!(
            node(&loading, "providers-loading"),
            (Role::Status, "Looking for providers…")
        );

        // The fields that repeat word for word across rows.
        let rows = [
            "provider-state-*",
            "provider-key-state-*",
            "provider-setup-needed-*",
            "provider-saving-*",
        ];
        let loaded = named(cx, &|this| {
            this.loaded = true;
            this.show_missing = true;
        });
        assert_distinct(&loaded, &rows);
        assert!(!has(&loaded, "providers-loading"));
        assert_eq!(
            node(&loaded, &format!("provider-name-{AGENT}")),
            (Role::Label, "Synthetic Agent")
        );
        assert_eq!(
            node(&loaded, "provider-key-state-remote"),
            (Role::Label, "No API key")
        );

        let unlisted = named(cx, &|this| this.error = Some("synthetic cause".into()));
        assert_eq!(
            node(&unlisted, "providers-error"),
            (
                Role::Alert,
                "Couldn't detect providers. Synthetic cause. Only configured providers are \
                 listed."
            )
        );
        named(cx, &|this| this.error = None);

        // The provider's reply is rejected: like the real CLI, the fixture
        // exits with an `inference` error. An alert on its row.
        let result_id = format!("provider-result-{AGENT}");
        let error = dir.path().join("test-error.json");
        std::fs::write(
            &error,
            serde_json::json!({"error": {"code": "inference", "message": format!(
                "inference error: {AGENT} returned an unusable reply (synthetic)"
            )}})
            .to_string(),
        )
        .unwrap();
        window
            .update(cx, |this, _, cx| this.test(AGENT.into(), cx))
            .unwrap();
        cx.run_until_parked();
        let failed = named(cx, &|_| {});
        assert_eq!(
            node(&failed, &result_id),
            (
                Role::Alert,
                "Couldn't use Synthetic Agent. It returned an unusable reply (synthetic)."
            )
        );

        // Testing again: while the reply is held, the same line is a wait,
        // never the previous failure.
        std::fs::remove_file(&error).unwrap();
        let reply = dir.path().join("test-reply.json");
        let pass = |model: Option<&str>| {
            serde_json::json!({"id": AGENT, "ok": true, "detail": "a valid reply",
                "model": model, "requested_model": model, "model_reported": false,
                "elapsed_ms": 5})
            .to_string()
        };
        std::fs::write(&reply, pass(None)).unwrap();
        let testing = window
            .update(cx, |this, window, cx| {
                this.test(AGENT.into(), cx);
                spoken_log::while_building(|| Render::render(this, window, cx)).1
            })
            .unwrap();
        assert_distinct(&testing, &rows);
        assert_eq!(
            node(&testing, &result_id),
            (Role::Status, "Testing Synthetic Agent…")
        );
        assert_eq!(
            testing.iter().filter(|(id, ..)| *id == result_id).count(),
            1
        );

        // The pass replaces the wait on the same line, as a status; the
        // reply names no model.
        cx.run_until_parked();
        let passed = named(cx, &|_| {});
        assert_eq!(node(&passed, &result_id), (Role::Status, "Test passed."));
        // Another provider's row never shows this outcome.
        assert!(!has(&passed, "provider-result-remote"));

        // A model is configured, but the agent CLI can't be given it and
        // runs its own default: the reply names no model, so neither does
        // the line.
        let data_dir = resolve_paths(&FakePaths::new(dir.path())).unwrap().data_dir;
        let mut config = Config::default();
        config.providers.insert(
            AGENT.into(),
            ProviderConfig {
                enabled: true,
                consented_at: Some("2026-11-14T00:00:00+00:00".parse().unwrap()),
                model: Some("synthetic-model".into()),
                ..Default::default()
            },
        );
        config.save(&data_dir).unwrap();
        window
            .update(cx, |this, _, cx| this.test(AGENT.into(), cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            state.read_with(cx, |s, _| s
                .config()
                .provider(AGENT)
                .and_then(|p| p.model.clone())),
            Some("synthetic-model".to_owned()),
            "the configured model was taken in"
        );
        let default_model = named(cx, &|_| {});
        assert_eq!(
            node(&default_model, &result_id),
            (Role::Status, "Test passed.")
        );

        // The reply names the model that ran: the line names it.
        std::fs::write(&reply, pass(Some("synthetic-model"))).unwrap();
        window
            .update(cx, |this, _, cx| this.test(AGENT.into(), cx))
            .unwrap();
        cx.run_until_parked();
        let named_model = named(cx, &|_| {});
        assert_eq!(
            node(&named_model, &result_id),
            (Role::Status, "Test passed: synthetic-model")
        );
    }

    /// Each control keeps its own line: a key refused while a test runs is
    /// shown and announced beside the key form at once, and the test's
    /// wait and later outcome never replace it.
    #[gpui::test]
    fn a_running_test_never_hides_the_key_controls_outcome(cx: &mut TestAppContext) {
        use broscribe_platform::AnnouncementPriority::{High, Medium};
        use broscribe_platform::fakes::FakeAnnouncer;

        use crate::ui::spoken_log::{self, assert_distinct, node};

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.providers.insert(
            "remote".into(),
            ProviderConfig {
                enabled: true,
                consented_at: Some("2026-11-14T00:00:00+00:00".parse().unwrap()),
                ..Default::default()
            },
        );
        let state = providers_state_with(cx, dir.path(), Some(config));
        std::fs::write(
            dir.path().join("test-reply.json"),
            serde_json::json!({"id": "remote", "ok": true, "detail": "a valid reply",
                "model": null, "requested_model": null, "model_reported": false,
                "elapsed_ms": 5})
            .to_string(),
        )
        .unwrap();
        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| {
            gpui::init(cx);
            crate::ui::install_announcer(announcer.clone(), cx);
        });
        let window = cx.add_window({
            let state = state.clone();
            move |_, _| ProvidersView::new(state, ProviderScope::All)
        });
        window.update(cx, |this, _, cx| this.reload(cx)).unwrap();
        cx.run_until_parked();
        // Draws a frame (live lines are announced when it ends) and returns
        // the nodes it named.
        let draw = |cx: &mut TestAppContext| {
            spoken_log::while_building(|| {
                cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                    .unwrap()
            })
            .1
        };
        draw(cx);
        announcer.take();
        let rows = [
            "provider-state-*",
            "provider-key-state-*",
            "provider-setup-needed-*",
            "provider-saving-*",
        ];
        let (test_line, key_line) = ("provider-result-remote", "provider-key-result-remote");
        let refused = "Type the API key for remote first.";

        // Test starts; then Save key with nothing typed is refused while the
        // test still runs.
        window
            .update(cx, |this, window, cx| {
                this.test("remote".into(), cx);
                this.open_key_form("remote".into(), window, cx);
                this.save_api_key("remote".into(), cx);
                assert_eq!(this.running("remote"), [ProviderOp::Test]);
            })
            .unwrap();
        let both = draw(cx);
        assert_distinct(&both, &rows);
        assert_eq!(node(&both, test_line), (Role::Status, "Testing remote…"));
        assert_eq!(node(&both, key_line), (Role::Alert, refused));
        assert_eq!(
            announcer.take(),
            [
                (refused.to_owned(), High),
                ("Testing remote…".to_owned(), Medium)
            ]
        );

        // The test's reply: its line passes, the key's alert stays (and is
        // not announced again).
        cx.run_until_parked();
        let passed = draw(cx);
        assert_distinct(&passed, &rows);
        assert_eq!(node(&passed, test_line), (Role::Status, "Test passed."));
        assert_eq!(node(&passed, key_line), (Role::Alert, refused));
        assert_eq!(announcer.take(), [("Test passed.".to_owned(), Medium)]);
        window
            .update(cx, |this, _, _| {
                assert_eq!(
                    this.result("remote", ProviderControl::Key),
                    Some(&Err(refused.to_owned()))
                );
                assert_eq!(
                    this.result("remote", ProviderControl::Test),
                    Some(&Ok("Test passed.".to_owned()))
                );
            })
            .unwrap();
    }

    /// An outcome goes when its control goes or is used again: a disabled
    /// provider loses its Test button and the last test's line; opening the
    /// API-key form drops the key's last line, as opening the consent
    /// dialog drops the consent's. Other controls and providers keep theirs.
    #[gpui::test]
    fn disable_and_a_new_key_form_drop_the_lines_they_outdate(cx: &mut TestAppContext) {
        use crate::ui::spoken_log::{self, has};

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.providers.insert(
            "remote".into(),
            ProviderConfig {
                enabled: true,
                consented_at: Some("2026-11-14T00:00:00+00:00".parse().unwrap()),
                ..Default::default()
            },
        );
        let state = providers_state_with(cx, dir.path(), Some(config));
        std::fs::write(
            dir.path().join("test-reply.json"),
            serde_json::json!({"id": "remote", "ok": true, "detail": "a valid reply",
                "model": null, "requested_model": null, "model_reported": false,
                "elapsed_ms": 5})
            .to_string(),
        )
        .unwrap();
        let window = providers_window(cx, &state, ProviderScope::All);
        let named = |cx: &mut TestAppContext| {
            window
                .update(cx, |this, window, cx| {
                    spoken_log::while_building(|| Render::render(this, window, cx)).1
                })
                .unwrap()
        };
        let refused = Err("Type the API key for remote first.".to_owned());
        let other_key = Err("synthetic embed key outcome".to_owned());

        // A passed test, a refused key, and another provider's key line.
        window
            .update(cx, |this, window, cx| {
                this.test("remote".into(), cx);
                this.open_key_form("remote".into(), window, cx);
                this.save_api_key("remote".into(), cx);
                this.close_key_form(cx);
                this.tell("embed".into(), ProviderControl::Key, other_key.clone());
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, window, cx| {
                assert_eq!(
                    this.result("remote", ProviderControl::Test),
                    Some(&Ok("Test passed.".to_owned()))
                );
                assert_eq!(this.result("remote", ProviderControl::Key), Some(&refused));
                // Reopening the form: the refusal goes; embed's line stays.
                this.open_key_form("remote".into(), window, cx);
                assert_eq!(this.result("remote", ProviderControl::Key), None);
                assert_eq!(this.result("embed", ProviderControl::Key), Some(&other_key));
                // Refused again, then the provider is disabled.
                this.save_api_key("remote".into(), cx);
                this.close_key_form(cx);
                this.disable("remote".into(), cx);
                // Until the disable succeeds, the test's line stays.
                assert_eq!(
                    this.result("remote", ProviderControl::Test),
                    Some(&Ok("Test passed.".to_owned()))
                );
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.result("remote", ProviderControl::Test), None);
                assert_eq!(
                    this.result("remote", ProviderControl::Consent),
                    Some(&Ok(
                        "Disabled; enabling it again asks for consent again.".to_owned()
                    ))
                );
                assert_eq!(this.result("remote", ProviderControl::Key), Some(&refused));
            })
            .unwrap();
        let disabled = named(cx);
        assert!(!has(&disabled, "provider-result-remote"), "{disabled:?}");
        assert!(has(&disabled, "provider-consent-result-remote"));
        assert!(has(&disabled, "provider-key-result-remote"));
    }

    #[gpui::test]
    fn provider_actions_go_through_the_cli(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = providers_state(cx, dir.path());
        let window = providers_window(cx, &state, ProviderScope::All);
        let log = || args_log(dir.path());
        window
            .update(cx, |this, _, cx| {
                let ids: Vec<_> = this.rows().map(|r| r.id.as_str()).collect();
                assert_eq!(ids, [AGENT, "remote", "embed"]);
                assert!(this.rows[1].http);
                // No consent question open: accepting does nothing.
                this.accept_consent("remote".into(), cx);
                assert_eq!(this.result("remote", ProviderControl::Consent), None);
                // Open, but the registry has no text for it: refused.
                this.request_consent("remote".into(), cx);
                this.accept_consent("remote".into(), cx);
                assert_eq!(
                    this.result("remote", ProviderControl::Consent),
                    Some(&Err(NO_CONSENT_TEXT.to_owned()))
                );
                assert!(this.running("remote").is_empty());
                assert_eq!(this.pending_consent(), None);
                this.disable("remote".into(), cx);
                assert_eq!(this.running("remote"), [ProviderOp::Disable]);
                // Another provider's commands are not held up by it.
                assert!(this.running(AGENT).is_empty());
                this.test(AGENT.into(), cx);
                assert_eq!(this.running(AGENT), [ProviderOp::Test]);
                // The same provider's other changes wait; its test may not
                // start while a change runs either.
                this.remove_api_key("remote".into(), cx);
                this.test("remote".into(), cx);
                assert_eq!(this.running("remote"), [ProviderOp::Disable]);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(!log().contains("providers enable"), "{}", log());
        assert!(log().contains("providers disable remote"), "{}", log());
        window
            .update(cx, |this, window, cx| {
                assert!(this.running("remote").is_empty() && this.running(AGENT).is_empty());
                this.open_key_form("remote".into(), window, cx);
                type_key(this, "sk-synthetic", window, cx);
                this.save_api_key("remote".into(), cx);
                assert!(this.key_form.is_none(), "the draft is dropped");
            })
            .unwrap();
        cx.run_until_parked();
        assert!(log().contains("providers set-key remote"), "{}", log());
        assert!(
            !log().contains("sk-synthetic"),
            "the key never reaches argv"
        );
        assert_eq!(
            std::fs::read(dir.path().join("stdin.bin")).unwrap(),
            b"sk-synthetic"
        );
        window
            .update(cx, |this, _, cx| {
                assert_eq!(
                    this.result("remote", ProviderControl::Key),
                    Some(&Ok("API key stored.".to_owned()))
                );
                this.remove_api_key("remote".into(), cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(log().contains("providers delete-key remote"), "{}", log());
    }

    #[gpui::test]
    fn api_key_draft_is_bound_to_its_provider(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = providers_state(cx, dir.path());
        let window = providers_window(cx, &state, ProviderScope::All);
        window
            .update(cx, |this, window, cx| {
                this.open_key_form("remote".into(), window, cx);
                type_key(this, "sk-for-remote", window, cx);
                // Reopening the same provider keeps the draft.
                this.open_key_form("remote".into(), window, cx);
                assert_eq!(
                    key_draft(this, cx),
                    Some(("remote".into(), "sk-for-remote".into()))
                );
                // Another provider's form starts empty.
                this.open_key_form("other".into(), window, cx);
                assert_eq!(key_draft(this, cx), Some(("other".into(), String::new())));
                // Neither save may store the draft typed for `remote`.
                this.save_api_key("other".into(), cx);
                this.save_api_key("remote".into(), cx);
                for id in ["other", "remote"] {
                    assert_eq!(
                        this.result(id, ProviderControl::Key),
                        Some(&Err(format!("Type the API key for {id} first.")))
                    );
                }
                // Remove key drops the draft; the next form is empty.
                this.open_key_form("remote".into(), window, cx);
                type_key(this, "sk-for-remote", window, cx);
                this.remove_api_key("remote".into(), cx);
                assert_eq!(key_draft(this, cx), None);
                this.open_key_form("remote".into(), window, cx);
                assert_eq!(key_draft(this, cx), Some(("remote".into(), String::new())));
            })
            .unwrap();
        cx.run_until_parked();
        let log = args_log(dir.path());
        assert!(!log.contains("set-key"), "{log}");
        assert!(!dir.path().join("stdin.bin").exists());
        assert!(log.contains("providers delete-key remote"), "{log}");
    }

    #[gpui::test]
    fn reset_drafts_drops_the_key_and_the_consent_question(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = providers_state(cx, dir.path());
        let window = providers_window(cx, &state, ProviderScope::Llm);
        window
            .update(cx, |this, window, cx| {
                this.open_key_form("remote".into(), window, cx);
                type_key(this, "sk-left-behind", window, cx);
                this.request_consent(AGENT.into(), cx);
                this.reset_drafts(cx);
                assert_eq!(this.key_form_provider(), None);
                assert_eq!(this.pending_consent(), None);
                // Neither the dropped draft nor the dropped question can act.
                this.save_api_key("remote".into(), cx);
                this.accept_consent(AGENT.into(), cx);
                assert!(this.running("remote").is_empty() && this.running(AGENT).is_empty());
            })
            .unwrap();
        cx.run_until_parked();
        let log = args_log(dir.path());
        assert!(
            !log.contains("set-key") && !log.contains("providers enable"),
            "{log}"
        );
        assert!(!dir.path().join("stdin.bin").exists());
    }

    #[gpui::test]
    fn enable_needs_the_accepted_registry_text_then_reloads_config(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        stage_enabled_agent(dir.path());
        let state = providers_state(cx, dir.path());
        let window = providers_window(cx, &state, ProviderScope::Llm);
        window
            .update(cx, |this, _, cx| {
                // Summaries keep the display name and detection status, and
                // leave text-embedding providers to the models.
                let rows: Vec<_> = this
                    .rows()
                    .map(|r| (r.id.as_str(), r.name.as_str(), r.status.as_deref()))
                    .collect();
                assert_eq!(
                    rows,
                    [
                        (AGENT, "Synthetic Agent", Some("not_found")),
                        ("remote", "remote", None)
                    ]
                );
                this.accept_consent(AGENT.into(), cx);
                this.request_consent(AGENT.into(), cx);
                this.decline_consent(cx);
                this.accept_consent(AGENT.into(), cx);
                assert!(this.running(AGENT).is_empty());
            })
            .unwrap();
        cx.run_until_parked();
        assert!(!args_log(dir.path()).contains("providers enable"));
        assert!(!agent_consented(&state, cx), "off until accepted");
        window
            .update(cx, |this, _, cx| {
                this.request_consent(AGENT.into(), cx);
                this.accept_consent(AGENT.into(), cx);
                assert_eq!(this.running(AGENT), [ProviderOp::Enable]);
            })
            .unwrap();
        cx.run_until_parked();
        let log = args_log(dir.path());
        assert_eq!(
            log.matches(&format!("--json providers enable {AGENT} --accept\n"))
                .count(),
            1,
            "{log}"
        );
        assert!(
            agent_consented(&state, cx),
            "AppState took in the new config"
        );
        window
            .update(cx, |this, _, _| {
                assert_eq!(
                    this.result(AGENT, ProviderControl::Consent),
                    Some(&Ok("Enabled.".to_owned()))
                );
                assert!(this.running(AGENT).is_empty());
            })
            .unwrap();
    }

    #[gpui::test]
    fn command_finishing_after_the_view_is_gone_still_reloads_config(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        stage_enabled_agent(dir.path());
        let state = providers_state(cx, dir.path());
        let view = cx.new(|_| ProvidersView::new(state.clone(), ProviderScope::Llm));
        view.update(cx, |this, cx| this.reload(cx));
        cx.run_until_parked();
        view.update(cx, |this, cx| {
            this.request_consent(AGENT.into(), cx);
            this.accept_consent(AGENT.into(), cx);
        });
        let weak = view.downgrade();
        drop(view);
        // The view is released before the child runs.
        cx.update(|_| {});
        assert!(weak.upgrade().is_none());
        assert!(!args_log(dir.path()).contains("providers enable"));
        cx.run_until_parked();
        assert!(args_log(dir.path()).contains(&format!("providers enable {AGENT} --accept")));
        assert!(agent_consented(&state, cx));
    }
}
