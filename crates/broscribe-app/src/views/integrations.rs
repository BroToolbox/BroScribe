//! Settings and setup share this opt-in integrations surface. Merely opening
//! it reads status; only explicit buttons connect, select calendars, sign in
//! or sign out. On a Mac the recommended source is the calendars macOS already
//! syncs, read by BroScribe Agent. Signing in to a provider directly with
//! your own OAuth app is an advanced, collapsed alternative.
//!
//! Each provider runs at most one action at a time, independently of the
//! others, and its outcome is shown only on the card (provider or account)
//! that started it. Every browser sign-in runs under an attempt ID this view
//! chooses; Cancel, leaving the page or closing it cancels that attempt in
//! the CLI's durable attempt journal, so a sign-in that completes in the
//! browser afterwards saves nothing.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;

use broscribe_core::calendar::{AccountRef, CalendarTarget, GoogleCalendarStamp};
use broscribe_integrations::{AccountState, AccountStatus, IntegrationId, SystemCalendarStatus};
use broscribe_integrations::{
    CalendarListError, CalendarRouteRead, CalendarTargetOutcome, CalendarTargetRemoval, Capability,
    GoogleCalendarCatalog, LoginAttemptId, MicrosoftAccount, MicrosoftCalendarCatalog, PublicError,
    RouteCopy,
};
#[cfg(test)]
use broscribe_platform::HostPlatform;
use broscribe_platform::{CalendarAuthorization, CalendarProvider};
use broscribe_platform_native::NativePlatform;
use gpui::InteractiveElement as _;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::checkbox::Checkbox;
use gpui::component::input::{Input, InputState};
use gpui::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui::component::{ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, BackgroundExecutor, Context, Entity, FocusHandle, IntoElement,
    ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::jobs::{ChildKind, CliError, CliRunner, failure_text, sentence};
use crate::state::AppState;
use crate::ui::format::{plural, spoken_date_time};
use crate::ui::kit::{self, ConfirmSpec, StatusKind, confirm_line};
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::ui::{heading, text_line, title};
use crate::views::settings::row;

/// Native calendar sources, in the order their cards are shown.
const NATIVE_IDS: [IntegrationId; 1] = [IntegrationId::SystemCalendar];

/// What connecting system calendars does with calendar data, shown before Connect.
pub const NATIVE_DISCLOSURE: &str = "Uses the calendar accounts this Mac already syncs (System Settings › Internet Accounts); BroScribe never adds accounts or changes permissions for you. Connect asks macOS to give BroScribe Agent full calendar access, because macOS has no read-only option; BroScribe only reads. It reads only the calendars you select here. For those, the next 30 days of event titles and attendees are copied into BroScribe's private cache (readable only by your user account) and its journals, and can appear as meeting details and in MCP context. Audio, voice data and sign-in tokens are never part of it.";

/// Direct provider sign-in, which needs the user's own OAuth app registration.
fn direct_ids() -> impl Iterator<Item = IntegrationId> {
    IntegrationId::ALL
        .into_iter()
        .filter(|id| !id.is_native_calendar())
}

fn is_direct(target: Option<&str>) -> bool {
    target
        .and_then(|target| IntegrationId::parse(target.split('/').next().unwrap_or(target)).ok())
        .is_some_and(|id| !id.is_native_calendar())
}

/// Why the agent could not report a native calendar's status.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct HelperError {
    pub code: String,
    pub message: String,
}

impl HelperError {
    /// The agent's words as a sentence, never its code (WRT-04): read as
    /// [`crate::daemon::agent_error_text`] reads the agent's refusals, so a
    /// code whose message names a file is a fixed sentence and no path
    /// stays in the words.
    pub fn user_text(&self) -> String {
        crate::jobs::answered_cause(&self.code)
            .map_or_else(|| crate::jobs::refusal_text(&self.message), str::to_owned)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct IntegrationStatus {
    pub integration: IntegrationId,
    pub configured: bool,
    pub state: AccountState,
    #[serde(default)]
    pub admin_controlled_scopes: Vec<String>,
    /// System calendars only: the agent's report, or unknown permission.
    #[serde(default)]
    pub system_calendar: Option<SystemCalendarStatus>,
    /// Native calendars only: why the agent could not report.
    #[serde(default)]
    pub helper_error: Option<HelperError>,
    #[serde(default)]
    pub microsoft_accounts: Vec<MicrosoftAccount>,
    #[serde(default)]
    pub legacy_microsoft: Option<broscribe_integrations::LegacyMicrosoftStatus>,
    /// This integration's own settings did not parse: it reads as not configured,
    /// while the other integrations keep working.
    #[serde(default)]
    pub config_error: Option<String>,
    /// Google only: whether its OAuth client secret is stored; unknown otherwise.
    #[serde(default)]
    pub client_secret_stored: Option<bool>,
}

/// What a configured Google card offers for its stored OAuth client secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretControl {
    /// None is stored: sign-in cannot start until it is saved again.
    Save,
    /// Stored and signed out: it may be removed.
    Remove,
    /// Stored while signed in (or needing sign-in): sign out before removing it.
    SignOutFirst,
}

impl IntegrationStatus {
    pub fn label(&self) -> &'static str {
        if let Some(provider) = self.integration.calendar_provider() {
            return self.native_state().label(provider);
        }
        if !self.microsoft_accounts.is_empty() {
            return "Verified corporate accounts";
        }
        if !self.configured {
            "Not set up"
        } else {
            account_status_label(self.state.status)
        }
    }
    /// Google's client-secret control; `None` elsewhere, before configuration, or
    /// when the status could not say.
    pub fn secret_control(&self) -> Option<SecretControl> {
        if self.integration != IntegrationId::Google || !self.configured {
            return None;
        }
        Some(match (self.client_secret_stored?, self.state.status) {
            (false, _) => SecretControl::Save,
            (true, AccountStatus::SignedOut) => SecretControl::Remove,
            (true, _) => SecretControl::SignOutFirst,
        })
    }
    /// The agent's report for this integration's own provider.
    fn native(&self) -> Option<&SystemCalendarStatus> {
        match self.integration.calendar_provider()? {
            CalendarProvider::EventKit => self.system_calendar.as_ref(),
        }
    }
    /// Only what the agent reported; an unknown permission is never guessed.
    pub fn native_state(&self) -> NativeState {
        if let Some(error) = &self.helper_error {
            return match error.code.as_str() {
                "helper_required" => NativeState::AgentMissing,
                "helper_outdated" => NativeState::AgentOutdated,
                _ => NativeState::AgentError(error.user_text()),
            };
        }
        let Some(native) = self.native() else {
            return NativeState::AgentError(
                "BroScribe Agent didn't report calendar status.".into(),
            );
        };
        let Some(authorization) = native.authorization else {
            return NativeState::AgentMissing;
        };
        let active = self.state.status == AccountStatus::Active;
        match authorization {
            CalendarAuthorization::Unsupported => NativeState::Unsupported,
            CalendarAuthorization::Denied => NativeState::Denied,
            CalendarAuthorization::Restricted => NativeState::Restricted,
            CalendarAuthorization::WriteOnly => NativeState::WriteOnly,
            CalendarAuthorization::Granted if active => match &native.calendars_error {
                // A failed listing is never shown as a connected source with no calendars.
                Some(CalendarListError { code, message }) => NativeState::ListFailed {
                    code: code.clone(),
                    message: message.clone(),
                },
                None => NativeState::Connected,
            },
            CalendarAuthorization::Granted => NativeState::NotConnected { granted: true },
            CalendarAuthorization::NotDetermined if active => NativeState::Pending,
            CalendarAuthorization::NotDetermined => NativeState::NotConnected { granted: false },
        }
    }
    fn eligible_ids(&self) -> BTreeSet<&str> {
        self.native()
            .iter()
            .flat_map(|native| &native.calendars)
            .map(|calendar| calendar.id.as_str())
            .collect()
    }
    /// Selected calendars the source no longer offers: not read, and never replaced.
    pub fn lost_calendars(&self) -> usize {
        let eligible = self.eligible_ids();
        self.state
            .native_calendar_ids
            .iter()
            .filter(|id| !eligible.contains(id.as_str()))
            .count()
    }
}

/// What a native calendar card can truthfully say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeState {
    /// Not a Mac.
    Unsupported,
    /// No agent answered: macOS permission is unknown.
    AgentMissing,
    /// The running agent predates this calendar source.
    AgentOutdated,
    AgentError(String),
    Denied,
    Restricted,
    WriteOnly,
    /// Not opted in; `granted` when macOS already allows access.
    NotConnected {
        granted: bool,
    },
    /// Opted in, but macOS has not recorded an answer.
    Pending,
    /// Opted in with read access: calendars can be selected.
    Connected,
    /// Opted in with read access, but listing the calendars failed; nothing can be chosen.
    ListFailed {
        code: String,
        message: String,
    },
}
impl NativeState {
    pub fn label(&self, _provider: CalendarProvider) -> &'static str {
        match self {
            Self::Unsupported => "Unavailable: system calendars need macOS",
            Self::AgentMissing => "BroScribe Agent isn't running. Calendar access is unknown.",
            Self::AgentOutdated => "Update BroScribe Agent",
            Self::AgentError(_) => "Status unavailable",
            Self::Denied => "Calendar access denied in macOS",
            Self::Restricted => "Calendar access restricted on this Mac",
            Self::WriteOnly => "Add-only access: events cannot be read",
            Self::NotConnected { .. } => "Not connected",
            Self::Pending => "Waiting for macOS",
            Self::Connected => "Connected",
            Self::ListFailed { .. } => "Connected, but its calendars could not be listed",
        }
    }
    pub fn guidance(&self, provider: CalendarProvider) -> Option<String> {
        Some(match self {
            Self::Unsupported => "System calendars are read through macOS. On this computer, use a direct provider under Advanced.".into(),
            Self::AgentMissing => "BroScribe Agent reads system calendars and is not running, so calendar access is unknown. Start BroScribe Agent, then refresh.".into(),
            Self::AgentOutdated => "BroScribe Agent is older than this app and cannot read system calendars. Restart BroScribe after updating it.".into(),
            Self::AgentError(message) => message.clone(),
            Self::ListFailed { code, message } => native_error(
                provider,
                "Couldn't list the calendars.",
                code,
                &list_cause(&CalendarListError {
                    code: code.clone(),
                    message: message.clone(),
                }),
            ),
            Self::Denied => "Calendar access for BroScribe Agent is off. You can turn it on in System Settings › Privacy & Security › Calendars, then choose Connect.".into(),
            Self::Restricted => "Calendar access is restricted on this Mac (for example by device management). BroScribe cannot change that.".into(),
            Self::WriteOnly => "BroScribe Agent may only add events, not read them. Choose Full Access for it in System Settings › Privacy & Security › Calendars, then choose Connect.".into(),
            Self::NotConnected { granted: true } => "macOS already allows calendar access. Connect, then choose calendars; nothing is read before you select them.".into(),
            Self::NotConnected { granted: false } => "Connect asks macOS for calendar access. Nothing is read before you select calendars.".into(),
            Self::Pending => "macOS has not recorded your answer. Answer its calendar prompt, then choose Connect again.".into(),
            Self::Connected => return None,
        })
    }
    /// A successful catalog permits explicit selection, independently of account coverage.
    pub fn can_select(&self) -> bool {
        matches!(self, Self::Connected)
    }
    fn can_connect(&self) -> bool {
        matches!(self, Self::NotConnected { .. } | Self::Pending)
    }
}

/// A delegated calendar the user typed that can't be a route: empty, too
/// long or with control characters (`CalendarTarget::validate`).
const DELEGATED_INVALID: &str =
    "Couldn't select that calendar. The mailbox address or calendar ID isn't valid.";

/// `value` as a `--spec-stdin` body. A JSON value always serialises; if it
/// ever didn't, the user reads words, never serde's message.
fn spec_body(value: &Value) -> Result<Vec<u8>, String> {
    serde_json::to_vec(value).map_err(|error| {
        tracing::warn!(category = ?error.classify(), "writing a calendar request");
        "Couldn't prepare the request.".to_owned()
    })
}

/// Why a calendar list failed, in words without its code: read like the
/// CLI's own answer ([`CliError::user_text`]).
fn list_cause(failed: &CalendarListError) -> String {
    let CalendarListError { code, message } = failed;
    CliError::answered(code, message.clone()).user_text()
}

/// The guidance for a failed native-calendar action's stable error code;
/// for another code, "`what` `cause`" ("Couldn't list the calendars.
/// <Cause>."): the failure in words, never the code and never empty
/// (WRT-04).
pub fn native_error(provider: CalendarProvider, what: &str, code: &str, cause: &str) -> String {
    let state = match code {
        "calendar_denied" => NativeState::Denied,
        "calendar_restricted" => NativeState::Restricted,
        "calendar_write_only" => NativeState::WriteOnly,
        "calendar_pending" => NativeState::Pending,
        "helper_required" => NativeState::AgentMissing,
        "helper_outdated" => NativeState::AgentOutdated,
        "busy" => {
            return "A macOS permission prompt is already open. Answer it, then refresh.".into();
        }
        _ => return failure_text(what, cause, ""),
    };
    state.guidance(provider).unwrap_or_default()
}

/// The calendars the user is choosing for one native source. It starts from the saved
/// selection (empty until they choose) and belongs to one account generation: a
/// connect, disconnect or selection saved elsewhere starts it again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSelection {
    pub generation: u64,
    pub ids: BTreeSet<String>,
}

/// One explicit integration action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    SaveClient,
    /// Store the Google client secret again for an already configured client.
    SaveSecret,
    /// Remove the stored Google client secret (signed out only).
    RemoveSecret,
    SignIn,
    SignOut,
    /// Sign out, then ask Google to revoke the grant for the whole OAuth project.
    SignOutRevoke,
    Connect,
    Configure,
    /// Stop reading one selected Microsoft calendar route, keeping the others.
    RemoveRoute,
    Disconnect,
    Sync,
    Catalog,
    DiscardLegacy,
}
impl Action {
    fn native(self) -> bool {
        matches!(
            self,
            Self::Connect | Self::Configure | Self::Disconnect | Self::Sync
        )
    }
    /// "Couldn't <do X>." of a failed native-calendar action.
    fn native_failure(self) -> &'static str {
        match self {
            Self::Connect => "Couldn't connect the system calendars.",
            Self::Configure => "Couldn't save the calendar choice.",
            Self::Disconnect => "Couldn't disconnect the system calendars.",
            _ => "Couldn't read the system calendars.",
        }
    }
    fn notice(self) -> &'static str {
        match self {
            Self::SaveClient => "Integration settings updated.",
            Self::SaveSecret => "Client secret saved. Choose Sign in when you want to continue.",
            Self::RemoveSecret => {
                "Client secret removed. Sign-in needs it saved again; nothing else changed."
            }
            Self::SignIn => "Sign-in finished.",
            Self::SignOut => "Signed out.",
            Self::SignOutRevoke => "Signed out.",
            Self::Connect => {
                "Connected. Choose the calendars BroScribe may read; none are read until you do."
            }
            Self::Configure => "Calendar selection saved.",
            Self::RemoveRoute => {
                "Stopped reading that calendar. The other selected calendars and their cached events are kept."
            }
            Self::Disconnect => {
                "Disconnected. BroScribe stopped reading these calendars and cleared their calendar cache."
            }
            Self::Sync => "Calendars refreshed.",
            Self::Catalog => "Calendars found. Choose which calendars to read.",
            Self::DiscardLegacy => {
                "The old sign-in is already gone. Your accounts and library are unchanged."
            }
        }
    }
    /// The words of the wait while the action runs (a Pending status line).
    fn waiting(self) -> &'static str {
        match self {
            Self::Connect => {
                "Waiting for macOS… Answer its permission prompt; it closes after two minutes."
            }
            Self::SignIn => "Waiting for sign-in… Finish it in your browser.",
            Self::SaveClient => "Saving the calendar integration settings…",
            Self::SaveSecret => "Saving the calendar client secret…",
            Self::RemoveSecret => "Removing the calendar client secret…",
            Self::SignOut | Self::SignOutRevoke => "Signing out of the calendar account…",
            Self::Configure => "Saving the calendar selection…",
            Self::RemoveRoute => "Removing the selected calendar…",
            Self::Disconnect => "Disconnecting the system calendars…",
            Self::Sync => "Refreshing calendars…",
            Self::Catalog => "Looking for calendars…",
            Self::DiscardLegacy => "Removing the old calendar sign-in…",
        }
    }
}

/// The stable id of a named node on `id`'s card: `calendar-<integration>-<part>`.
fn node_id(id: IntegrationId, part: &str) -> SharedString {
    SharedString::from(format!("calendar-{}-{part}", id.as_str()))
}

/// The stable id of a named node on one account's card:
/// `calendar-<integration>-<account>-<part>`.
fn account_node_id(id: IntegrationId, account: &AccountRef, part: &str) -> SharedString {
    SharedString::from(format!(
        "calendar-{}-{}-{part}",
        id.as_str(),
        account.account_id
    ))
}

/// The stable id of a card's wait and outcome line:
/// `calendar-status-<integration>`, or `calendar-status-<integration>-<account>`
/// on an account's card.
pub fn card_status_id(card: &Card) -> String {
    match &card.account {
        None => format!("calendar-status-{}", card.integration.as_str()),
        Some(account) => format!(
            "calendar-status-{}-{}",
            card.integration.as_str(),
            account.account_id
        ),
    }
}

/// The page's Failed line when calendar status can't be read; `cause` is
/// [`CliError::user_text`]'s words (WRT-04).
pub fn status_error_text(cause: &str) -> String {
    failure_text("Couldn't read the calendar status.", cause, "")
}

/// The Failed line of an integration whose settings did not parse.
pub fn config_error_text(id: IntegrationId, error: &str) -> String {
    failure_text(
        &format!(
            "{} is off because its saved settings are invalid.",
            id.display_name()
        ),
        &sentence(error),
        "Other calendars keep working.",
    )
}

fn calendar_permission_words(scope: &str) -> &'static str {
    match scope {
        "Calendars.Read" => "Read your calendars",
        "Calendars.Read.Shared" => "Read calendars shared with you",
        "Group.Read.All" => "Find Microsoft 365 groups and their calendars",
        "User.Read" => "Read your account profile",
        "User.ReadBasic.All" => "Search your organisation's people and photos",
        "offline_access" => "Keep your account signed in",
        _ => "Other account permission",
    }
}

/// The card an action and its outcome belong to: a provider, or one of its accounts.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Card {
    pub integration: IntegrationId,
    pub account: Option<AccountRef>,
}
impl Card {
    pub fn provider(integration: IntegrationId) -> Self {
        Self {
            integration,
            account: None,
        }
    }
    pub fn account(integration: IntegrationId, account: AccountRef) -> Self {
        Self {
            integration,
            account: Some(account),
        }
    }
}

/// One running action. A provider runs at most one; providers never wait on each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub card: Card,
    pub action: Action,
    /// The browser sign-in this action runs as.
    pub attempt: Option<LoginAttemptId>,
    /// The cancellation of `attempt` was sent.
    pub cancelling: bool,
    /// Google selection: the catalog generations the selection was made under.
    pub expected_google: Option<GoogleCalendarStamp>,
    ticket: u64,
}
impl Operation {
    pub fn new(card: Card, action: Action) -> Self {
        Self {
            card,
            action,
            attempt: None,
            cancelling: false,
            expected_google: None,
            ticket: 0,
        }
    }
    pub fn signing_in(mut self, attempt: LoginAttemptId) -> Self {
        self.attempt = Some(attempt);
        self
    }
    pub fn expecting(mut self, google: GoogleCalendarStamp) -> Self {
        self.expected_google = Some(google);
        self
    }
}

/// The last outcome of an action, shown only on its card (the key it is
/// kept under).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardMessage {
    pub text: String,
    pub error: bool,
}

/// What the view does after an action's result was applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Followup {
    pub reload: bool,
    pub people_changed: bool,
}

pub(crate) fn account_status_label(status: AccountStatus) -> &'static str {
    match status {
        AccountStatus::SignedOut => "Signed out",
        AccountStatus::Active => "Signed in",
        AccountStatus::ReauthRequired => "Sign in required",
    }
}

fn microsoft_account_name(account: &MicrosoftAccount) -> Option<String> {
    let identity = &account.identity;
    let name = identity
        .display_name
        .as_ref()
        .or_else(|| identity.verified_addresses.first());
    match (&name, &identity.tenant_display_name) {
        (Some(name), Some(organisation)) => Some(format!("{name} · {organisation}")),
        (Some(name), None) => Some((*name).clone()),
        (None, Some(organisation)) => Some(organisation.clone()),
        (None, None) => None,
    }
}

/// The numbered label lets the existing People chooser distinguish accounts with identical names.
pub(crate) fn microsoft_account_label(account: &MicrosoftAccount, index: usize) -> String {
    let label = format!("Microsoft account {}", index + 1);
    match microsoft_account_name(account) {
        Some(name) => format!("{label} · {name}"),
        None => label,
    }
}

fn calendar_account_name(account: &MicrosoftAccount) -> String {
    microsoft_account_name(account).unwrap_or_else(|| "Microsoft account".into())
}

fn capability_label(capability: Capability) -> &'static str {
    match capability {
        Capability::Unknown => "unchecked",
        Capability::Available => "available",
        Capability::Denied => "access denied",
        Capability::NotRequested => "permission not requested",
        Capability::Error => "last check failed",
        Capability::Partial => "partial access",
    }
}

fn calendar_kind(target: &CalendarTarget) -> &'static str {
    match target {
        CalendarTarget::Own { .. } => "Personal calendar",
        CalendarTarget::RecipientShared { .. } => "Shared calendar",
        CalendarTarget::Delegated { .. } => "Delegated calendar",
        CalendarTarget::Group { .. } => "Group calendar",
    }
}

fn calendar_count_and_sync(count: usize, last: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let noun = plural(count as u64, "calendar", "calendars");
    let sync = last.map_or_else(
        || "Never synced".to_owned(),
        |at| format!("Last synced {}", spoken_date_time(&at.fixed_offset())),
    );
    format!("{count} {noun} selected · {sync}")
}

/// One selected route's last successful read and what its cached copy holds.
pub fn route_read_label(read: &CalendarRouteRead) -> String {
    let last = read.last_success.map_or_else(
        || "never read successfully".to_owned(),
        |at| format!("last read {}", spoken_date_time(&at.fixed_offset())),
    );
    let count = read.cached_events as u64;
    let events = plural(count, "event", "events");
    let copy = match read.copy {
        RouteCopy::Fresh => format!("{count} {events} from the last check"),
        RouteCopy::Retained => {
            format!("latest read failed. Kept {count} {events} from the last successful read")
        }
        RouteCopy::Empty => "nothing cached".to_owned(),
    };
    format!("{last} · {copy}")
}

#[derive(Default)]
pub struct IntegrationsModel {
    pub statuses: BTreeMap<IntegrationId, IntegrationStatus>,
    /// Running actions by provider.
    pub operations: BTreeMap<IntegrationId, Operation>,
    /// The last outcome per card: an action on one account never clears another
    /// account's outcome, though the provider runs one action at a time.
    pub messages: BTreeMap<Card, CardMessage>,
    /// Reading status failed.
    pub status_error: Option<String>,
    status_loading: bool,
    pub instructions: BTreeMap<IntegrationId, String>,
    /// One draft per connected native source; they never share a selection.
    pub selection: BTreeMap<IntegrationId, NativeSelection>,
    /// Microsoft catalogs, each valid for its account generation; selection changes
    /// keep them.
    pub catalogs: BTreeMap<AccountRef, MicrosoftCalendarCatalog>,
    /// The Google catalog, valid for the account generation it names; a newer
    /// selection keeps it.
    pub google_catalog: Option<GoogleCalendarCatalog>,
    /// The exact unidentified provider slot and epoch the user is confirming.
    pub legacy_discard: Option<(IntegrationId, u64)>,
    /// The user is confirming a Google sign-out that also revokes at Google.
    pub google_revoke: bool,
    generation: u64,
    ticket: u64,
}
impl IntegrationsModel {
    pub fn prepare_legacy_discard(&mut self, id: IntegrationId) -> Result<(), String> {
        let status = self
            .statuses
            .get(&id)
            .ok_or("Refresh integration status first.")?;
        if id != IntegrationId::MicrosoftGraph
            || status.legacy_microsoft
                != Some(broscribe_integrations::LegacyMicrosoftStatus::Quarantined)
        {
            return Err("There's no old sign-in to remove.".into());
        }
        self.legacy_discard = Some((id, status.state.generation));
        Ok(())
    }

    pub fn cancel_legacy_discard(&mut self) {
        self.legacy_discard = None;
    }

    pub fn confirm_legacy_discard_args(
        &mut self,
        id: IntegrationId,
    ) -> Result<Vec<OsString>, String> {
        let Some((provider, generation)) = self.legacy_discard else {
            return Err("Confirm that you want to remove the old sign-in first.".into());
        };
        if provider != id {
            return Err("The confirmation belongs to another provider.".into());
        }
        self.legacy_discard = None;
        Ok(vec![
            "integrations".into(),
            "discard-legacy".into(),
            id.as_str().into(),
            "--generation".into(),
            generation.to_string().into(),
            "--yes".into(),
        ])
    }

    /// Opens the confirmation of a revoking Google sign-out (only while signed in).
    pub fn prepare_google_revoke(&mut self) -> Result<(), String> {
        let signed_in = self
            .statuses
            .get(&IntegrationId::Google)
            .ok_or("Refresh integration status first.")?
            .state
            .status
            != AccountStatus::SignedOut;
        if !signed_in {
            return Err("Google is already signed out.".into());
        }
        self.google_revoke = true;
        Ok(())
    }

    pub fn cancel_google_revoke(&mut self) {
        self.google_revoke = false;
    }

    /// The confirmed revoking sign-out; refused unless the confirmation is open.
    pub fn confirm_google_revoke_args(&mut self) -> Result<Vec<OsString>, String> {
        if !std::mem::take(&mut self.google_revoke) {
            return Err("Confirm signing out and revoking at Google first.".into());
        }
        Ok(vec![
            "integrations".into(),
            "logout".into(),
            IntegrationId::Google.as_str().into(),
            "--revoke-at-google".into(),
        ])
    }

    /// Saves the Google client secret for the configured client; the secret goes
    /// only on stdin.
    pub fn save_secret_command(secret: &str) -> Result<(Vec<OsString>, Vec<u8>), String> {
        if secret.trim().is_empty() {
            return Err("Enter your registered Google OAuth client secret.".into());
        }
        Ok((
            vec![
                "integrations".into(),
                "setup".into(),
                IntegrationId::Google.as_str().into(),
                "--client-secret-stdin".into(),
            ],
            secret.as_bytes().to_vec(),
        ))
    }

    pub fn clear_secret_args() -> Vec<OsString> {
        vec![
            "integrations".into(),
            "setup".into(),
            IntegrationId::Google.as_str().into(),
            "--clear-client-secret".into(),
        ]
    }
    pub fn apply_statuses(&mut self, value: Value) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Response {
            integrations: Vec<IntegrationStatus>,
        }
        let response: Response = serde_json::from_value(value).map_err(|error| {
            tracing::warn!(category = ?error.classify(), "reading the integrations status");
            CliError::new("bad_output", "the calendar status couldn't be read").user_text()
        })?;
        self.statuses = response
            .integrations
            .into_iter()
            .map(|status| (status.integration, status))
            .collect();
        // A catalog lists what the account may read; a newer selection does not change
        // that, but a replaced or unavailable account does.
        self.catalogs.retain(|account, catalog| {
            self.statuses
                .values()
                .flat_map(|s| &s.microsoft_accounts)
                .any(|current| {
                    current.account == *account
                        && current.generation == catalog.generation
                        && current.access_error.is_none()
                })
        });
        // A removed account's outcome goes with its card, so it never reappears on
        // an account added again later.
        let statuses = &self.statuses;
        self.messages.retain(|card, _| {
            card.account.as_ref().is_none_or(|account| {
                statuses.get(&card.integration).is_some_and(|status| {
                    status
                        .microsoft_accounts
                        .iter()
                        .any(|current| current.account == *account)
                })
            })
        });
        // The Google catalog likewise lists what the account may read: it stays
        // while the account generation is current, and adopts a newer saved
        // selection (made elsewhere) instead of offering a stale one.
        let google = self
            .statuses
            .get(&IntegrationId::Google)
            .filter(|status| status.state.status == AccountStatus::Active);
        let current = match (self.google_catalog.as_mut(), google) {
            (Some(catalog), Some(status))
                if status.state.generation == catalog.authority.generation =>
            {
                if status.state.selection_generation > catalog.authority.selection_generation {
                    catalog.authority.selection_generation = status.state.selection_generation;
                    catalog.selected = status.state.native_calendar_ids.clone();
                }
                true
            }
            _ => false,
        };
        if !current {
            self.google_catalog = None;
        }
        self.refresh_selection();
        // Nothing is left to revoke once Google reads signed out.
        if self
            .statuses
            .get(&IntegrationId::Google)
            .is_none_or(|status| status.state.status == AccountStatus::SignedOut)
        {
            self.google_revoke = false;
        }
        Ok(())
    }
    /// Starts a status read; only the latest one's result is applied.
    pub fn begin_reload(&mut self) -> u64 {
        self.status_loading = true;
        self.status_error = None;
        self.generation += 1;
        self.generation
    }
    /// Applies a status read unless a newer one started; `false` when stale.
    pub fn finish_reload(&mut self, generation: u64, result: Result<Value, String>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.status_loading = false;
        self.status_error = match result {
            Ok(value) => self.apply_statuses(value).err(),
            Err(error) => Some(error),
        };
        true
    }
    /// The provider runs an action.
    pub fn busy(&self, id: IntegrationId) -> bool {
        self.operations.contains_key(&id)
    }
    /// Starts `operation` unless its provider already runs one; clears the last
    /// outcome of its own card only. Returns the ticket its result must present.
    pub fn begin(&mut self, mut operation: Operation) -> Option<u64> {
        let id = operation.card.integration;
        if self.busy(id) {
            return None;
        }
        self.ticket += 1;
        operation.ticket = self.ticket;
        self.messages.remove(&operation.card);
        self.operations.insert(id, operation);
        Some(self.ticket)
    }
    /// Ends the provider's action if it is still the one holding `ticket`.
    pub fn finish(&mut self, id: IntegrationId, ticket: u64) -> Option<Operation> {
        if self.operations.get(&id)?.ticket != ticket {
            return None;
        }
        self.operations.remove(&id)
    }
    /// The outcome shown on exactly this card.
    pub fn message(&self, card: &Card) -> Option<&CardMessage> {
        self.messages.get(card)
    }
    fn tell(&mut self, card: Card, text: String, error: bool) {
        self.messages.insert(card, CardMessage { text, error });
    }
    /// Marks the provider's running sign-in as cancelling and returns its
    /// `cancel-login` command and ticket; `None` without one, or when already sent.
    pub fn cancel_request(&mut self, id: IntegrationId) -> Option<(Vec<OsString>, u64)> {
        let operation = self.operations.get_mut(&id)?;
        let attempt = operation
            .attempt
            .as_ref()
            .filter(|_| !operation.cancelling)?;
        let args = Self::cancel_args(id, attempt);
        operation.cancelling = true;
        Some((args, operation.ticket))
    }
    /// Every running sign-in not yet cancelled (leaving the page).
    pub fn cancel_all_requests(&mut self) -> Vec<(IntegrationId, Vec<OsString>, u64)> {
        let ids: Vec<IntegrationId> = self.operations.keys().copied().collect();
        ids.into_iter()
            .filter_map(|id| {
                self.cancel_request(id)
                    .map(|(args, ticket)| (id, args, ticket))
            })
            .collect()
    }
    /// A failed cancellation is reported on the sign-in's own card, which may cancel
    /// again; a successful one is followed by the sign-in's own terminal result.
    pub fn apply_cancel_result(
        &mut self,
        id: IntegrationId,
        ticket: u64,
        result: Result<Value, CliError>,
    ) {
        let Err(error) = result else {
            return;
        };
        let Some(operation) = self
            .operations
            .get_mut(&id)
            .filter(|operation| operation.ticket == ticket)
        else {
            return;
        };
        operation.cancelling = false;
        let card = operation.card.clone();
        self.tell(
            card,
            failure_text("Couldn't cancel the sign-in.", &error.user_text(), ""),
            true,
        );
    }
    /// Applies a finished action's result to its own card and to what it changed.
    pub fn apply_result(
        &mut self,
        operation: &Operation,
        result: Result<Value, CliError>,
    ) -> Followup {
        let id = operation.card.integration;
        match result {
            Ok(value) => {
                let mut text = match self.apply_success(operation, &value) {
                    // The route is removed and the selection saved; only the other
                    // routes' cached events were lost until their next read.
                    Some(PublicError { message, .. }) => failure_text(
                        "Stopped reading that calendar. The other selected calendars stay selected, but their cached events couldn't be kept.",
                        &sentence(&message),
                        "They are read again at the next sync.",
                    ),
                    None => Self::notice_for(operation.action, &value),
                };
                if value["agent_notified"] == json!(false) {
                    text.push_str(" BroScribe Agent was not notified; it uses this change when it next starts.");
                }
                let failed = Self::revocation_failed(operation.action, &value);
                self.tell(operation.card.clone(), text, failed);
                Followup {
                    reload: true,
                    // Sign-out removes cached directory photos and sign-in changes
                    // the account: the people gallery reloads them.
                    people_changed: !id.is_native_calendar(),
                }
            }
            Err(error) if operation.action.native() && id.is_native_calendar() => {
                let provider = id.calendar_provider().unwrap_or(CalendarProvider::EventKit);
                self.tell(
                    operation.card.clone(),
                    native_error(
                        provider,
                        operation.action.native_failure(),
                        &error.code,
                        &error.user_text(),
                    ),
                    true,
                );
                // A refusal can change what macOS reports (denied, pending…).
                Followup {
                    reload: true,
                    people_changed: false,
                }
            }
            Err(error) => {
                let text = match error.code.as_str() {
                    "login_cancelled" => "Sign-in cancelled. Nothing from it was saved; the existing account, its calendars and its data are unchanged.".to_owned(),
                    _ => error.user_text(),
                };
                self.tell(operation.card.clone(), text, true);
                Followup {
                    // Refresh only local status, never retry the denied authentication.
                    reload: error.code == "conditional_access_blocked",
                    people_changed: false,
                }
            }
        }
    }
    /// Applies what a successful action changed; a route removal's cache carry-over
    /// failure is returned for its notice.
    fn apply_success(&mut self, operation: &Operation, value: &Value) -> Option<PublicError> {
        let id = operation.card.integration;
        if let Some(instructions) = value.get("instructions").and_then(Value::as_str) {
            self.instructions.insert(id, instructions.to_owned());
        }
        match (operation.action, id) {
            (Action::Catalog, IntegrationId::Google) => {
                // The next status read keeps it only while its generation is current.
                if let Ok(catalog) = serde_json::from_value(value.clone()) {
                    self.google_catalog = Some(catalog);
                }
            }
            (Action::Catalog, _) => {
                if let Ok(catalog) =
                    serde_json::from_value::<MicrosoftCalendarCatalog>(value.clone())
                {
                    let current = self
                        .statuses
                        .values()
                        .flat_map(|s| &s.microsoft_accounts)
                        .any(|a| {
                            a.account == catalog.account && a.generation == catalog.generation
                        });
                    if current {
                        self.catalogs.insert(catalog.account.clone(), catalog);
                    }
                }
            }
            (Action::Configure, IntegrationId::Google) => {
                let Ok(status) = serde_json::from_value::<IntegrationStatus>(value.clone()) else {
                    return None;
                };
                // Only this selection advanced the selection generation: the catalog it
                // was made from still lists the same account's calendars.
                if let Some(catalog) = self.google_catalog.as_mut().filter(|catalog| {
                    Some(catalog.authority) == operation.expected_google
                        && status.state.status == AccountStatus::Active
                        && status.state.generation == catalog.authority.generation
                }) {
                    catalog.authority.selection_generation = status.state.selection_generation;
                    catalog.selected = status.state.native_calendar_ids.clone();
                }
                let status = IntegrationStatus {
                    config_error: self
                        .statuses
                        .get(&id)
                        .and_then(|old| old.config_error.clone()),
                    ..status
                };
                self.statuses.insert(id, status);
            }
            (Action::Configure, IntegrationId::MicrosoftGraph) => {
                if let Ok(account) = serde_json::from_value::<MicrosoftAccount>(value.clone()) {
                    self.update_account(account);
                }
            }
            (Action::RemoveRoute, IntegrationId::MicrosoftGraph) => {
                if let Ok(removal) = serde_json::from_value::<CalendarTargetRemoval>(value.clone())
                {
                    self.update_account(removal.account);
                    return removal.copy_error;
                }
            }
            _ => {}
        }
        None
    }
    /// A newer view of one account from an action's result: its next selection
    /// uses these generations, and its catalog stays usable.
    fn update_account(&mut self, account: MicrosoftAccount) {
        if let Some(catalog) = self
            .catalogs
            .get_mut(&account.account)
            .filter(|catalog| catalog.generation == account.generation)
        {
            catalog.selection_generation = account.selection_generation;
        }
        if let Some(current) = self
            .statuses
            .values_mut()
            .flat_map(|s| s.microsoft_accounts.iter_mut())
            .find(|current| {
                current.account == account.account
                    && (current.generation, current.selection_generation)
                        <= (account.generation, account.selection_generation)
            })
        {
            *current = account;
        }
    }
    fn refresh_selection(&mut self) {
        for source in NATIVE_IDS {
            let Some(status) = self
                .statuses
                .get(&source)
                .filter(|status| status.native_state().can_select())
            else {
                self.selection.remove(&source);
                continue;
            };
            let eligible = status.eligible_ids();
            let generation = status.state.generation;
            match self.selection.get_mut(&source) {
                // The same account: keep the user's unsaved choices the source still offers.
                Some(draft) if draft.generation == generation => {
                    draft.ids.retain(|id| eligible.contains(id.as_str()))
                }
                _ => {
                    let ids = status
                        .state
                        .native_calendar_ids
                        .iter()
                        .filter(|id| eligible.contains(id.as_str()))
                        .cloned()
                        .collect();
                    self.selection
                        .insert(source, NativeSelection { generation, ids });
                }
            }
        }
    }
    /// A click from a calendar list of another account generation, or on a
    /// calendar the source no longer offers, changes nothing.
    pub fn toggle_calendar(
        &mut self,
        source: IntegrationId,
        generation: u64,
        id: &str,
        selected: bool,
    ) -> bool {
        let eligible = self
            .statuses
            .get(&source)
            .is_some_and(|status| status.eligible_ids().contains(id));
        let Some(draft) = self
            .selection
            .get_mut(&source)
            .filter(|draft| draft.generation == generation)
        else {
            return false;
        };
        if selected {
            eligible && draft.ids.insert(id.to_owned())
        } else {
            draft.ids.remove(id)
        }
    }
    /// The source's draft differs from its saved selection.
    pub fn selection_changed(&self, source: IntegrationId) -> bool {
        let saved = self
            .statuses
            .get(&source)
            .map(|status| {
                status
                    .state
                    .native_calendar_ids
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        self.selection.get(&source).is_some_and(|draft| {
            draft
                .ids
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>()
                != saved
        })
    }
    /// Exactly the checked calendars of one source, never all of them, as the
    /// `integrations setup <source> --spec-stdin` child: the calendar IDs (which can
    /// be email addresses) travel on stdin, never in argv. An empty list
    /// explicitly stops reading every calendar.
    pub fn native_selection(
        &self,
        source: IntegrationId,
    ) -> Result<(Vec<OsString>, Vec<u8>), String> {
        let draft = self
            .selection
            .get(&source)
            .ok_or("Connect this calendar source before choosing calendars.")?;
        let args = vec![
            "integrations".into(),
            "setup".into(),
            source.as_str().into(),
            "--spec-stdin".into(),
        ];
        let stdin = spec_body(&json!({ "calendar_ids": draft.ids }))?;
        Ok((args, stdin))
    }
    /// A browser sign-in (Google) or new corporate account (Microsoft) under
    /// `attempt`. Native calendars connect through macOS permission instead.
    pub fn login_args(id: IntegrationId, attempt: &LoginAttemptId) -> Vec<OsString> {
        let command = if id == IntegrationId::MicrosoftGraph {
            "add-account"
        } else {
            "login"
        };
        vec![
            "integrations".into(),
            command.into(),
            id.as_str().into(),
            "--attempt".into(),
            attempt.as_str().into(),
        ]
    }
    /// Connecting system calendars asks macOS through the agent: no sign-in attempt.
    pub fn connect_args(id: IntegrationId) -> Vec<OsString> {
        vec!["integrations".into(), "login".into(), id.as_str().into()]
    }
    pub fn cancel_args(id: IntegrationId, attempt: &LoginAttemptId) -> Vec<OsString> {
        vec![
            "integrations".into(),
            "cancel-login".into(),
            id.as_str().into(),
            "--attempt".into(),
            attempt.as_str().into(),
        ]
    }
    pub fn account_args(
        id: IntegrationId,
        account: &MicrosoftAccount,
        command: &str,
        fenced: bool,
    ) -> Vec<OsString> {
        let mut args = vec![
            "integrations".into(),
            command.into(),
            id.as_str().into(),
            "--account".into(),
            account.account.account_id.as_str().into(),
        ];
        if fenced {
            args.extend(["--generation".into(), account.generation.to_string().into()]);
        }
        args
    }
    /// Replaces the account's whole selection under its current generations;
    /// the targets travel on stdin, never in argv.
    pub fn remote_selection(
        id: IntegrationId,
        account: &MicrosoftAccount,
        targets: &[CalendarTarget],
    ) -> Result<(Vec<OsString>, Vec<u8>), String> {
        let mut args = Self::account_args(id, account, "select-calendars", true);
        args.extend([
            "--selection-generation".into(),
            account.selection_generation.to_string().into(),
            "--spec-stdin".into(),
        ]);
        let stdin = spec_body(&json!({ "targets": targets }))?;
        Ok((args, stdin))
    }
    /// Stops reading one route only; the other routes and their caches stay.
    pub fn remote_removal(
        id: IntegrationId,
        account: &MicrosoftAccount,
        target: &CalendarTarget,
    ) -> Result<(Vec<OsString>, Vec<u8>), String> {
        let mut args = Self::account_args(id, account, "remove-calendar", true);
        args.extend([
            "--selection-generation".into(),
            account.selection_generation.to_string().into(),
            "--spec-stdin".into(),
        ]);
        let stdin = spec_body(&json!({ "target": target }))?;
        Ok((args, stdin))
    }
    /// The Google catalog's selection with `calendar` checked or not, under both of the
    /// catalog's generations; never "primary" or "all" by default. A saved calendar the
    /// catalog no longer lists (deleted, unshared or unsubscribed) has no checkbox and
    /// cannot be chosen again, so any change leaves it out.
    pub fn google_selection(
        &self,
        calendar: &str,
        checked: bool,
    ) -> Result<(Vec<OsString>, Vec<u8>, GoogleCalendarStamp), String> {
        let catalog = self
            .google_catalog
            .as_ref()
            .ok_or("Find your Google calendars first.")?;
        let listed = |id: &str| catalog.calendars.iter().any(|c| c.id == id);
        if !listed(calendar) {
            return Err("That calendar isn't in the list of calendars.".into());
        }
        let mut ids: Vec<&str> = catalog
            .selected
            .iter()
            .map(String::as_str)
            .filter(|id| *id != calendar && listed(id))
            .collect();
        if checked {
            ids.push(calendar);
        }
        let authority = catalog.authority;
        let args = vec![
            "integrations".into(),
            "select-calendars".into(),
            IntegrationId::Google.as_str().into(),
            "--generation".into(),
            authority.generation.to_string().into(),
            "--selection-generation".into(),
            authority.selection_generation.to_string().into(),
            "--spec-stdin".into(),
        ];
        let stdin = spec_body(&json!({ "calendar_ids": ids }))?;
        Ok((args, stdin, authority))
    }
    pub fn setup_command(
        id: IntegrationId,
        client_id: &str,
        client_secret: &str,
    ) -> Result<(Vec<OsString>, Option<Vec<u8>>), String> {
        if id.is_native_calendar() {
            return Err("Native calendars need no OAuth client.".into());
        }
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return Err("Enter your registered OAuth client ID.".into());
        }
        let mut args = vec![
            "integrations".into(),
            "setup".into(),
            id.as_str().into(),
            "--client-id".into(),
            client_id.into(),
        ];
        let stdin = if id == IntegrationId::Google {
            if client_secret.trim().is_empty() {
                return Err("Enter your registered Google OAuth client secret.".into());
            }
            args.push("--client-secret-stdin".into());
            Some(client_secret.as_bytes().to_vec())
        } else {
            None
        };
        Ok((args, stdin))
    }
    /// What a finished action tells the user; a changed client ID says the old sign-in is
    /// gone, and a revoking sign-out says what Google was asked and answered.
    pub fn notice_for(action: Action, result: &Value) -> String {
        if result["client_change"]["sign_in_retired"].as_bool() == Some(true) {
            return "Client ID changed: the previous sign-in and its cached calendar were removed. Choose Sign in when you want to continue.".into();
        }
        if action == Action::SignOutRevoke {
            let revocation = &result["remote_revocation"];
            return match revocation["outcome"].as_str() {
                Some("revocation_requested") => "Signed out. Google accepted the revocation request for your OAuth project; it may take a while to take full effect.".into(),
                Some("no_stored_grant") => format!("Signed out. No stored Google sign-in was found, so nothing was sent to Google. To remove an older grant, remove your OAuth app (the name on your consent screen) at {GOOGLE_PERMISSIONS}."),
                _ => format!(
                    "Signed out on this computer, but revoking at Google failed ({}), so access may still be granted. Remove your OAuth app (the name on your consent screen) at {GOOGLE_PERMISSIONS}.",
                    revocation["code"].as_str().unwrap_or("unknown")
                ),
            };
        }
        if action == Action::RemoveSecret && result["client_secret_removed"] == json!(false) {
            return "No Google client secret was stored; nothing changed.".into();
        }
        action.notice().into()
    }
    /// A revoking sign-out that signed out but could not revoke is shown as a failure.
    fn revocation_failed(action: Action, result: &Value) -> bool {
        action == Action::SignOutRevoke
            && !matches!(
                result["remote_revocation"]["outcome"].as_str(),
                Some("revocation_requested" | "no_stored_grant")
            )
    }
}

/// Where a user removes a Google grant themselves.
const GOOGLE_PERMISSIONS: &str = "https://myaccount.google.com/permissions";

fn secret_input(window: &mut Window, cx: &mut Context<IntegrationsView>) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("Your Google OAuth client secret")
            .masked(true)
    })
}

#[cfg(test)]
pub(crate) fn calendar_settings_hint() -> &'static str {
    calendar_settings_hint_for(NativePlatform::host_platform())
}

#[cfg(test)]
fn calendar_settings_hint_for(host: HostPlatform) -> &'static str {
    match host {
        HostPlatform::MacOs => {
            "Use the calendars macOS already syncs. Direct provider sign-in with your own OAuth app is under Advanced."
        }
        HostPlatform::Windows | HostPlatform::Linux => {
            "Connect a Microsoft 365 or Google calendar under Advanced."
        }
    }
}

#[derive(Clone, Copy)]
enum AccountAction {
    Reconnect,
    Grant(&'static str),
    Delegate,
    Clear,
    Remove,
}

pub struct IntegrationsView {
    native_supported: bool,
    state: Entity<AppState>,
    pub model: IntegrationsModel,
    /// Direct provider forms are collapsed by default.
    advanced: bool,
    native_disclosure: Option<bool>,
    details: BTreeSet<AccountRef>,
    instructions_open: BTreeSet<IntegrationId>,
    action_focus: BTreeMap<SharedString, FocusHandle>,
    target_needs_focus: bool,
    target_anchor: Option<gpui::ScrollAnchor>,
    /// Credential drafts exist only while the direct forms are open.
    clients: BTreeMap<IntegrationId, Entity<InputState>>,
    google_secret: Option<Entity<InputState>>,
    target: Option<String>,
    in_setup: bool,
    /// Cancels running sign-ins when the view goes away.
    cli: CliRunner,
    executor: BackgroundExecutor,
}
impl IntegrationsView {
    pub fn new(state: Entity<AppState>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_native_support(state, NativePlatform::system_calendar().is_supported(), cx)
    }

    pub(crate) fn in_setup(mut self) -> Self {
        self.in_setup = true;
        self
    }

    fn with_native_support(
        state: Entity<AppState>,
        native_supported: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let cli = state.read(cx).cli().clone();
        let mut this = Self {
            native_supported,
            state,
            model: IntegrationsModel::default(),
            advanced: false,
            native_disclosure: None,
            details: BTreeSet::new(),
            instructions_open: BTreeSet::new(),
            action_focus: BTreeMap::new(),
            target_needs_focus: false,
            target_anchor: None,
            clients: BTreeMap::new(),
            google_secret: None,
            target: None,
            in_setup: false,
            cli,
            executor: cx.background_executor().clone(),
        };
        this.reload(cx);
        this
    }
    fn close_advanced(&mut self) {
        self.advanced = false;
        self.drop_drafts();
    }
    /// Typed credentials and pending confirmations never outlive the page being shown.
    fn drop_drafts(&mut self) {
        self.clients.clear();
        self.instructions_open.clear();
        self.details.clear();
        self.google_secret = None;
        self.model.cancel_legacy_discard();
        self.model.cancel_google_revoke();
    }
    /// Direct-provider inputs, created only while their forms are shown.
    fn direct_open(&self) -> bool {
        !self.native_supported || self.advanced
    }

    fn ensure_direct_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.action_focus
            .entry("native-disconnect".into())
            .or_insert_with(|| cx.focus_handle());
        if !self.direct_open() {
            return;
        }
        for id in direct_ids() {
            self.clients.entry(id).or_insert_with(|| {
                cx.new(|cx| InputState::new(window, cx).placeholder("Your OAuth client ID"))
            });
        }
        for key in [
            "integration-revoke-open-google",
            "discard-legacy-open-microsoft-graph",
            "integration-login-google",
            "integration-login-microsoft-graph",
        ] {
            self.action_focus
                .entry(key.into())
                .or_insert_with(|| cx.focus_handle());
        }
        for account in self
            .model
            .statuses
            .values()
            .flat_map(|status| &status.microsoft_accounts)
        {
            for part in ["choose", "more"] {
                self.action_focus
                    .entry(account_node_id(
                        IntegrationId::MicrosoftGraph,
                        &account.account,
                        part,
                    ))
                    .or_insert_with(|| cx.focus_handle());
            }
        }
        if self.google_secret.is_none() {
            self.google_secret = Some(secret_input(window, cx));
        }
    }
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let generation = self.model.begin_reload();
        cx.notify();
        let task = self.state.read(cx).run_cli(
            vec!["integrations".into(), "status".into()],
            ChildKind::Command,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(|error| error.user_text());
            this.update(cx, |this, cx| {
                if this.model.finish_reload(generation, result) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
    fn run(
        &mut self,
        operation: Operation,
        args: Vec<OsString>,
        stdin: Option<Vec<u8>>,
        cx: &mut Context<Self>,
    ) {
        let id = operation.card.integration;
        let Some(ticket) = self.model.begin(operation) else {
            return;
        };
        let task = if let Some(stdin) = stdin {
            self.state.read(cx).cli().run_with_stdin(
                args,
                ChildKind::Command,
                stdin,
                cx.background_executor(),
            )
        } else {
            self.state.read(cx).run_cli(args, ChildKind::Command, cx)
        };
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                // Only the action that still holds its provider's ticket reports.
                let Some(operation) = this.model.finish(id, ticket) else {
                    return;
                };
                let followup = this.model.apply_result(&operation, result);
                if followup.reload {
                    this.reload(cx);
                }
                if followup.people_changed {
                    this.state.update(cx, |state, cx| {
                        state.library_changed(
                            &crate::watch::Changes {
                                people: true,
                                ..Default::default()
                            },
                            cx,
                        );
                    });
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
    /// A browser sign-in of `card` under a new attempt ID the view can cancel.
    fn sign_in(
        &mut self,
        card: Card,
        build: impl FnOnce(&LoginAttemptId) -> Vec<OsString>,
        cx: &mut Context<Self>,
    ) {
        let attempt = LoginAttemptId::generate();
        let args = build(&attempt);
        self.run(
            Operation::new(card, Action::SignIn).signing_in(attempt),
            args,
            None,
            cx,
        );
    }
    /// Cancels the provider's running sign-in in the CLI's attempt journal.
    fn cancel_sign_in(&mut self, id: IntegrationId, cx: &mut Context<Self>) {
        if let Some((args, ticket)) = self.model.cancel_request(id) {
            self.send_cancel(id, args, ticket, cx);
        }
    }
    fn send_cancel(
        &mut self,
        id: IntegrationId,
        args: Vec<OsString>,
        ticket: u64,
        cx: &mut Context<Self>,
    ) {
        let task = self.state.read(cx).run_cli(args, ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.model.apply_cancel_result(id, ticket, result);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
    fn cancel_all_sign_ins(&mut self, cx: &mut Context<Self>) {
        for (id, args, ticket) in self.model.cancel_all_requests() {
            self.send_cancel(id, args, ticket, cx);
        }
    }
    /// The page stops being shown while its owner keeps it (the setup assistant leaving
    /// its Calendars step or being hidden): every running browser sign-in is cancelled in
    /// the CLI's attempt journal, once, so none completes unseen; the sign-in's own
    /// terminal result still reports on its card, which stays expanded. Typed credential
    /// drafts are dropped. Nothing is needed to show the page again.
    pub fn deactivate(&mut self, cx: &mut Context<Self>) {
        self.cancel_all_sign_ins(cx);
        self.drop_drafts();
        cx.notify();
    }
    fn instructions(&mut self, id: IntegrationId, cx: &mut Context<Self>) {
        let task = self.state.read(cx).run_cli(
            vec!["integrations".into(), "setup".into(), id.as_str().into()],
            ChildKind::Command,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(value) => {
                        if let Some(instructions) =
                            value.get("instructions").and_then(Value::as_str)
                        {
                            this.model.instructions.insert(id, instructions.to_owned());
                        }
                    }
                    Err(error) => this.model.tell(
                        Card::provider(id),
                        failure_text(
                            "Couldn't load the setup instructions.",
                            &error.user_text(),
                            "",
                        ),
                        true,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
    /// What `card` is doing and how its last action ended, with Cancel for a sign-in:
    /// a Pending line while it runs, then Done or Failed (an alert), in words and with
    /// a role, never colour alone (SET-06).
    fn card_state(&self, card: &Card, cx: &Context<Self>) -> gpui::Div {
        let status_id = card_status_id(card);
        let mut row = div().flex().flex_col().gap_1();
        let mut waiting = false;
        if let Some(operation) = self
            .model
            .operations
            .get(&card.integration)
            .filter(|operation| operation.card == *card)
        {
            waiting = true;
            row = row.child(kit::status_line(
                SharedString::from(status_id.clone()),
                StatusKind::Pending,
                operation.action.waiting(),
                cx,
            ));
            if operation.attempt.is_some() {
                let id = card.integration;
                row = row.child(
                    Button::new(SharedString::from(format!("cancel-sign-in-{id}")))
                        .debug_selector(move || format!("cancel-sign-in-{id}"))
                        .label(if operation.cancelling {
                            "Cancelling sign-in…"
                        } else {
                            "Cancel sign-in"
                        })
                        .map(|button| {
                            crate::ui::kit::disable_with_reason(
                                button,
                                operation.cancelling,
                                "This sign-in is already cancelling.",
                                cx,
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.cancel_sign_in(id, cx))),
                );
            }
        }
        if let Some(message) = self.model.message(card) {
            // A failed cancel is told while its sign-in still waits, beside the wait.
            let id = if waiting {
                format!("{status_id}-message")
            } else {
                status_id
            };
            row = row.child(kit::status_line(
                SharedString::from(id),
                if message.error {
                    StatusKind::Failed
                } else {
                    StatusKind::Done
                },
                message.text.clone(),
                cx,
            ));
        }
        row
    }
    fn open_confirmation(
        &mut self,
        spec: ConfirmSpec,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
        confirm: impl Fn(&mut Self, &mut Context<Self>) + Clone + 'static,
    ) {
        let view = cx.weak_entity();
        let cancel = view.clone();
        kit::confirm_dialog(
            opener,
            window,
            cx,
            move |_| Some(spec.clone()),
            move |_, cx| {
                let _ = cancel.update(cx, |this, cx| {
                    this.model.cancel_legacy_discard();
                    this.model.cancel_google_revoke();
                    cx.notify();
                });
                true
            },
            move |_, cx| {
                let _ = view.update(cx, |this, cx| confirm(this, cx));
                true
            },
        );
    }

    fn disconnect_native(
        &mut self,
        source: IntegrationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let mut spec = ConfirmSpec::new(
            confirm_line(
                "native-disconnect-title",
                "Disconnect calendars on this Mac?",
            ),
            "native-disconnect-dialog",
            "native-disconnect-actions",
            "native-disconnect-cancel",
            confirm_line("native-disconnect-confirm", "Disconnect calendars"),
        );
        spec.irreversible = false;
        spec.goes.push(confirm_line(
            "native-disconnect-goes",
            "BroScribe stops reading these calendars and clears its cached events.",
        ));
        spec.stays.push(confirm_line(
            "native-disconnect-stays",
            "Meetings, people and recordings stay.",
        ));
        self.open_confirmation(
            spec,
            self.action_focus["native-disconnect"].clone(),
            window,
            cx,
            move |this, cx| {
                this.run(
                    Operation::new(Card::provider(source), Action::Disconnect),
                    ["integrations", "logout", source.as_str()]
                        .map(OsString::from)
                        .into(),
                    None,
                    cx,
                )
            },
        );
    }

    fn remove_account_confirmation(
        &mut self,
        id: IntegrationId,
        account: &MicrosoftAccount,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let name = account.identity.display_name.as_deref().or_else(|| {
            account
                .identity
                .verified_addresses
                .first()
                .map(String::as_str)
        });
        let title = name.map_or_else(
            || "Remove this Microsoft account?".into(),
            |name| format!("Remove the Microsoft account for {name}?"),
        );
        let mut spec = ConfirmSpec::new(
            confirm_line("calendar-remove-account-title", title),
            "calendar-remove-account-dialog",
            "calendar-remove-account-actions",
            "calendar-remove-account-cancel",
            confirm_line("calendar-remove-account-confirm", "Remove account"),
        );
        spec.goes.push(confirm_line("calendar-remove-account-goes",
            "BroScribe signs out, forgets the calendars you chose and deletes its cached events for this account."));
        spec.stays.push(confirm_line(
            "calendar-remove-account-stays",
            "Meetings, people and recordings stay.",
        ));
        let args = IntegrationsModel::account_args(id, account, "remove-account", true);
        let card = Card::account(id, account.account.clone());
        self.open_confirmation(spec, opener, window, cx, move |this, cx| {
            this.run(
                Operation::new(card.clone(), Action::SignOut),
                args.clone(),
                None,
                cx,
            );
        });
    }

    fn native_button(
        &self,
        source: IntegrationId,
        label: &'static str,
        command: &'static str,
        action: Action,
        enabled: bool,
        cx: &Context<Self>,
    ) -> Button {
        Button::new(SharedString::from(format!("{source}-{command}")))
            .label(label)
            .debug_selector(move || format!("{source}-{command}"))
            .disabled(self.model.busy(source) || !enabled)
            .on_click(cx.listener(move |this, _, _, cx| {
                let args = if action == Action::Connect {
                    IntegrationsModel::connect_args(source)
                } else {
                    ["integrations", command, source.as_str()]
                        .map(OsString::from)
                        .into()
                };
                this.run(
                    Operation::new(Card::provider(source), action),
                    args,
                    None,
                    cx,
                )
            }))
    }
    /// The system-calendar card never offers an OAuth client form.
    fn render_native(
        &self,
        source: IntegrationId,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let Some(provider) = source.calendar_provider() else {
            return div().id("calendar-unsupported");
        };
        let busy = self.model.busy(source);
        let status = self.model.statuses.get(&source);
        let state = status.map(IntegrationStatus::native_state);
        let opted_in = status.is_some_and(|status| status.state.status == AccountStatus::Active);
        let disclosure_open = !opted_in || self.native_disclosure.unwrap_or(false);
        let mut card = div()
            .id(node_id(source, "card"))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_1()
            .border_color(cx.theme().border)
            .anchor_scroll(self.anchor(source.as_str()))
            .rounded(cx.theme().radius_lg)
            .child(heading(node_id(source, "heading"), "Calendars on this Mac"))
            .child(text_line(node_id(source, "summary"), Role::Label,
                "Reads the calendars you choose from the accounts this Mac already syncs. Nothing is read until you choose calendars."))
            .child(kit::disclosure(node_id(source, "reading-details"), "What BroScribe reads and stores",
                disclosure_open, cx, move |this, _, cx| {
                    this.native_disclosure = Some(!disclosure_open);
                    cx.notify();
                }).disabled(!opted_in))
            .when(disclosure_open, |card| card.child(text_line(
                node_id(source, "disclosure"), Role::Label, NATIVE_DISCLOSURE,
            )))
            .child(text_line(
                node_id(source, "state"),
                Role::Status,
                state
                    .as_ref()
                    .map_or("Status unavailable", |state| state.label(provider)),
            ));
        if let Some(guidance) = state.as_ref().and_then(|state| state.guidance(provider)) {
            card = card.child(text_line(
                node_id(source, "guidance"),
                Role::Label,
                guidance,
            ));
        }
        if let (Some(status), Some(draft)) = (
            status.filter(|_| state.as_ref().is_some_and(NativeState::can_select)),
            self.model.selection.get(&source),
        ) {
            let calendars = status
                .native()
                .map_or(&[][..], |native| &native.calendars[..]);
            if calendars.is_empty() {
                card = card.child(text_line(
                    node_id(source, "no-calendars"),
                    Role::Label,
                    "macOS offers no calendars BroScribe can read (birthday and subscribed \
                     calendars are never offered).",
                ));
            } else {
                card = card.child(text_line(
                    node_id(source, "choose"),
                    Role::Label,
                    "Choose the work calendars BroScribe may read:",
                ));
            }
            for (ix, calendar) in calendars.iter().enumerate() {
                let id = calendar.id.clone();
                let generation = draft.generation;
                let checkbox_id = SharedString::from(format!("{source}-{ix}"));
                let checkbox = Checkbox::new(checkbox_id)
                    .debug_selector(|| format!("{source}-{ix}"))
                    .label(format!("{} · {}", calendar.title, calendar.source))
                    .checked(draft.ids.contains(&calendar.id))
                    .disabled(busy)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        if this
                            .model
                            .toggle_calendar(source, generation, &id, *checked)
                        {
                            cx.notify();
                        }
                    }));
                card = card.child(checkbox);
            }
            if status.state.native_calendar_ids.is_empty() {
                card = card.child(text_line(
                    node_id(source, "none-selected"),
                    Role::Label,
                    "No calendar events are read until you select calendars.",
                ));
            }
            let lost = status.lost_calendars();
            if lost > 0 {
                card = card.child(kit::status_line(
                    node_id(source, "lost"),
                    StatusKind::Attention,
                    format!(
                        "{lost} selected {} no longer available in macOS. Choose the calendars \
                         you want BroScribe to read.",
                        plural(lost as u64, "calendar is", "calendars are")
                    ),
                    cx,
                ));
            }
            card = card.child(
                Button::new(SharedString::from(format!("{source}-configure")))
                    .debug_selector(move || format!("{source}-configure"))
                    .label("Use selected calendars")
                    .disabled(busy || !self.model.selection_changed(source))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        match this.model.native_selection(source) {
                            Ok((args, stdin)) => this.run(
                                Operation::new(Card::provider(source), Action::Configure),
                                args,
                                Some(stdin),
                                cx,
                            ),
                            Err(error) => {
                                this.model.tell(Card::provider(source), error, true);
                                cx.notify();
                            }
                        }
                    })),
            );
        }
        let connected = state.as_ref().is_some_and(NativeState::can_select);
        let selected = status.is_some_and(|status| !status.state.native_calendar_ids.is_empty());
        card.child(
            row()
                .when(
                    state.as_ref().is_some_and(NativeState::can_connect),
                    |row| {
                        row.child(self.native_button(
                            source,
                            "Connect…",
                            "login",
                            Action::Connect,
                            true,
                            cx,
                        ))
                    },
                )
                .when(connected && selected, |row| {
                    row.child(self.native_button(
                        source,
                        "Sync now",
                        "sync",
                        Action::Sync,
                        true,
                        cx,
                    ))
                })
                // Revoking the OS grant doesn't erase the local opt-in; it can still be disconnected.
                .when(opted_in, |row| {
                    row.child(
                        Button::new("native-disconnect")
                            .label("Disconnect")
                            .outline()
                            .debug_selector(|| "native-disconnect".into())
                            .track_focus(&self.action_focus["native-disconnect"])
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.disconnect_native(source, window, cx)
                            })),
                    )
                }),
        )
        .child(self.card_state(&Card::provider(source), cx))
    }

    pub(crate) fn set_target_scroll(&mut self, scroll: gpui::ScrollHandle) {
        self.target_anchor = Some(gpui::ScrollAnchor::for_handle(scroll));
    }

    pub(crate) fn set_window_target(&mut self, provider: Option<String>, cx: &mut Context<Self>) {
        self.target_needs_focus = provider.is_some();
        if self.target != provider {
            self.close_advanced();
            self.advanced = is_direct(provider.as_deref());
            self.target = provider;
        }
        self.reload(cx);
        cx.notify();
    }

    fn anchor(&self, target: &str) -> Option<gpui::ScrollAnchor> {
        self.target_anchor
            .clone()
            .filter(|_| self.target.as_deref() == Some(target))
    }

    /// Adds `target` to the account's selection under its current generations.
    fn select_remote(
        &mut self,
        id: IntegrationId,
        account: &MicrosoftAccount,
        targets: Vec<CalendarTarget>,
        cx: &mut Context<Self>,
    ) {
        let card = Card::account(id, account.account.clone());
        match IntegrationsModel::remote_selection(id, account, &targets) {
            Ok((args, stdin)) => self.run(
                Operation::new(card, Action::Configure),
                args,
                Some(stdin),
                cx,
            ),
            Err(error) => {
                self.model.tell(card, error, true);
                cx.notify();
            }
        }
    }

    fn remove_remote(
        &mut self,
        id: IntegrationId,
        account: &MicrosoftAccount,
        target: &CalendarTarget,
        cx: &mut Context<Self>,
    ) {
        let card = Card::account(id, account.account.clone());
        match IntegrationsModel::remote_removal(id, account, target) {
            Ok((args, stdin)) => self.run(
                Operation::new(card, Action::RemoveRoute),
                args,
                Some(stdin),
                cx,
            ),
            Err(error) => {
                self.model.tell(card, error, true);
                cx.notify();
            }
        }
    }

    /// The account as last reported; clicks act on this view, never on an older copy.
    fn current_account(&self, account: &AccountRef) -> Option<MicrosoftAccount> {
        self.model
            .statuses
            .values()
            .flat_map(|s| &s.microsoft_accounts)
            .find(|current| current.account == *account)
            .cloned()
    }

    fn render_account(
        &self,
        id: IntegrationId,
        account: &MicrosoftAccount,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let busy = self.model.busy(id);
        let blocked = account.access_error.as_deref() == Some("conditional_access_blocked");
        let handle = account.account.clone();
        let node = |part: &str| account_node_id(id, &handle, part);
        let catalog = self
            .model
            .catalogs
            .get(&handle)
            .filter(|catalog| catalog.generation == account.generation);
        let target_name = |target: &CalendarTarget| {
            let position = account
                .selected_calendars
                .iter()
                .position(|selected| selected == target);
            catalog
                .and_then(|catalog| {
                    catalog
                        .calendars
                        .iter()
                        .find(|calendar| &calendar.target == target)
                })
                .and_then(|calendar| calendar.name.clone())
                .unwrap_or_else(|| match position {
                    Some(index) => format!("{} {}", calendar_kind(target), index + 1),
                    None => calendar_kind(target).to_owned(),
                })
        };
        let mut row = div()
            .id(node("card"))
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .anchor_scroll(self.anchor(&format!("{id}/{}", account.account.account_id)))
            .child(text_line(
                node("identity"),
                Role::Label,
                calendar_account_name(account),
            ))
            .child(text_line(
                node("routes"),
                Role::Label,
                format!(
                    "{} · {}",
                    account_status_label(account.status),
                    calendar_count_and_sync(account.selected_calendars.len(), account.last_sync)
                ),
            ));
        if account.status == AccountStatus::ReauthRequired
            || self.target.as_deref()
                == Some(format!("{}/{}", id.as_str(), account.account.account_id).as_str())
        {
            row = row.child(kit::status_line(
                node("notification"),
                StatusKind::Attention,
                "This account needs your attention.",
                cx,
            ));
        }
        if blocked {
            row = row.child(kit::status_line(
                node("access"),
                StatusKind::Failed,
                "Microsoft Conditional Access blocked this account. Ask your organisation's \
                 administrator to inspect sign-in logs. Device-registration status alone does \
                 not establish the cause. No automatic retry or alternative client will be \
                 attempted; reconnect only after administrator approval.",
                cx,
            ));
        } else if let Some(error) = &account.access_error {
            row = row.child(kit::status_line(
                node("access"),
                StatusKind::Failed,
                failure_text("Account unavailable.", &sentence(error), ""),
                cx,
            ));
        }
        for (index, result) in account.target_results.iter().enumerate() {
            let status = match &result.outcome {
                CalendarTargetOutcome::Available { event_count } => {
                    let noun = plural(*event_count as u64, "event", "events");
                    format!("{event_count} {noun} read")
                }
                CalendarTargetOutcome::Denied { .. } => "access denied".to_owned(),
                CalendarTargetOutcome::Error { .. } => "last check failed".to_owned(),
            };
            row = row.child(text_line(
                node(&format!("target-{index}")),
                Role::Label,
                format!("{} · {status}", target_name(&result.target)),
            ));
        }
        for (index, read) in account.route_reads.iter().enumerate() {
            row = row.child(text_line(
                node(&format!("read-{index}")),
                Role::Label,
                format!("{} · {}", target_name(&read.target), route_read_label(read)),
            ));
        }
        for (index, selected) in account.selected_calendars.iter().enumerate() {
            let target = selected.clone();
            let current = handle.clone();
            row = row.child(
                Button::new(SharedString::from(format!(
                    "remove-route-{}-{index}",
                    account.account.account_id
                )))
                .debug_selector(|| format!("remove-route-{}-{index}", account.account.account_id))
                .label("Stop reading")
                .accessibility_label(format!("Stop reading {}", target_name(selected)))
                .ghost()
                .map(|button| {
                    crate::ui::kit::disable_with_reason(
                        button,
                        busy,
                        "An action for this calendar service is still running.",
                        cx,
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(account) = this.current_account(&current) {
                        this.remove_remote(id, &account, &target, cx);
                    }
                })),
            );
        }
        let details_open = self.details.contains(&handle);
        let details_handle = handle.clone();
        let choose_focus = self.action_focus[&node("choose")].clone();
        let choose_handle = handle.clone();
        let sync_handle = handle.clone();
        row = row.child(
            crate::views::settings::row()
                .child(
                    Button::new(node("choose"))
                        .debug_selector(|| {
                            format!("choose-calendars-{}", account.account.account_id)
                        })
                        .label("Choose calendars…")
                        .outline()
                        .track_focus(&choose_focus)
                        .map(|button| {
                            kit::disable_with_reason(
                                button,
                                busy || blocked,
                                if blocked {
                                    "Ask your administrator to review this sign-in first."
                                } else {
                                    "A calendar action is still running."
                                },
                                cx,
                            )
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_catalog(id, &choose_handle, choose_focus.clone(), window, cx);
                        })),
                )
                .child(
                    Button::new(node("sync"))
                        .label("Sync now")
                        .outline()
                        .disabled(busy || blocked)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(account) = this.current_account(&sync_handle) {
                                let args = IntegrationsModel::account_args(
                                    id,
                                    &account,
                                    "sync-account",
                                    false,
                                );
                                this.run(
                                    Operation::new(
                                        Card::account(id, sync_handle.clone()),
                                        Action::Sync,
                                    ),
                                    args,
                                    None,
                                    cx,
                                );
                            }
                        })),
                )
                .child(self.account_menu(id, account, cx)),
        );
        row.child(kit::disclosure(
            node("details"),
            "Details",
            details_open,
            cx,
            move |this, _, cx| {
                if !this.details.remove(&details_handle) {
                    this.details.insert(details_handle.clone());
                }
                cx.notify();
            },
        ))
        .when(details_open, |row| {
            row.child(kit::fact_row_mono(
                node("account-id"),
                "Account ID",
                account.account.account_id.to_string(),
                cx,
            ))
            .child(kit::fact_row_mono(
                node("organisation-id"),
                "Organisation ID",
                account.identity.resource_tenant_id.clone(),
                cx,
            ))
            .children(
                account
                    .granted_scopes
                    .iter()
                    .enumerate()
                    .map(|(index, scope)| {
                        kit::fact_row_mono(
                            node(&format!("permission-{index}")),
                            calendar_permission_words(scope),
                            format!("({scope})"),
                            cx,
                        )
                    }),
            )
            .child(kit::fact_row(
                node("access-levels"),
                "Access",
                format!(
                    "Calendars {} · Work directory {}",
                    capability_label(account.calendar),
                    capability_label(account.directory)
                ),
                cx,
            ))
        })
        .child(self.card_state(&Card::account(id, handle.clone()), cx))
    }

    fn open_catalog(
        &mut self,
        id: IntegrationId,
        handle: &AccountRef,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.model.busy(id) || window.has_active_dialog(cx) {
            return;
        }
        let Some(account) = self.current_account(handle) else {
            return;
        };
        if account.access_error.as_deref() == Some("conditional_access_blocked") {
            return;
        }
        let args = IntegrationsModel::account_args(id, &account, "calendars", false);
        self.run(
            Operation::new(Card::account(id, handle.clone()), Action::Catalog),
            args,
            None,
            cx,
        );
        opener.focus(window, cx);
        let view = cx.weak_entity();
        let handle = handle.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            // BroScribe's dialogs stand on the popover ground, not the desk.
            let dialog = dialog.bg(cx.theme().popover);
            let body = view
                .update(cx, |this, cx| this.catalog_dialog(id, &handle, cx))
                .ok();
            dialog
                .width(window.rem_size() * 26.)
                .title(heading("calendar-chooser-title", "Choose calendars"))
                .children(body)
                .on_close({
                    let opener = opener.clone();
                    move |_, window, cx| opener.focus(window, cx)
                })
        });
    }

    fn catalog_dialog(
        &self,
        id: IntegrationId,
        handle: &AccountRef,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let scroll = div()
            .id("calendar-chooser-scroll")
            .max_h(gpui::rems(20.))
            .overflow_y_scroll();
        let Some(account) = self.current_account(handle) else {
            return scroll;
        };
        let busy = self.model.busy(id);
        let blocked = account.access_error.as_deref() == Some("conditional_access_blocked");
        let catalog = self
            .model
            .catalogs
            .get(handle)
            .filter(|catalog| catalog.generation == account.generation);
        let node = |part: &str| account_node_id(id, handle, part);
        let mut row = div().flex().flex_col().gap_2();
        if busy {
            row = row.child(kit::status_line(
                "calendar-chooser-pending",
                StatusKind::Pending,
                "Checking calendars…",
                cx,
            ));
        }
        if let Some(message) = self
            .model
            .message(&Card::account(id, handle.clone()))
            .filter(|message| message.error)
        {
            row = row.child(kit::status_line(
                "calendar-chooser-error",
                StatusKind::Failed,
                message.text.clone(),
                cx,
            ));
        }
        if let Some(catalog) = catalog {
            let count = catalog.calendars.len();
            row = row.child(text_line(
                node("catalog"),
                Role::Label,
                format!(
                    "{count} {} found · Group calendars {}",
                    plural(count as u64, "calendar", "calendars"),
                    capability_label(catalog.groups),
                ),
            ));
            if let Some(error) = &catalog.groups_error {
                row = row.child(kit::status_line(
                    node("groups-error"),
                    StatusKind::Failed,
                    failure_text("Couldn't list the group calendars.", &list_cause(error), ""),
                    cx,
                ));
            }
            for (index, calendar) in catalog.calendars.iter().enumerate() {
                let target = calendar.target.clone();
                let selected = account.selected_calendars.contains(&target);
                let current = handle.clone();
                let checkbox_id =
                    SharedString::from(format!("remote-{}-{index}", account.account.account_id));
                let checkbox = Checkbox::new(checkbox_id)
                    .debug_selector(|| format!("remote-{}-{index}", account.account.account_id))
                    .label(format!(
                        "{} · {} · {}",
                        calendar.name.as_deref().unwrap_or("Unnamed calendar"),
                        calendar_kind(&calendar.target),
                        capability_label(calendar.access),
                    ))
                    .checked(selected)
                    .disabled(busy || blocked)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let Some(account) = this.current_account(&current) else {
                            return;
                        };
                        if *checked {
                            let mut targets = account.selected_calendars.clone();
                            if !targets.contains(&target) {
                                targets.push(target.clone());
                            }
                            this.select_remote(id, &account, targets, cx);
                        } else {
                            // One route only: the others keep their cached events.
                            this.remove_remote(id, &account, &target, cx);
                        }
                    }));
                row = row.child(checkbox);
            }
        }
        scroll.child(row)
    }

    fn account_menu(
        &self,
        id: IntegrationId,
        account: &MicrosoftAccount,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let handle = account.account.clone();
        let node = account_node_id(id, &handle, "more");
        let opener = self.action_focus[&node].clone();
        let menu_opener = opener.clone();
        let view = cx.weak_entity();
        let busy = self.model.busy(id);
        let blocked = account.access_error.as_deref() == Some("conditional_access_blocked");
        let has_selection = !account.selected_calendars.is_empty();
        let directory = self
            .state
            .read(cx)
            .config()
            .integration
            .get(id.as_str())
            .and_then(|settings| settings.get("directory"))
            .and_then(|value| value.as_str());
        let directory_allowed =
            directory.is_none() || matches!(directory, Some("user-read-basic-all" | "people-read"));
        let directory_help = if directory == Some("people-read") {
            "Search people relevant to you"
        } else {
            "Search your organisation's people (photos included)"
        };
        Button::new(node)
            .label("More")
            .ghost()
            .track_focus(&opener)
            .debug_selector(|| format!("calendar-more-{}", account.account.account_id))
            .accessibility_label("More calendar account actions")
            .dropdown_menu(move |mut menu, _, _| {
                for (action, label, reason) in [
                    (
                        AccountAction::Reconnect,
                        "Reconnect…",
                        busy.then_some("A calendar action is running"),
                    ),
                    (
                        AccountAction::Grant("directory"),
                        "Allow directory search…",
                        if !directory_allowed {
                            Some("Directory search is disabled")
                        } else {
                            blocked.then_some("Ask your administrator to review this sign-in first")
                        },
                    ),
                    (
                        AccountAction::Grant("shared_calendars"),
                        "Allow shared calendars…",
                        blocked.then_some("Ask your administrator to review this sign-in first"),
                    ),
                    (
                        AccountAction::Grant("microsoft365_groups"),
                        "Allow Microsoft 365 groups…",
                        blocked.then_some("Ask your administrator to review this sign-in first"),
                    ),
                    (
                        AccountAction::Delegate,
                        "Add a delegated calendar…",
                        blocked.then_some("Ask your administrator to review this sign-in first"),
                    ),
                    (
                        AccountAction::Clear,
                        "Stop reading all calendars",
                        (!has_selection).then_some("No calendars selected"),
                    ),
                    (AccountAction::Remove, "Remove account", None),
                ] {
                    let reason = if busy {
                        Some("A calendar action is running")
                    } else {
                        reason
                    };
                    let help = reason.or(match action {
                        AccountAction::Grant("directory") => Some(directory_help),
                        AccountAction::Grant("microsoft365_groups") => {
                            Some("May need administrator approval")
                        }
                        _ => None,
                    });
                    let label =
                        help.map_or_else(|| label.to_owned(), |help| format!("{label} · {help}"));
                    let view = view.clone();
                    let handle = handle.clone();
                    let opener = menu_opener.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .disabled(reason.is_some())
                            .on_click(move |_, window, cx| {
                                let (view, handle, opener) =
                                    (view.clone(), handle.clone(), opener.clone());
                                window.defer(cx, move |window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.account_action(id, &handle, action, opener, window, cx)
                                    });
                                });
                            }),
                    );
                }
                menu
            })
    }

    fn account_action(
        &mut self,
        id: IntegrationId,
        handle: &AccountRef,
        action: AccountAction,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.model.busy(id) {
            return;
        }
        let Some(account) = self.current_account(handle) else {
            return;
        };
        let card = Card::account(id, handle.clone());
        let blocked = account.access_error.as_deref() == Some("conditional_access_blocked");
        match action {
            AccountAction::Reconnect | AccountAction::Grant(_) => {
                if blocked && !matches!(action, AccountAction::Reconnect) {
                    return;
                }
                self.sign_in(
                    card,
                    |attempt| {
                        let command = match action {
                            AccountAction::Grant(_) => "grant-account",
                            _ => "reconnect-account",
                        };
                        let mut args = IntegrationsModel::account_args(id, &account, command, true);
                        if let AccountAction::Grant(permission) = action {
                            args.extend(["--permission".into(), permission.into()]);
                        }
                        args.extend(["--attempt".into(), attempt.as_str().into()]);
                        args
                    },
                    cx,
                );
            }
            AccountAction::Clear => self.select_remote(id, &account, Vec::new(), cx),
            AccountAction::Delegate if !blocked => {
                self.open_delegated(id, handle.clone(), opener, window, cx)
            }
            AccountAction::Delegate => {}
            AccountAction::Remove => {
                self.remove_account_confirmation(id, &account, opener, window, cx)
            }
        }
    }

    fn open_delegated(
        &mut self,
        id: IntegrationId,
        handle: AccountRef,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let mailbox = cx.new(|cx| InputState::new(window, cx));
        let calendar = cx.new(|cx| InputState::new(window, cx));
        opener.focus(window, cx);
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            // BroScribe's dialogs stand on the popover ground, not the desk.
            let dialog = dialog.bg(cx.theme().popover);
            let handle = handle.clone();
            let view = view.clone();
            let mailbox_input = mailbox.clone();
            let calendar_input = calendar.clone();
            let opener = opener.clone();
            dialog
                .width(window.rem_size() * 26.)
                .title(heading(
                    "calendar-delegated-title",
                    "Add a delegated calendar",
                ))
                .child(kit::field(
                    "calendar-mailbox-label",
                    "Mailbox",
                    None,
                    |label| Input::new(&mailbox).aria_label(label),
                    cx,
                ))
                .child(kit::field(
                    "calendar-id-label",
                    "Calendar ID (optional)",
                    None,
                    |label| Input::new(&calendar).aria_label(label),
                    cx,
                ))
                .child(
                    Button::new("calendar-add-delegated")
                        .label("Add calendar")
                        .map(|button| kit::primary(button, false, cx))
                        .on_click(move |_, window, cx| {
                            let mailbox = mailbox_input.read(cx).value().trim().to_owned();
                            let calendar_id = calendar_input.read(cx).value().trim().to_owned();
                            let target = CalendarTarget::Delegated {
                                mailbox,
                                calendar_id: (!calendar_id.is_empty()).then_some(calendar_id),
                            };
                            let _ = view.update(cx, |this, cx| {
                                let Some(account) = this.current_account(&handle) else {
                                    return;
                                };
                                if this.model.busy(id)
                                    || account.access_error.as_deref()
                                        == Some("conditional_access_blocked")
                                {
                                    return;
                                }
                                if target.validate().is_err() {
                                    this.model.tell(
                                        Card::account(id, handle.clone()),
                                        DELEGATED_INVALID.to_owned(),
                                        true,
                                    );
                                    cx.notify();
                                    window.close_dialog(cx);
                                    return;
                                }
                                let mut targets = account.selected_calendars.clone();
                                if !targets.contains(&target) {
                                    targets.push(target);
                                }
                                this.select_remote(id, &account, targets, cx);
                                window.close_dialog(cx);
                            });
                        }),
                )
                .on_close(move |_, window, cx| opener.focus(window, cx))
        });
    }

    /// Google reads only calendars explicitly chosen from its catalog.
    fn render_google_calendars(&self, status: &IntegrationStatus, cx: &Context<Self>) -> gpui::Div {
        let id = IntegrationId::Google;
        let busy = self.model.busy(id);
        let mut section = div().flex().flex_col().gap_2();
        if status.state.status != AccountStatus::Active {
            return section;
        }
        section = section.child(text_line(
            node_id(id, "selected"),
            Role::Label,
            calendar_count_and_sync(
                status.state.native_calendar_ids.len(),
                status.state.last_sync,
            ),
        ));
        if status.state.native_calendar_ids.is_empty() {
            section = section.child(text_line(
                node_id(id, "none-selected"),
                Role::Label,
                "No Google calendar is read until you select calendars; the primary calendar \
                 is not chosen for you.",
            ));
        }
        section = section.child(
            Button::new("google-calendars")
                .debug_selector(|| "google-calendars".into())
                .label("Find calendars")
                .map(|button| {
                    crate::ui::kit::disable_with_reason(
                        button,
                        busy,
                        "An action for this calendar service is still running.",
                        cx,
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run(
                        Operation::new(Card::provider(id), Action::Catalog),
                        vec![
                            "integrations".into(),
                            "calendars".into(),
                            id.as_str().into(),
                        ],
                        None,
                        cx,
                    );
                })),
        );
        let Some(catalog) = self
            .model
            .google_catalog
            .as_ref()
            .filter(|catalog| catalog.authority.generation == status.state.generation)
        else {
            return section;
        };
        if catalog.calendars.is_empty() {
            section = section.child(text_line(
                node_id(id, "no-calendars"),
                Role::Label,
                "This Google account offers no calendar BroScribe can read.",
            ));
        }
        let gone = catalog
            .selected
            .iter()
            .filter(|id| !catalog.calendars.iter().any(|c| c.id == **id))
            .count();
        if gone > 0 {
            section = section.child(kit::status_line(
                node_id(id, "lost"),
                StatusKind::Attention,
                format!(
                    "{gone} selected {} no longer available in this Google account. Your next \
                     change leaves out the missing calendars.",
                    plural(gone as u64, "calendar is", "calendars are")
                ),
                cx,
            ));
        }
        for (index, calendar) in catalog.calendars.iter().enumerate() {
            let calendar_id = calendar.id.clone();
            let label = format!(
                "{}{} · {}",
                calendar.name.as_deref().unwrap_or("Unnamed calendar"),
                if calendar.primary { " (primary)" } else { "" },
                calendar.access_role
            );
            section = section.child(
                Checkbox::new(SharedString::from(format!("google-calendar-{index}")))
                    .debug_selector(move || format!("google-calendar-{index}"))
                    .label(label)
                    .checked(catalog.selected.contains(&calendar.id))
                    .disabled(busy)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        match this.model.google_selection(&calendar_id, *checked) {
                            Ok((args, stdin, authority)) => this.run(
                                Operation::new(Card::provider(id), Action::Configure)
                                    .expecting(authority),
                                args,
                                Some(stdin),
                                cx,
                            ),
                            Err(error) => {
                                this.model.tell(Card::provider(id), error, true);
                                cx.notify();
                            }
                        }
                    })),
            );
        }
        section
    }

    /// The stored Google client secret: saved again when missing, removable once signed
    /// out. Sign-out keeps it.
    fn render_google_secret(
        &self,
        status: &IntegrationStatus,
        busy: bool,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let id = IntegrationId::Google;
        let mut section = div().flex().flex_col().gap_2();
        match status.secret_control() {
            Some(SecretControl::Save) => {
                section = section.child(kit::status_line(
                    node_id(id, "secret-missing"),
                    StatusKind::Attention,
                    "No Google OAuth client secret is stored, so sign-in cannot start. Save it \
                     again to sign in.",
                    cx,
                ));
                if let Some(secret) = &self.google_secret {
                    section = section.child(kit::field(
                        node_id(id, "client-secret-label"),
                        "Client secret",
                        None,
                        |label| Input::new(secret).aria_label(label).disabled(busy),
                        cx,
                    ));
                }
                section = section.child(
                    Button::new("integration-save-secret-google")
                        .debug_selector(|| "integration-save-secret-google".into())
                        .label("Save client secret")
                        .map(|button| {
                            crate::ui::kit::disable_with_reason(
                                button,
                                busy,
                                "An action for this calendar service is still running.",
                                cx,
                            )
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let secret = this
                                .google_secret
                                .as_ref()
                                .map(|secret| secret.read(cx).value())
                                .unwrap_or_default();
                            match IntegrationsModel::save_secret_command(secret.as_str()) {
                                Ok((args, stdin)) => {
                                    // Replace the state as well as its text so undo cannot restore the secret.
                                    this.google_secret = Some(secret_input(window, cx));
                                    this.run(
                                        Operation::new(Card::provider(id), Action::SaveSecret),
                                        args,
                                        Some(stdin),
                                        cx,
                                    );
                                }
                                Err(error) => {
                                    this.model.tell(Card::provider(id), error, true);
                                    cx.notify();
                                }
                            }
                        })),
                );
            }
            Some(SecretControl::Remove) => {
                section = section.child(
                    Button::new("integration-remove-secret-google")
                        .debug_selector(|| "integration-remove-secret-google".into())
                        .label("Remove client secret")
                        .map(|button| {
                            crate::ui::kit::disable_with_reason(
                                button,
                                busy,
                                "An action for this calendar service is still running.",
                                cx,
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.run(
                                Operation::new(Card::provider(id), Action::RemoveSecret),
                                IntegrationsModel::clear_secret_args(),
                                None,
                                cx,
                            );
                        })),
                );
            }
            Some(SecretControl::SignOutFirst) => {
                section = section.child(text_line(
                    node_id(id, "secret-note"),
                    Role::Label,
                    "The Google OAuth client secret stays stored while you are signed in; sign \
                     out before removing it.",
                ));
            }
            None => {}
        }
        section
    }

    fn render_google_revoke(
        &self,
        status: Option<&IntegrationStatus>,
        busy: bool,
        cx: &Context<Self>,
    ) -> Button {
        let id = IntegrationId::Google;
        let signed_in =
            status.is_some_and(|status| status.state.status != AccountStatus::SignedOut);
        let key = "integration-revoke-open-google";
        let opener = self.action_focus[key].clone();
        Button::new(key).debug_selector(move || key.into()).label("Sign out and revoke at Google")
            .track_focus(&opener)
            .map(|button| kit::disable_with_reason(button, busy || !signed_in,
                if signed_in { "A calendar action is still running." } else { "Sign in to Google first." }, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                if window.has_active_dialog(cx) {
                    return;
                }
                if let Err(error) = this.model.prepare_google_revoke() {
                    this.model.tell(Card::provider(id), error, true);
                    cx.notify();
                    return;
                }
                let mut spec = ConfirmSpec::new(
                    confirm_line("google-revoke-title", "Sign out and revoke Google access?"),
                    "google-revoke-dialog", "google-revoke-actions", "integration-revoke-cancel-google",
                    confirm_line("integration-revoke-confirm-google", "Sign out and revoke"),
                );
                spec.warning = Some(confirm_line("google-revoke-scope",
                    "Google revokes every permission granted to your OAuth project, for every data folder, computer or tool signed in with it, and may take a while to apply it. Sign out alone keeps that access at Google."));
                spec.irreversible = false;
                this.open_confirmation(spec, opener.clone(), window, cx, move |this, cx| {
                    match this.model.confirm_google_revoke_args() {
                        Ok(args) => this.run(Operation::new(Card::provider(id), Action::SignOutRevoke), args, None, cx),
                        Err(error) => {
                            this.model.tell(Card::provider(id), error, true);
                            cx.notify();
                        }
                    }
                });
            }))
    }

    fn render_legacy_discard(&self, id: IntegrationId, cx: &Context<Self>) -> gpui::Div {
        let key = "discard-legacy-open-microsoft-graph";
        let opener = self.action_focus[key].clone();
        div().flex().flex_col().gap_2()
            .child(text_line(node_id(id, "legacy-note"), Role::Label,
                "An older Microsoft sign-in can't be matched to an account, so BroScribe doesn't use it."))
            .child(Button::new(key).debug_selector(move || key.into()).label("Remove old sign-in")
                .track_focus(&opener).disabled(self.model.busy(id))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if window.has_active_dialog(cx) {
                        return;
                    }
                    if let Err(error) = this.model.prepare_legacy_discard(id) {
                        this.model.tell(Card::provider(id), error, true);
                        cx.notify();
                        return;
                    }
                    let mut spec = ConfirmSpec::new(
                        confirm_line("legacy-discard-title", "Remove the old Microsoft sign-in?"),
                        "legacy-discard-dialog", "legacy-discard-actions", "discard-legacy-cancel-microsoft-graph",
                        confirm_line("discard-legacy-confirm-microsoft-graph", "Remove sign-in"),
                    );
                    spec.goes = vec![
                        confirm_line("legacy-sign-in", "Its saved sign-in"),
                        confirm_line("legacy-cache", "Its calendar cache and directory photos"),
                    ];
                    spec.stays = vec![
                        confirm_line("legacy-accounts", "Your accounts"),
                        confirm_line("legacy-meetings", "Meetings, people and recordings"),
                    ];
                    this.open_confirmation(spec, opener.clone(), window, cx, move |this, cx| {
                        match this.model.confirm_legacy_discard_args(id) {
                            Ok(args) => this.run(Operation::new(Card::provider(id), Action::DiscardLegacy), args, None, cx),
                            Err(error) => {
                                this.model.tell(Card::provider(id), error, true);
                                cx.notify();
                            }
                        }
                    });
                })))
    }

    fn render_direct(&self, cx: &Context<Self>) -> gpui::Div {
        let any_busy = direct_ids().any(|id| self.model.busy(id));
        let mut section = div()
            .flex()
            .flex_col()
            .gap_3()
            .when(self.native_supported, |section| {
                section.child(
                    kit::disclosure(
                        "integrations-advanced",
                        "Microsoft 365 and Google (advanced)",
                        self.advanced,
                        cx,
                        |this, _, cx| {
                            if this.advanced {
                                this.close_advanced();
                            } else {
                                this.advanced = true;
                            }
                            cx.notify();
                        },
                    )
                    .debug_selector(|| "integrations-advanced".into())
                    .map(|button| {
                        kit::disable_with_reason(
                            button,
                            any_busy,
                            "A direct calendar sign-in or update is still running.",
                            cx,
                        )
                    }),
                )
            });
        if !self.direct_open() {
            return section;
        }
        section = section.when(self.native_supported, |section| section.child(text_line(
            "calendars-advanced-note", Role::Label,
            "For organisations that register BroScribe with Microsoft or Google. You need a client ID from your administrator. On a Mac, system calendars need none of this.",
        )));
        for id in direct_ids() {
            let busy = self.model.busy(id);
            let status = self.model.statuses.get(&id);
            let configured = status.is_some_and(|status| status.configured);
            let mut row = div()
                .id(node_id(id, "card"))
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .border_1()
                .border_color(cx.theme().border)
                .anchor_scroll(self.anchor(id.as_str()))
                .child(heading(node_id(id, "heading"), id.display_name()))
                .child(text_line(
                    node_id(id, "state"),
                    Role::Status,
                    status.map_or("Status unavailable", IntegrationStatus::label),
                ));
            if let Some(status) = status {
                // Google's verified address is the primary calendar's ID, read with the
                // calendar permission at sign-in; no profile permission is requested.
                if id == IntegrationId::Google
                    && status.state.status != AccountStatus::SignedOut
                    && let Some(email) = status
                        .state
                        .account
                        .as_ref()
                        .and_then(|account| account.email.as_deref())
                {
                    row = row.child(text_line(
                        node_id(id, "account"),
                        Role::Label,
                        format!("Google account: {email}"),
                    ));
                }
                row = row.child(text_line(
                    node_id(id, "capabilities"),
                    Role::Label,
                    format!(
                        "Calendar access · {}. Directory access · {}.",
                        capability_label(status.state.capabilities.calendar),
                        capability_label(status.state.capabilities.directory),
                    ),
                ));
                if !status.admin_controlled_scopes.is_empty() {
                    row = row.child(text_line(
                        node_id(id, "admin-scopes"),
                        Role::Label,
                        "Your administrator may need to approve some permissions.",
                    ));
                }
                if let Some(error) = &status.config_error {
                    row = row.child(kit::status_line(
                        node_id(id, "config-error"),
                        StatusKind::Failed,
                        config_error_text(id, error),
                        cx,
                    ));
                }
            }
            if !configured {
                row = row.child(text_line(
                    node_id(id, "not-configured"),
                    Role::Label,
                    "Not set up. Enter the client ID from your administrator.",
                ));
                if let Some(client) = self.clients.get(&id) {
                    row = row.child(kit::field(
                        node_id(id, "client-id-label"),
                        "Client ID",
                        None,
                        |label| Input::new(client).aria_label(label).disabled(busy),
                        cx,
                    ));
                }
                if id == IntegrationId::Google
                    && let Some(secret) = &self.google_secret
                {
                    row = row.child(kit::field(
                        node_id(id, "client-secret-label"),
                        "Client secret",
                        None,
                        |label| Input::new(secret).aria_label(label).disabled(busy),
                        cx,
                    ));
                }
                row = row.child(
                    Button::new(SharedString::from(format!(
                        "integration-save-{}",
                        id.as_str()
                    )))
                    .label("Save")
                    .map(|button| {
                        crate::ui::kit::disable_with_reason(
                            button,
                            busy,
                            "An action for this calendar service is still running.",
                            cx,
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let client = this
                            .clients
                            .get(&id)
                            .map(|client| client.read(cx).value())
                            .unwrap_or_default();
                        let secret = match (&this.google_secret, id) {
                            (Some(secret), IntegrationId::Google) => secret.read(cx).value(),
                            _ => SharedString::default(),
                        };
                        match IntegrationsModel::setup_command(id, client.as_str(), secret.as_str())
                        {
                            Ok((args, stdin)) => {
                                if id == IntegrationId::Google {
                                    // Replace the state as well as its text so undo cannot restore the secret.
                                    this.google_secret = Some(secret_input(window, cx));
                                }
                                this.run(
                                    Operation::new(Card::provider(id), Action::SaveClient),
                                    args,
                                    stdin,
                                    cx,
                                );
                            }
                            Err(error) => {
                                this.model.tell(Card::provider(id), error, true);
                                cx.notify();
                            }
                        }
                    })),
                );
            }
            if id == IntegrationId::Google {
                if let Some(status) = status {
                    row = row.child(self.render_google_secret(status, busy, cx));
                    row = row.child(self.render_google_calendars(status, cx));
                }
            } else {
                row = row.child(text_line(node_id(id, "sign-in-note"), Role::Label,
                    "Sign-in verifies your Microsoft account. BroScribe reads only the calendars you choose."));
                if let Some(status) = status {
                    if status.legacy_microsoft
                        == Some(broscribe_integrations::LegacyMicrosoftStatus::Quarantined)
                    {
                        row = row.child(self.render_legacy_discard(id, cx));
                    }
                    for account in &status.microsoft_accounts {
                        row = row.child(self.render_account(id, account, cx));
                    }
                }
            }
            row = row
                .child(
                    kit::disclosure(
                        node_id(id, "instructions-toggle"),
                        "Setup instructions",
                        self.instructions_open.contains(&id),
                        cx,
                        move |this, _, cx| {
                            if !this.instructions_open.remove(&id) {
                                this.instructions_open.insert(id);
                                if !this.model.instructions.contains_key(&id) {
                                    this.instructions(id, cx);
                                }
                            }
                            cx.notify();
                        },
                    )
                    .disabled(busy),
                )
                .child(
                    Button::new(SharedString::from(format!(
                        "integration-login-{}",
                        id.as_str()
                    )))
                    .debug_selector(move || format!("integration-login-{}", id.as_str()))
                    .label(if id == IntegrationId::Google {
                        "Sign in with Google…"
                    } else {
                        "Sign in with Microsoft…"
                    })
                    .track_focus(
                        &self.action_focus[format!("integration-login-{}", id.as_str()).as_str()],
                    )
                    .disabled(busy || !configured)
                    .when(busy || !configured, |button| {
                        button.tooltip(if !configured {
                            "Save the calendar integration settings first."
                        } else {
                            "A calendar action is still running."
                        })
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sign_in(
                            Card::provider(id),
                            |attempt| IntegrationsModel::login_args(id, attempt),
                            cx,
                        );
                    })),
                )
                .children((id == IntegrationId::Google).then(|| {
                    Button::new(SharedString::from(format!(
                        "integration-logout-{}",
                        id.as_str()
                    )))
                    .label("Sign out")
                    .disabled(
                        busy || !status
                            .is_some_and(|status| status.state.status != AccountStatus::SignedOut),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.run(
                            Operation::new(Card::provider(id), Action::SignOut),
                            vec!["integrations".into(), "logout".into(), id.as_str().into()],
                            None,
                            cx,
                        );
                    }))
                }));
            if id == IntegrationId::Google {
                row = row.child(self.render_google_revoke(status, busy, cx));
            }
            if self.instructions_open.contains(&id)
                && let Some(instructions) = self.model.instructions.get(&id)
            {
                row = row.child(
                    text_line(
                        node_id(id, "instructions"),
                        Role::Code,
                        instructions.clone(),
                    )
                    .text_role(TextRole::Mono)
                    .font_family(cx.theme().mono_font_family.clone())
                    .p_2()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted),
                );
            }
            row = row.child(self.card_state(&Card::provider(id), cx));
            section = section.child(row);
        }
        section
    }
}
impl Drop for IntegrationsView {
    /// Closing the view cancels its running sign-ins in the attempt journal.
    fn drop(&mut self) {
        for (_, args, _) in self.model.cancel_all_requests() {
            self.cli
                .run(args, ChildKind::Command, &self.executor)
                .detach();
        }
    }
}
impl Render for IntegrationsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_direct_inputs(window, cx);
        // The page is one named group in Settings and in the setup assistant, whose
        // own page landmark holds it.
        let mut view = div()
            .id("settings-integrations")
            .role(Role::Group)
            .aria_label("Calendars")
            .flex()
            .flex_col()
            .gap_3()
            .when(!self.in_setup, |view| {
                view.child(title("calendars-heading", "Calendars"))
            })
            .child(text_line(
                "calendars-intro",
                Role::Label,
                if self.native_supported {
                    "Calendars help name meetings and their participants."
                } else {
                    "Connect Microsoft 365 or Google to name meetings and their participants. Your organisation registers BroScribe with them first."
                },
            ));
        if let Some(target) = &self.target {
            let (provider, account_id) = target
                .split_once('/')
                .map_or((target.as_str(), None), |(provider, account)| {
                    (provider, Some(account))
                });
            if let Ok(id) = IntegrationId::parse(provider)
                && (self.native_supported || !id.is_native_calendar())
            {
                let account = account_id.and_then(|id| {
                    self.model
                        .statuses
                        .values()
                        .flat_map(|status| &status.microsoft_accounts)
                        .find(|account| account.account.account_id.as_str() == id)
                });
                let words = account.map_or_else(
                    || format!("{} needs your attention.", id.display_name()),
                    |account| {
                        format!(
                            "Microsoft 365 needs your attention: sign in again for {}.",
                            calendar_account_name(account)
                        )
                    },
                );
                view = view.child(kit::status_line(
                    "calendars-target",
                    StatusKind::Attention,
                    words,
                    cx,
                ));
                let focus = account
                    .and_then(|account| {
                        self.action_focus
                            .get(&account_node_id(id, &account.account, "more"))
                    })
                    .or_else(|| {
                        account_id
                            .is_none()
                            .then(|| {
                                self.action_focus
                                    .get(format!("integration-login-{id}").as_str())
                            })
                            .flatten()
                    });
                if self.target_needs_focus
                    && let Some(focus) = focus.cloned()
                {
                    self.target_needs_focus = false;
                    if let Some(anchor) = &self.target_anchor {
                        anchor.scroll_to(window, cx);
                    }
                    window.on_next_frame(move |window, cx| focus.focus(window, cx));
                }
            }
        }
        if self.native_supported {
            for source in NATIVE_IDS {
                view = view.child(self.render_native(source, cx));
            }
        }
        view = view.child(self.render_direct(cx));
        view.when(self.model.status_loading, |el| {
            el.child(kit::status_line(
                "calendars-status-loading",
                StatusKind::Pending,
                "Reading calendars…",
                cx,
            ))
        })
        .when_some(self.model.status_error.as_deref(), |el, error| {
            el.child(kit::retry_line(
                "calendars-status-error",
                status_error_text(error),
                cx,
                |this, _, cx| this.reload(cx),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    #[gpui::test]
    fn quarantined_slot_confirmation_cancels_without_child_and_keeps_its_original_fence(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::{ps_path, ps_str, write_cli};
        use broscribe_core::Library;
        use broscribe_platform::{fakes::FakePaths, resolve_paths};

        let dir = tempfile::tempdir().unwrap();
        let handle = "11111111111111111111111111111111";
        let account = serde_json::json!({
            "account":{"integration":"microsoft-graph","account_id":handle},
            "identity":{"authority_host":"login.microsoftonline.com","resource_tenant_id":"synthetic-tenant",
                "object_id":handle,"client_id":"synthetic-client","verified_addresses":[]},
            "generation":3,"selection_generation":2,"status":"active","granted_scopes":["User.Read"],
            "selected_calendars":[{"kind":"own","calendar_id":"synthetic-calendar"}],
            "calendar":"available","directory":"not_requested","shared_calendars":"not_requested",
            "microsoft365_groups":"not_requested","target_results":[]
        });
        let status = serde_json::json!({"integrations":[{
            "integration":"microsoft-graph","configured":true,"state":{"status":"signed_out","generation":9},
            "legacy_microsoft":"quarantined","microsoft_accounts":[account]
        }]});
        let status_path = dir.path().join("status.json");
        std::fs::write(&status_path, serde_json::to_vec(&status).unwrap()).unwrap();
        let marker = dir.path().join("mutations");
        let superseded = r#"{"error":{"code":"superseded","message":"Old sign-in generation changed. Refresh status before deciding again."}}"#;
        let unexpected = r#"{"error":{"code":"unexpected_action","message":"Wrong command, generation or provider"}}"#;
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "case \"$*\" in\n*\"discard-legacy microsoft-graph --generation 9 --yes\"*) printf '%s\\n' discard >> '{}'; printf '%s\\n' '{superseded}'; exit 1;;\n*\"integrations status\"*) cat '{}';;\n*) printf '%s\\n' unexpected >> '{}'; printf '%s\\n' '{unexpected}'; exit 1;;\nesac\n",
                marker.display(),
                status_path.display(),
                marker.display()
            ),
            &format!(
                "if($all -clike '*discard-legacy microsoft-graph --generation 9 --yes*'){{fx_append {marker} ('discard'+[char]10);fx_echo {superseded};exit 1}}\
                 elseif($all -clike '*integrations status*'){{fx_cat {status}}}\
                 else{{fx_append {marker} ('unexpected'+[char]10);fx_echo {unexpected};exit 1}}",
                marker = ps_path(&marker),
                status = ps_path(&status_path),
                superseded = ps_str(superseded),
                unexpected = ps_str(unexpected),
            ),
        );
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Default::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    cli,
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let mut view = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let integration = cx.new(|cx| IntegrationsView::with_native_support(state, true, cx));
            view = Some(integration.clone());
            gpui::component::Root::new(integration, window, cx)
        });
        let view = view.unwrap();
        cx.run_until_parked();
        view.update(cx, |this, cx| {
            this.advanced = true;
            cx.notify();
        });
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(2400.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let open = cx
            .debug_bounds("discard-legacy-open-microsoft-graph")
            .unwrap();
        cx.simulate_click(open.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let cancel = cx
            .debug_bounds("discard-legacy-cancel-microsoft-graph")
            .unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            !marker.exists(),
            "Cancel must not send a mutation or start authentication"
        );
        view.read_with(cx, |this, _| assert!(this.model.legacy_discard.is_none()));

        cx.update(|window, cx| window.draw(cx).clear(cx));
        let open = cx
            .debug_bounds("discard-legacy-open-microsoft-graph")
            .unwrap();
        cx.simulate_click(open.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        view.update(cx, |this, cx| {
            let mut newer = status.clone();
            newer["integrations"][0]["state"]["generation"] = serde_json::json!(10);
            this.model.apply_statuses(newer).unwrap();
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let confirm = cx
            .debug_bounds("discard-legacy-confirm-microsoft-graph")
            .unwrap();
        cx.simulate_click(confirm.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "discard\n",
            "exactly one fenced mutation, without retry"
        );
        view.read_with(cx, |this, _| {
            assert!(this.model.legacy_discard.is_none());
            let card = Card::provider(IntegrationId::MicrosoftGraph);
            let message = this.model.message(&card).unwrap();
            // The refusal's words, without its code (WRT-04).
            assert!(
                message.error
                    && message
                        .text
                        .contains("Refresh status before deciding again."),
                "{}",
                message.text
            );
            assert!(!message.text.contains("(superseded)"), "{}", message.text);
            let accounts = &this.model.statuses[&IntegrationId::MicrosoftGraph].microsoft_accounts;
            assert_eq!(accounts.len(), 1);
            assert_eq!(accounts[0].account.account_id.as_str(), handle);
            assert_eq!(accounts[0].generation, 3);
            assert_eq!(
                accounts[0].selected_calendars,
                vec![CalendarTarget::Own {
                    calendar_id: "synthetic-calendar".into()
                }]
            );
        });
    }

    use super::*;

    #[test]
    fn advanced_calendar_permissions_are_explained_in_plain_words() {
        assert_eq!(
            calendar_permission_words("User.ReadBasic.All"),
            "Search your organisation's people and photos"
        );
        assert_eq!(
            calendar_permission_words("Calendars.Read"),
            "Read your calendars"
        );
        assert_eq!(
            calendar_permission_words("Calendars.Read.Shared"),
            "Read calendars shared with you"
        );
        assert_eq!(
            calendar_permission_words("unknown-scope"),
            "Other account permission"
        );
        assert!(NATIVE_DISCLOSURE.contains("voice data"));
        assert!(NATIVE_DISCLOSURE.contains("System Settings › Internet Accounts"));
    }

    #[gpui::test]
    fn corporate_route_action_refuses_stale_account_without_touching_other_account(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::{ps_path, ps_str, write_cli};
        use broscribe_core::Library;
        use broscribe_platform::{fakes::FakePaths, resolve_paths};

        let dir = tempfile::tempdir().unwrap();
        let first = "11111111111111111111111111111111";
        let second = "22222222222222222222222222222222";
        let make_account = |handle: &str| {
            serde_json::json!({
                "account":{"integration":"microsoft-graph","account_id":handle},
                "identity":{"authority_host":"login.microsoftonline.com","resource_tenant_id":"synthetic-tenant",
                    "object_id":handle,"client_id":"synthetic-client","display_name":"Synthetic account",
                    "tenant_display_name":"Synthetic tenant","verified_addresses":[]},
                "generation":4,"selection_generation":7,"status":"active","granted_scopes":["User.Read","Calendars.Read"],
                "selected_calendars":[{"kind":"own","calendar_id":"same-id"}],
                "calendar":"available","directory":"available","shared_calendars":"unknown",
                "microsoft365_groups":"unknown","last_sync":null,
                "target_results":[{"target":{"kind":"own","calendar_id":"same-id"},"outcome":{"status":"denied","code":"forbidden"}}]
            })
        };
        let status = serde_json::json!({"integrations":[{
            "integration":"microsoft-graph","configured":true,"state":{"status":"signed_out"},
            "microsoft_accounts":[make_account(first),make_account(second)]
        }]});
        let status_path = dir.path().join("status.json");
        std::fs::write(&status_path, serde_json::to_vec(&status).unwrap()).unwrap();
        let spec = dir.path().join("spec.json");
        let superseded = r#"{"error":{"code":"superseded","message":"Account changed; discover its calendars again."}}"#;
        let unexpected =
            r#"{"error":{"code":"unexpected_route","message":"Wrong account or generation"}}"#;
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "case \"$*\" in\n*remove-calendar*--account\\ {first}*--generation\\ 4*--selection-generation\\ 7*--spec-stdin*) cat > '{}'; printf '%s\\n' '{superseded}'; exit 1;;\n*status*) cat '{}';;\n*) printf '%s\\n' '{unexpected}'; exit 1;;\nesac\n",
                spec.display(),
                status_path.display()
            ),
            &format!(
                "if($all -clike '*remove-calendar*--account {first}*--generation 4*--selection-generation 7*--spec-stdin*'){{fx_save_stdin {spec};fx_echo {superseded};exit 1}}\
                 elseif($all -clike '*status*'){{fx_cat {status}}}\
                 else{{fx_echo {unexpected};exit 1}}",
                spec = ps_path(&spec),
                status = ps_path(&status_path),
                superseded = ps_str(superseded),
                unexpected = ps_str(unexpected),
            ),
        );
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Default::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    cli,
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let mut view = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let integration = cx.new(|cx| IntegrationsView::with_native_support(state, true, cx));
            view = Some(integration.clone());
            gpui::component::Root::new(integration, window, cx)
        });
        let view = view.unwrap();
        cx.run_until_parked();
        view.update(cx, |this, cx| {
            this.advanced = true;
            // The catalog is a transport response scoped to this exact identity/epoch.
            let account = this.model.statuses[&IntegrationId::MicrosoftGraph].microsoft_accounts[0].clone();
            this.model.catalogs.insert(account.account.clone(), serde_json::from_value(serde_json::json!({
                "account":account.account,"generation":4,"selection_generation":7,"groups":"unknown",
                "calendars":[{"target":{"kind":"own","calendar_id":"same-id"},"name":"Synthetic calendar",
                    "owner_name":null,"owner_address":null,"is_default":true,"can_edit":false,"access":"available"}],
                "groups_error":null
            })).unwrap());
            cx.notify();
        });
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(2400.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| {
            let view = view.clone();
            window.open_dialog(cx, move |dialog, _, cx| {
                // BroScribe's dialogs stand on the popover ground, not the desk.
                let dialog = dialog.bg(cx.theme().popover);
                let body = view.update(cx, |this, cx| {
                    this.catalog_dialog(IntegrationId::MicrosoftGraph, &account_ref(), cx)
                });
                dialog.child(body)
            });
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let checkbox = cx
            .debug_bounds("remote-11111111111111111111111111111111-0")
            .unwrap();
        cx.simulate_click(checkbox.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |this, _| {
            let accounts = &this.model.statuses[&IntegrationId::MicrosoftGraph].microsoft_accounts;
            // Unchecking removes exactly that route, named on stdin, never in argv.
            let sent: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&spec).unwrap()).unwrap();
            assert_eq!(sent, serde_json::json!({"target":{"kind":"own","calendar_id":"same-id"}}));
            // The refusal is shown on that account's card only.
            let own = Card::account(IntegrationId::MicrosoftGraph, accounts[0].account.clone());
            let message = this.model.message(&own).unwrap();
            assert!(
                message.error && message.text.contains("discover its calendars again."),
                "{}",
                message.text
            );
            assert!(!message.text.contains("(superseded)"), "{}", message.text);
            let other = Card::account(IntegrationId::MicrosoftGraph, accounts[1].account.clone());
            assert!(this.model.message(&other).is_none());
            assert!(this.model.message(&Card::provider(IntegrationId::MicrosoftGraph)).is_none());
            assert!(!this.model.busy(IntegrationId::MicrosoftGraph));
            assert_eq!(accounts.len(), 2);
            assert_eq!(accounts[1].account.account_id.as_str(), second);
            assert_eq!(accounts[1].selected_calendars, vec![CalendarTarget::Own { calendar_id:"same-id".into() }]);
            assert!(matches!(&accounts[0].target_results[0].outcome,
                broscribe_integrations::CalendarTargetOutcome::Denied { code } if code == "forbidden"));
        });
    }
    #[test]
    fn calendar_instructions_follow_the_host() {
        assert!(
            calendar_settings_hint_for(HostPlatform::MacOs)
                .contains("calendars macOS already syncs")
        );
        for host in [HostPlatform::Windows, HostPlatform::Linux] {
            assert_eq!(
                calendar_settings_hint_for(host),
                "Connect a Microsoft 365 or Google calendar under Advanced."
            );
        }
    }

    #[test]
    fn status_distinguishes_unconfigured_and_reauthentication() {
        let mut model = IntegrationsModel::default();
        model.apply_statuses(serde_json::json!({"integrations":[
            {"integration":"google","configured":false,"state":{"status":"signed_out"}},
            {"integration":"microsoft-graph","configured":true,"state":{"status":"reauth_required"}}
        ]})).unwrap();
        assert_eq!(model.statuses[&IntegrationId::Google].label(), "Not set up");
        assert_eq!(
            model.statuses[&IntegrationId::MicrosoftGraph].label(),
            "Sign in required"
        );
    }
    #[test]
    fn login_uses_account_qualified_microsoft_flow_and_google_login_under_an_attempt() {
        let attempt = LoginAttemptId::parse("synthetic-attempt").unwrap();
        assert_eq!(
            IntegrationsModel::login_args(IntegrationId::MicrosoftGraph, &attempt),
            [
                "integrations",
                "add-account",
                "microsoft-graph",
                "--attempt",
                "synthetic-attempt"
            ]
            .map(OsString::from)
        );
        assert_eq!(
            IntegrationsModel::login_args(IntegrationId::Google, &attempt),
            [
                "integrations",
                "login",
                "google",
                "--attempt",
                "synthetic-attempt"
            ]
            .map(OsString::from)
        );
        assert_eq!(
            IntegrationsModel::cancel_args(IntegrationId::Google, &attempt),
            [
                "integrations",
                "cancel-login",
                "google",
                "--attempt",
                "synthetic-attempt"
            ]
            .map(OsString::from)
        );
    }
    #[test]
    fn registered_client_setup_rejects_empty_ids_and_preserves_client_change_notices() {
        let (args, stdin) =
            IntegrationsModel::setup_command(IntegrationId::MicrosoftGraph, " synthetic-own ", "")
                .unwrap();
        assert_eq!(
            args,
            [
                "integrations",
                "setup",
                "microsoft-graph",
                "--client-id",
                "synthetic-own"
            ]
            .map(OsString::from)
        );
        assert!(stdin.is_none());
        assert!(
            IntegrationsModel::setup_command(IntegrationId::MicrosoftGraph, " ", "")
                .unwrap_err()
                .contains("registered")
        );
        // A retired sign-in is announced; an unchanged client keeps the plain notice.
        let retired = IntegrationsModel::notice_for(
            Action::SaveClient,
            &serde_json::json!({"client_change":{"effective_changed":true,"sign_in_retired":true}}),
        );
        assert!(retired.contains("Sign in"), "{retired}");
        assert_eq!(
            IntegrationsModel::notice_for(
                Action::SaveClient,
                &serde_json::json!({"client_change":{"effective_changed":false,"sign_in_retired":false}}),
            ),
            Action::SaveClient.notice()
        );
    }
    #[test]
    fn google_setup_sends_secret_only_on_stdin() {
        let secret = "synthetic-client-secret";
        assert!(IntegrationsModel::setup_command(IntegrationId::Google, " ", secret).is_err());
        assert!(IntegrationsModel::setup_command(IntegrationId::Google, "client", " ").is_err());
        let (args, stdin) =
            IntegrationsModel::setup_command(IntegrationId::Google, " synthetic-client ", secret)
                .unwrap();
        assert_eq!(
            args,
            [
                "integrations",
                "setup",
                "google",
                "--client-id",
                "synthetic-client",
                "--client-secret-stdin",
            ]
            .map(OsString::from)
        );
        assert!(
            args.iter()
                .all(|arg| !arg.to_string_lossy().contains(secret))
        );
        assert_eq!(stdin, Some(secret.as_bytes().to_vec()));
    }
    #[test]
    fn microsoft_setup_does_not_request_a_client_secret() {
        let (args, stdin) =
            IntegrationsModel::setup_command(IntegrationId::MicrosoftGraph, "synthetic-client", "")
                .unwrap();
        assert_eq!(
            args,
            [
                "integrations",
                "setup",
                "microsoft-graph",
                "--client-id",
                "synthetic-client"
            ]
            .map(OsString::from)
        );
        assert!(stdin.is_none());
    }

    fn native(
        status: &str,
        generation: u64,
        selected: &[&str],
        native: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({"integrations":[{
            "integration":"system-calendar",
            "configured":true,
            "state":{"generation":generation,"status":status,"native_calendar_ids":selected},
            "system_calendar":native
        }]})
    }

    fn granted(calendars: &[&str]) -> serde_json::Value {
        let calendars: Vec<_> = calendars
            .iter()
            .map(|id| serde_json::json!({"id":id,"title":format!("Synthetic {id}"),"source":"Synthetic"}))
            .collect();
        serde_json::json!({"authorization":"granted","helper_required":false,"calendars":calendars})
    }

    fn state_of(value: serde_json::Value) -> NativeState {
        let mut model = IntegrationsModel::default();
        model.apply_statuses(value).unwrap();
        model.statuses[&IntegrationId::SystemCalendar].native_state()
    }

    #[test]
    fn native_status_reports_only_what_the_agent_knows() {
        let unknown =
            serde_json::json!({"authorization":null,"helper_required":true,"calendars":[]});
        assert_eq!(
            state_of(native("signed_out", 0, &[], unknown.clone())),
            NativeState::AgentMissing
        );
        // An unknown permission with an explanation names the actual cause.
        let mut outdated = native("active", 1, &[], unknown);
        outdated["integrations"][0]["helper_error"] =
            serde_json::json!({"code":"helper_outdated","message":"synthetic"});
        assert_eq!(state_of(outdated), NativeState::AgentOutdated);
        for (authorization, expected) in [
            ("denied", NativeState::Denied),
            ("restricted", NativeState::Restricted),
            ("write_only", NativeState::WriteOnly),
            ("unsupported", NativeState::Unsupported),
        ] {
            let value = serde_json::json!({"authorization":authorization,"helper_required":false,"calendars":[]});
            assert_eq!(state_of(native("active", 1, &[], value)), expected);
        }
        assert_eq!(
            state_of(native("signed_out", 1, &[], granted(&[]))),
            NativeState::NotConnected { granted: true }
        );
        assert_eq!(
            state_of(native("active", 1, &[], granted(&[]))),
            NativeState::Connected
        );
        let pending = serde_json::json!({"authorization":"not_determined","helper_required":false,"calendars":[]});
        assert_eq!(
            state_of(native("active", 1, &[], pending)),
            NativeState::Pending
        );
        // Disconnected, denied, unknown or outdated: nothing to select.
        assert!(!NativeState::AgentMissing.can_connect());
        assert!(!NativeState::Denied.can_connect());
        assert!(NativeState::NotConnected { granted: false }.can_connect());
    }

    const NATIVE: IntegrationId = IntegrationId::SystemCalendar;

    #[test]
    fn native_flow_needs_no_client_and_selects_nothing_implicitly() {
        assert!(IntegrationsModel::setup_command(NATIVE, "x", "y").is_err());
        // A macOS permission request, never a browser sign-in attempt.
        assert_eq!(
            IntegrationsModel::connect_args(NATIVE),
            ["integrations", "login", "system-calendar"].map(OsString::from)
        );
        let mut model = IntegrationsModel::default();
        model
            .apply_statuses(native("active", 4, &[], granted(&["cal-a", "cal-b"])))
            .unwrap();
        let draft = model.selection[&NATIVE].clone();
        assert!(draft.ids.is_empty(), "nothing is selected by default");
        assert!(
            !model.selection_changed(NATIVE),
            "opening the view does not configure any calendar"
        );
        assert!(model.toggle_calendar(NATIVE, draft.generation, "cal-b", true));
        assert!(model.selection_changed(NATIVE));
        let (args, stdin) = model.native_selection(NATIVE).unwrap();
        assert_eq!(args, NATIVE_SELECTION_ARGS.map(OsString::from));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdin).unwrap(),
            serde_json::json!({"calendar_ids":["cal-b"]})
        );
    }

    /// The one argv of a native selection: its calendars only ever travel on stdin.
    const NATIVE_SELECTION_ARGS: [&str; 4] =
        ["integrations", "setup", "system-calendar", "--spec-stdin"];

    #[test]
    fn late_and_duplicate_status_results_keep_the_users_choice() {
        let mut model = IntegrationsModel::default();
        let old = model.begin_reload();
        let new = model.begin_reload();
        let status = native("active", 4, &["cal-a"], granted(&["cal-a", "cal-b"]));
        assert!(model.finish_reload(new, Ok(status.clone())));
        let generation = model.selection[&NATIVE].generation;
        // The saved selection is shown; the user adds another calendar.
        assert!(model.toggle_calendar(NATIVE, generation, "cal-b", true));
        // A stale read never overwrites the newer one.
        let stale = native("signed_out", 3, &[], granted(&[]));
        assert!(!model.finish_reload(old, Ok(stale)));
        assert_eq!(model.statuses[&NATIVE].state.generation, 4);
        // The same status again keeps the unsaved choice.
        let again = model.begin_reload();
        assert!(model.finish_reload(again, Ok(status)));
        let ids: Vec<_> = model.selection[&NATIVE].ids.iter().cloned().collect();
        assert_eq!(ids, ["cal-a", "cal-b"]);
        // A calendar macOS does not offer cannot be chosen.
        assert!(!model.toggle_calendar(NATIVE, generation, "cal-elsewhere", true));

        // A selection saved elsewhere (new account generation): the draft starts over.
        let saved = model.begin_reload();
        let status = native(
            "active",
            5,
            &["cal-b", "cal-gone"],
            granted(&["cal-a", "cal-b"]),
        );
        assert!(model.finish_reload(saved, Ok(status)));
        let draft = model.selection[&NATIVE].clone();
        assert_eq!(draft.ids.iter().collect::<Vec<_>>(), ["cal-b"]);
        // A click on the list from before is ignored.
        assert!(!model.toggle_calendar(NATIVE, generation, "cal-a", true));
        // A removed calendar is reported, never replaced by others.
        assert_eq!(model.statuses[&NATIVE].lost_calendars(), 1);
        // Saving now would drop the lost calendar only after an explicit choice.
        assert!(model.selection_changed(NATIVE));

        // Disconnected: no selection to act on.
        let off = model.begin_reload();
        assert!(model.finish_reload(off, Ok(native("signed_out", 6, &[], granted(&[])))));
        assert!(!model.selection.contains_key(&NATIVE));
        assert!(model.native_selection(NATIVE).is_err());
    }

    #[test]
    fn deselecting_every_calendar_can_explicitly_clear_the_saved_selection() {
        let mut model = IntegrationsModel::default();
        model
            .apply_statuses(native("active", 7, &["cal-a"], granted(&["cal-a"])))
            .unwrap();
        assert!(model.toggle_calendar(NATIVE, 7, "cal-a", false));
        assert!(model.selection_changed(NATIVE));
        let (args, stdin) = model.native_selection(NATIVE).unwrap();
        assert_eq!(args, NATIVE_SELECTION_ARGS.map(OsString::from));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdin).unwrap(),
            serde_json::json!({"calendar_ids":[]})
        );
    }

    #[test]
    fn a_failed_calendar_listing_is_not_a_connected_source_without_calendars() {
        let mut report = granted(&[]);
        report["calendars_error"] =
            serde_json::json!({"code":"calendar_unavailable","message":"synthetic"});
        let mut model = IntegrationsModel::default();
        model
            .apply_statuses(native("active", 2, &[], report))
            .unwrap();
        let state = model.statuses[&NATIVE].native_state();
        assert!(
            matches!(&state, NativeState::ListFailed { code, .. } if code == "calendar_unavailable")
        );
        assert_ne!(state, NativeState::Connected);
        assert!(!model.selection.contains_key(&NATIVE));
        assert!(model.native_selection(NATIVE).is_err());
        // WRT-04: the guidance says what failed and why in words.
        assert_eq!(
            state.guidance(CalendarProvider::EventKit).as_deref(),
            Some("Couldn't list the calendars. Synthetic.")
        );
    }

    /// WRT-04: an unknown code with nothing known about it is still a line
    /// that says what failed, never an empty Alert or the code.
    #[test]
    fn a_native_failure_without_a_cause_still_says_what_failed() {
        let line = native_error(
            CalendarProvider::EventKit,
            "Couldn't connect the system calendars.",
            "zz_synthetic_code",
            "",
        );
        assert_eq!(line, "Couldn't connect the system calendars.");
        let listed = list_cause(&CalendarListError {
            code: "internal".into(),
            message: "internal error: /synthetic/private/Calendars.sqlite".into(),
        });
        assert_eq!(listed, "Something went wrong inside BroScribe.");
    }

    #[gpui::test]
    fn native_view_only_changes_explicit_calendar_selection(cx: &mut gpui::TestAppContext) {
        use broscribe_core::Library;
        use broscribe_platform::{fakes::FakePaths, resolve_paths};

        let dir = tempfile::tempdir().unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Default::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let (view, cx) =
            cx.add_window_view(move |_, cx| IntegrationsView::with_native_support(state, true, cx));
        cx.run_until_parked();
        cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1600.)));
        view.update(cx, |this, cx| {
            this.model
                .apply_statuses(native("active", 4, &["cal-a"], granted(&["cal-a"])))
                .unwrap();
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let checkbox = cx.debug_bounds("system-calendar-0").unwrap();
        cx.simulate_click(checkbox.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |this, _| {
            assert!(this.model.selection[&NATIVE].ids.is_empty());
            assert!(this.model.selection_changed(NATIVE));
            let (args, stdin) = this.model.native_selection(NATIVE).unwrap();
            assert_eq!(args, NATIVE_SELECTION_ARGS.map(OsString::from));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&stdin).unwrap(),
                serde_json::json!({"calendar_ids":[]})
            );
            assert_eq!(
                this.model.statuses[&NATIVE].state.native_calendar_ids,
                ["cal-a"]
            );
        });

        // A successful empty catalog allows selection but never invents calendars.
        view.update(cx, |this, cx| {
            this.model
                .apply_statuses(native("active", 5, &[], granted(&[])))
                .unwrap();
            assert!(this.model.statuses[&NATIVE].native_state().can_select());
            assert!(this.model.selection[&NATIVE].ids.is_empty());
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));

        // Permission loss removes the draft without changing the saved selection.
        let mut report = granted(&["cal-a"]);
        report["authorization"] = serde_json::json!("denied");
        view.update(cx, |this, cx| {
            this.model
                .apply_statuses(native("active", 6, &["cal-a"], report))
                .unwrap();
            assert_eq!(
                this.model.statuses[&NATIVE].native_state(),
                NativeState::Denied
            );
            assert!(this.model.native_selection(NATIVE).is_err());
            assert_eq!(
                this.model.statuses[&NATIVE].state.native_calendar_ids,
                ["cal-a"]
            );
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    /// CL02: "Use selected calendars" (the same view in Settings and the setup
    /// assistant) starts exactly one child whose argv names no calendar; the
    /// checked IDs, address-like ones included, reach it only as its stdin spec.
    #[gpui::test]
    fn native_selection_child_carries_calendar_ids_only_on_stdin(cx: &mut gpui::TestAppContext) {
        use crate::test_support::{ps_path, ps_str, write_cli};
        use broscribe_core::Library;
        use broscribe_platform::{fakes::FakePaths, resolve_paths};

        let address = "synthetic.owner@example.invalid";
        let dir = tempfile::tempdir().unwrap();
        let status_path = dir.path().join("status.json");
        std::fs::write(
            &status_path,
            serde_json::to_vec(&native("active", 4, &[], granted(&[address, "cal-b"]))).unwrap(),
        )
        .unwrap();
        let log = dir.path().join("argv");
        let spec = dir.path().join("spec.json");
        let unexpected = r#"{"error":{"code":"unexpected","message":"unexpected command"}}"#;
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{log}'\ncase \"$*\" in\n*\"integrations setup system-calendar --spec-stdin\") cat > '{spec}'; cat '{status}';;\n*\"integrations status\") cat '{status}';;\n*) printf '%s\\n' '{unexpected}'; exit 1;;\nesac\n",
                log = log.display(),
                spec = spec.display(),
                status = status_path.display()
            ),
            &format!(
                "fx_log_args {log};\
                 if($all -clike '*integrations setup system-calendar --spec-stdin'){{fx_save_stdin {spec};fx_cat {status}}}\
                 elseif($all -clike '*integrations status'){{fx_cat {status}}}\
                 else{{fx_echo {unexpected};exit 1}}",
                log = ps_path(&log),
                spec = ps_path(&spec),
                status = ps_path(&status_path),
                unexpected = ps_str(unexpected),
            ),
        );
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Default::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    cli,
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let (view, cx) =
            cx.add_window_view(move |_, cx| IntegrationsView::with_native_support(state, true, cx));
        cx.run_until_parked();
        cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1600.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        view.read_with(cx, |this, _| {
            assert!(this.model.selection[&NATIVE].ids.is_empty());
        });
        // Check both offered calendars, then save.
        for checkbox in ["system-calendar-0", "system-calendar-1"] {
            let bounds = cx.debug_bounds(checkbox).unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        let save = cx.debug_bounds("system-calendar-configure").unwrap();
        cx.simulate_click(save.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let argv = std::fs::read_to_string(&log).unwrap();
        let setups: Vec<&str> = argv
            .lines()
            .filter(|line| line.contains("integrations setup"))
            .collect();
        assert_eq!(setups.len(), 1, "{argv}");
        assert!(
            setups[0].ends_with("integrations setup system-calendar --spec-stdin"),
            "{argv}"
        );
        assert!(
            !argv.contains(address) && !argv.contains("cal-b") && !argv.contains("--calendar-id"),
            "{argv}"
        );
        let sent: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&spec).unwrap()).unwrap();
        assert_eq!(sent, serde_json::json!({"calendar_ids":["cal-b", address]}));
        view.read_with(cx, |this, _| {
            assert!(!this.model.busy(NATIVE));
            let message = this.model.message(&Card::provider(NATIVE)).unwrap();
            assert!(!message.error, "{}", message.text);
        });
    }

    const GRAPH: IntegrationId = IntegrationId::MicrosoftGraph;
    const HANDLE: &str = "11111111111111111111111111111111";

    fn graph_account(generation: u64, selection_generation: u64) -> serde_json::Value {
        serde_json::json!({
            "account":{"integration":"microsoft-graph","account_id":HANDLE},
            "identity":{"authority_host":"login.microsoftonline.com","resource_tenant_id":"synthetic-tenant",
                "object_id":HANDLE,"client_id":"synthetic-client","verified_addresses":[]},
            "generation":generation,"selection_generation":selection_generation,"status":"active",
            "granted_scopes":["User.Read","Calendars.Read"],
            "selected_calendars":[{"kind":"own","calendar_id":"cal-a"}],
            "calendar":"available","directory":"not_requested","shared_calendars":"not_requested",
            "microsoft365_groups":"not_requested","target_results":[]
        })
    }

    fn graph_status(account: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"integrations":[{
            "integration":"microsoft-graph","configured":true,"state":{"status":"signed_out"},
            "microsoft_accounts":[account]
        }]})
    }

    fn account_ref() -> AccountRef {
        serde_json::from_value(
            serde_json::json!({"integration":"microsoft-graph","account_id":HANDLE}),
        )
        .unwrap()
    }

    fn refused(code: &str) -> Result<Value, CliError> {
        Err(CliError::answered(code, format!("synthetic {code}")))
    }

    #[test]
    fn providers_run_independently_and_only_the_current_action_reports() {
        let mut model = IntegrationsModel::default();
        let google = model
            .begin(Operation::new(
                Card::provider(IntegrationId::Google),
                Action::Sync,
            ))
            .unwrap();
        // Another provider is never blocked by Google's action.
        let native = model
            .begin(Operation::new(Card::provider(NATIVE), Action::Connect))
            .unwrap();
        assert!(model.busy(IntegrationId::Google) && model.busy(NATIVE));
        assert!(!model.busy(GRAPH));
        // The same provider runs one action at a time.
        assert!(
            model
                .begin(Operation::new(
                    Card::provider(IntegrationId::Google),
                    Action::Catalog
                ))
                .is_none()
        );
        // A result with another ticket is ignored.
        assert!(model.finish(IntegrationId::Google, native).is_none());
        assert!(model.busy(IntegrationId::Google));
        let operation = model.finish(IntegrationId::Google, google).unwrap();
        assert_eq!(operation.action, Action::Sync);
        assert!(!model.busy(IntegrationId::Google) && model.busy(NATIVE));
    }

    #[test]
    fn sign_ins_are_cancelled_once_and_report_on_their_own_card() {
        let mut model = IntegrationsModel::default();
        let card = Card::account(GRAPH, account_ref());
        let attempt = LoginAttemptId::parse("synthetic-reconnect").unwrap();
        let ticket = model
            .begin(Operation::new(card.clone(), Action::SignIn).signing_in(attempt.clone()))
            .unwrap();
        model
            .begin(Operation::new(Card::provider(NATIVE), Action::Connect))
            .unwrap();
        // Leaving the page cancels the sign-in (and only sign-ins), once.
        let requests = model.cancel_all_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, GRAPH);
        assert_eq!(
            requests[0].1,
            IntegrationsModel::cancel_args(GRAPH, &attempt)
        );
        assert!(model.cancel_all_requests().is_empty());
        assert!(model.cancel_request(GRAPH).is_none());
        // A failed cancellation is reported on that card and may be retried.
        model.apply_cancel_result(GRAPH, ticket, refused("busy"));
        assert!(model.message(&card).is_some_and(|m| m.error));
        assert!(model.cancel_request(GRAPH).is_some());
        // The sign-in itself then ends as cancelled, on its own card only.
        let operation = model.finish(GRAPH, ticket).unwrap();
        let followup = model.apply_result(&operation, refused("login_cancelled"));
        assert!(!followup.reload);
        let message = model.message(&card).unwrap();
        assert!(message.error && message.text.contains("Nothing from it was saved"));
        assert!(model.message(&Card::provider(GRAPH)).is_none());
        // A late result of a finished action changes nothing.
        model.apply_cancel_result(GRAPH, ticket, refused("busy"));
        assert!(
            model
                .message(&card)
                .unwrap()
                .text
                .contains("Nothing from it was saved")
        );
    }

    const SECOND: &str = "22222222222222222222222222222222";

    /// `graph_account` for the account with handle `SECOND`.
    fn second_account() -> serde_json::Value {
        let mut account = graph_account(3, 2);
        account["account"]["account_id"] = json!(SECOND);
        account["identity"]["object_id"] = json!(SECOND);
        account
    }

    fn second_ref() -> AccountRef {
        serde_json::from_value(json!({"integration":"microsoft-graph","account_id":SECOND}))
            .unwrap()
    }

    /// SET-06: one account's outcome stays on its card while another account of the
    /// same provider runs and finishes an action; the provider still runs one action
    /// at a time, and a removed account's outcome goes with its card.
    #[test]
    fn an_action_on_one_account_keeps_another_accounts_outcome() {
        let mut model = IntegrationsModel::default();
        let mut status = graph_status(graph_account(3, 2));
        status["integrations"][0]["microsoft_accounts"]
            .as_array_mut()
            .unwrap()
            .push(second_account());
        model.apply_statuses(status).unwrap();
        let first = Card::account(GRAPH, account_ref());
        let second = Card::account(GRAPH, second_ref());

        let ticket = model
            .begin(Operation::new(first.clone(), Action::Sync))
            .unwrap();
        let operation = model.finish(GRAPH, ticket).unwrap();
        model.apply_result(&operation, refused("forbidden"));
        let alert = model.message(&first).unwrap().clone();
        assert!(alert.error);

        let ticket = model
            .begin(Operation::new(second.clone(), Action::Sync))
            .unwrap();
        // The lock still covers the whole provider: neither account nor the provider
        // card starts another action meanwhile.
        assert!(
            model
                .begin(Operation::new(first.clone(), Action::Catalog))
                .is_none()
        );
        assert!(
            model
                .begin(Operation::new(Card::provider(GRAPH), Action::SignIn))
                .is_none()
        );
        assert_eq!(model.message(&first), Some(&alert));
        let operation = model.finish(GRAPH, ticket).unwrap();
        model.apply_result(&operation, Ok(json!({})));
        assert_eq!(model.message(&first), Some(&alert));
        let done = model.message(&second).unwrap();
        assert!(!done.error && done.text == "Calendars refreshed.");

        // Starting again on the first account clears only its own outcome.
        let ticket = model
            .begin(Operation::new(first.clone(), Action::Sync))
            .unwrap();
        assert!(model.message(&first).is_none());
        assert!(model.message(&second).is_some());
        model.finish(GRAPH, ticket).unwrap();

        // The second account is removed: its outcome goes with its card.
        model
            .apply_statuses(graph_status(graph_account(3, 2)))
            .unwrap();
        assert!(model.message(&second).is_none());
    }

    /// SET-06, drawn: account A's alert stays on A's line while account B of
    /// the same provider runs and finishes an action on its own line.
    #[gpui::test]
    fn one_accounts_alert_stays_drawn_while_another_account_acts(cx: &mut gpui::TestAppContext) {
        use crate::ui::spoken_log::{self, assert_distinct, node};
        use broscribe_core::Library;
        use broscribe_platform::{fakes::FakePaths, resolve_paths};

        let dir = tempfile::tempdir().unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Default::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let (view, cx) =
            cx.add_window_view(move |_, cx| IntegrationsView::with_native_support(state, true, cx));
        cx.run_until_parked();
        let named = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                view.update(cx, |this, cx| {
                    spoken_log::while_building(|| Render::render(this, window, cx)).1
                })
            })
        };
        let first = format!("calendar-status-microsoft-graph-{HANDLE}");
        let second = format!("calendar-status-microsoft-graph-{SECOND}");

        view.update(cx, |this, cx| {
            this.model.status_error = None;
            this.advanced = true;
            let mut status = graph_status(graph_account(3, 2));
            status["integrations"][0]["microsoft_accounts"]
                .as_array_mut()
                .unwrap()
                .push(second_account());
            this.model.apply_statuses(status).unwrap();
            let ticket = this
                .model
                .begin(Operation::new(
                    Card::account(GRAPH, account_ref()),
                    Action::Sync,
                ))
                .unwrap();
            let operation = this.model.finish(GRAPH, ticket).unwrap();
            this.model.apply_result(&operation, refused("forbidden"));
            this.model
                .begin(Operation::new(
                    Card::account(GRAPH, second_ref()),
                    Action::Sync,
                ))
                .unwrap();
            cx.notify();
        });
        // The fields cards repeat word for word: two integration cards'
        // state, and the two account cards' details.
        let rows = [
            "calendar-*-state",
            "calendar-microsoft-graph-*-identity",
            "calendar-microsoft-graph-*-routes",
        ];
        let alert = (Role::Alert, "Synthetic forbidden.");
        let running = named(cx);
        assert_distinct(&running, &rows);
        assert_eq!(node(&running, &first), alert);
        assert_eq!(
            node(&running, &second),
            (Role::Status, "Refreshing calendars…")
        );

        view.update(cx, |this, cx| {
            let operation = this.model.operations.remove(&GRAPH).unwrap();
            this.model.apply_result(&operation, Ok(json!({})));
            cx.notify();
        });
        let done = named(cx);
        assert_distinct(&done, &rows);
        assert_eq!(node(&done, &first), alert);
        assert_eq!(node(&done, &second), (Role::Status, "Calendars refreshed."));
    }

    #[test]
    fn a_microsoft_catalog_stays_usable_after_a_new_selection_generation() {
        let mut model = IntegrationsModel::default();
        model
            .apply_statuses(graph_status(graph_account(4, 7)))
            .unwrap();
        let operation = Operation::new(Card::account(GRAPH, account_ref()), Action::Catalog);
        let catalog = serde_json::json!({
            "account":account_ref(),"generation":4,"selection_generation":7,"groups":"unknown",
            "calendars":[{"target":{"kind":"own","calendar_id":"cal-b"},"name":"Synthetic",
                "owner_name":null,"owner_address":null,"is_default":false,"can_edit":false,
                "access":"available"}],
            "groups_error":null
        });
        model.apply_result(&operation, Ok(catalog));
        assert!(model.catalogs.contains_key(&account_ref()));
        // Our own selection advances only the selection generation.
        let mut selected = graph_account(4, 8);
        selected["selected_calendars"] = serde_json::json!([
            {"kind":"own","calendar_id":"cal-a"},{"kind":"own","calendar_id":"cal-b"}
        ]);
        let operation = Operation::new(Card::account(GRAPH, account_ref()), Action::Configure);
        model.apply_result(&operation, Ok(selected.clone()));
        let current = &model.statuses[&GRAPH].microsoft_accounts[0];
        assert_eq!(current.selection_generation, 8);
        assert_eq!(model.catalogs[&account_ref()].selection_generation, 8);
        // The next selection names the new generation; targets travel on stdin.
        let (args, stdin) =
            IntegrationsModel::remote_selection(GRAPH, current, &current.selected_calendars)
                .unwrap();
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|w| w == ["--selection-generation", "8"])
        );
        assert!(args.iter().all(|a| !a.contains("cal-b")));
        assert!(args.contains(&"--spec-stdin".to_owned()));
        let spec: serde_json::Value = serde_json::from_slice(&stdin).unwrap();
        assert_eq!(spec["targets"][1]["calendar_id"], "cal-b");
        // A status read after the selection keeps the catalog...
        model.apply_statuses(graph_status(selected)).unwrap();
        assert!(model.catalogs.contains_key(&account_ref()));
        // ...a reconnected account (new generation) drops it.
        model
            .apply_statuses(graph_status(graph_account(5, 8)))
            .unwrap();
        assert!(model.catalogs.is_empty());
    }

    fn google_status(
        generation: u64,
        selection_generation: u64,
        selected: &[&str],
    ) -> serde_json::Value {
        serde_json::json!({
            "integration":"google","configured":true,
            "state":{"generation":generation,"selection_generation":selection_generation,
                "status":"active","native_calendar_ids":selected}
        })
    }

    fn stamp(generation: u64, selection_generation: u64) -> GoogleCalendarStamp {
        GoogleCalendarStamp {
            generation,
            selection_generation,
        }
    }

    #[test]
    fn google_selection_is_explicit_and_keeps_its_catalog_across_its_own_generation() {
        let mut model = IntegrationsModel::default();
        model
            .apply_statuses(serde_json::json!({"integrations":[google_status(3, 1, &[])]}))
            .unwrap();
        assert!(model.google_selection("work", true).is_err());
        let card = Card::provider(IntegrationId::Google);
        model.apply_result(
            &Operation::new(card.clone(), Action::Catalog),
            Ok(serde_json::json!({"generation":3,"selection_generation":1,"selected":[],"calendars":[
                {"id":"primary-id","name":"Synthetic primary","primary":true,"access_role":"owner"},
                {"id":"work","name":"Synthetic work","primary":false,"access_role":"reader"}
            ]})),
        );
        assert!(model.google_catalog.is_some());
        // Nothing is selected for the user: only the checked calendar, under both of
        // the catalog's generations.
        let (args, stdin, authority) = model.google_selection("work", true).unwrap();
        assert_eq!(authority, stamp(3, 1));
        assert_eq!(
            args,
            [
                "integrations",
                "select-calendars",
                "google",
                "--generation",
                "3",
                "--selection-generation",
                "1",
                "--spec-stdin"
            ]
            .map(OsString::from)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdin).unwrap(),
            serde_json::json!({"calendar_ids":["work"]})
        );
        assert!(model.google_selection("elsewhere", true).is_err());
        // The saved selection advanced only the selection generation; the catalog
        // follows it and the account generation stays.
        model.apply_result(
            &Operation::new(card.clone(), Action::Configure).expecting(stamp(3, 1)),
            Ok(google_status(3, 2, &["work"])),
        );
        let catalog = model.google_catalog.as_ref().unwrap();
        assert_eq!(
            (catalog.authority, catalog.selected.clone()),
            (stamp(3, 2), vec!["work".to_owned()])
        );
        model
            .apply_statuses(serde_json::json!({"integrations":[google_status(3, 2, &["work"])]}))
            .unwrap();
        let (_, stdin, authority) = model.google_selection("work", false).unwrap();
        assert_eq!(authority, stamp(3, 2));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdin).unwrap(),
            serde_json::json!({"calendar_ids":[]})
        );
        // A result of a selection made from another catalog view leaves this one as it
        // is; the next status read brings the newer saved selection instead.
        model.apply_result(
            &Operation::new(card.clone(), Action::Configure).expecting(stamp(3, 1)),
            Ok(google_status(3, 3, &["primary-id"])),
        );
        assert_eq!(
            model.google_catalog.as_ref().unwrap().authority,
            stamp(3, 2)
        );
        model
            .apply_statuses(
                serde_json::json!({"integrations":[google_status(3, 3, &["primary-id"])]}),
            )
            .unwrap();
        let catalog = model.google_catalog.as_ref().unwrap();
        assert_eq!(
            (catalog.authority, catalog.selected.clone()),
            (stamp(3, 3), vec!["primary-id".to_owned()])
        );
        // A late status read from before does not roll the selection back.
        model
            .apply_statuses(serde_json::json!({"integrations":[google_status(3, 2, &["work"])]}))
            .unwrap();
        let (_, stdin, authority) = model.google_selection("work", true).unwrap();
        assert_eq!(authority, stamp(3, 3));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdin).unwrap(),
            serde_json::json!({"calendar_ids":["primary-id","work"]})
        );
        // A sign-in replaced elsewhere drops the catalog.
        model
            .apply_statuses(serde_json::json!({"integrations":[google_status(6, 0, &[])]}))
            .unwrap();
        assert!(model.google_catalog.is_none());
    }

    #[test]
    fn a_saved_change_the_agent_did_not_hear_of_is_still_reported_saved() {
        let mut model = IntegrationsModel::default();
        let card = Card::provider(IntegrationId::Google);
        let followup = model.apply_result(
            &Operation::new(card.clone(), Action::SignOut),
            Ok(serde_json::json!({"integration":"google","signed_out":true,"agent_notified":false})),
        );
        assert!(followup.reload && followup.people_changed);
        let message = model.message(&card).unwrap();
        assert!(!message.error);
        assert!(message.text.starts_with(Action::SignOut.notice()));
        assert!(message.text.contains("not notified"));
    }

    #[test]
    fn route_reads_say_when_each_route_last_succeeded_and_what_is_kept() {
        let read = |copy: &str, last: Option<&str>, events: usize| -> CalendarRouteRead {
            serde_json::from_value(serde_json::json!({
                "target":{"kind":"own","calendar_id":"cal-a"},
                "last_success":last,"copy":copy,"cached_events":events
            }))
            .unwrap()
        };
        let retained = route_read_label(&read("retained", Some("2026-11-17T09:00:00Z"), 3));
        assert!(retained.contains("latest read failed") && retained.contains("3 event"));
        let when = spoken_date_time(
            &chrono::DateTime::parse_from_rfc3339("2026-11-17T09:00:00Z").unwrap(),
        );
        assert!(retained.contains(&format!("last read {when}")));
        assert!(
            route_read_label(&read("fresh", Some("2026-11-17T09:00:00Z"), 2))
                .contains("2 events from the last check")
        );
        assert!(route_read_label(&read("fresh", None, 1)).contains("1 event from the last check"));
        let empty = route_read_label(&read("empty", None, 0));
        assert!(empty.contains("never read successfully") && empty.contains("nothing cached"));
    }

    #[test]
    fn invalid_settings_of_one_integration_are_reported_on_it() {
        let mut model = IntegrationsModel::default();
        model
            .apply_statuses(serde_json::json!({"integrations":[
                {"integration":"google","configured":false,"state":{"status":"signed_out"},
                    "config_error":"invalid [integration.google] settings: synthetic"},
                {"integration":"microsoft-graph","configured":true,"state":{"status":"signed_out"}}
            ]}))
            .unwrap();
        assert!(
            model.statuses[&IntegrationId::Google]
                .config_error
                .as_deref()
                .is_some_and(|e| e.contains("integration.google"))
        );
        assert!(model.statuses[&GRAPH].config_error.is_none());
        assert!(model.statuses[&GRAPH].configured);
    }

    /// IN22: a saved Google calendar the catalog no longer lists has no checkbox, so it
    /// never blocks a new selection made from the catalog.
    #[test]
    fn a_google_calendar_that_left_the_catalog_is_left_out_of_the_next_selection() {
        let mut model = IntegrationsModel::default();
        model
            .apply_statuses(
                serde_json::json!({"integrations":[google_status(3, 1, &["work", "gone"])]}),
            )
            .unwrap();
        model.apply_result(
            &Operation::new(Card::provider(IntegrationId::Google), Action::Catalog),
            Ok(
                serde_json::json!({"generation":3,"selection_generation":1,"selected":["work","gone"],"calendars":[
                    {"id":"work","name":"Synthetic work","primary":false,"access_role":"reader"},
                    {"id":"team","name":"Synthetic team","primary":false,"access_role":"reader"}
                ]}),
            ),
        );
        let spec = |calendar: &str, checked: bool| {
            let (_, stdin, _) = model.google_selection(calendar, checked).unwrap();
            serde_json::from_slice::<serde_json::Value>(&stdin).unwrap()
        };
        assert_eq!(spec("work", false), serde_json::json!({"calendar_ids":[]}));
        assert_eq!(
            spec("team", true),
            serde_json::json!({"calendar_ids":["work","team"]})
        );
        assert!(model.google_selection("gone", false).is_err());
    }

    /// A committed route removal whose cached copy could not be carried over keeps the
    /// new selection and says what was lost, without reporting a failure.
    #[test]
    fn a_route_removal_with_a_lost_copy_keeps_the_selection_and_warns() {
        let mut model = IntegrationsModel::default();
        model
            .apply_statuses(graph_status(graph_account(4, 7)))
            .unwrap();
        let mut removed = graph_account(4, 8);
        removed["selected_calendars"] = serde_json::json!([]);
        removed["copy_error"] = serde_json::json!({
            "code":"io","message":"BroScribe could not read or write its private integration data"
        });
        let card = Card::account(GRAPH, account_ref());
        let followup = model.apply_result(
            &Operation::new(card.clone(), Action::RemoveRoute),
            Ok(removed),
        );
        assert!(followup.reload);
        let current = &model.statuses[&GRAPH].microsoft_accounts[0];
        assert_eq!(current.selection_generation, 8);
        assert!(current.selected_calendars.is_empty());
        let message = model.message(&card).unwrap();
        assert!(!message.error);
        // The cause in words, without its code (WRT-04).
        assert!(
            message.text.contains(
                "couldn't be kept. BroScribe could not read or write its private integration data."
            ),
            "{}",
            message.text
        );
        assert!(!message.text.contains("(io)"), "{}", message.text);
        // Without a carry-over failure the ordinary notice stands.
        model.apply_result(
            &Operation::new(card.clone(), Action::RemoveRoute),
            Ok(graph_account(4, 9)),
        );
        assert_eq!(
            model.message(&card).unwrap().text,
            Action::RemoveRoute.notice()
        );
    }

    /// IN25: a page its owner keeps but stops showing (the setup assistant leaving its
    /// Calendars step) cancels the running sign-in durably, once, under its own attempt,
    /// and the sign-in's terminal result still reports on its card.
    #[gpui::test]
    fn deactivating_the_page_cancels_its_sign_in_once_through_the_attempt_journal(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::{ps_path, ps_str, write_cli};
        use broscribe_core::Library;
        use broscribe_platform::{fakes::FakePaths, resolve_paths};

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("argv");
        let cancelled = r#"{"state":"cancelled"}"#;
        let refused = r#"{"error":{"code":"login_cancelled","message":"sign-in was cancelled"}}"#;
        let status = r#"{"integrations":[{"integration":"google","configured":true,"state":{"status":"signed_out"}}]}"#;
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n*\"cancel-login google --attempt\"*) printf '%s\\n' '{cancelled}';;\n*\"login google --attempt\"*) printf '%s\\n' '{refused}'; exit 1;;\n*\"integrations status\"*) printf '%s\\n' '{status}';;\n*) exit 1;;\nesac\n",
                log.display()
            ),
            &format!(
                "fx_log_args {log};\
                 if($all -clike '*cancel-login google --attempt*'){{fx_echo {cancelled}}}\
                 elseif($all -clike '*login google --attempt*'){{fx_echo {refused};exit 1}}\
                 elseif($all -clike '*integrations status*'){{fx_echo {status}}}\
                 else{{exit 1}}",
                log = ps_path(&log),
                cancelled = ps_str(cancelled),
                refused = ps_str(refused),
                status = ps_str(status),
            ),
        );
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Default::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    cli,
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let (view, cx) =
            cx.add_window_view(move |_, cx| IntegrationsView::with_native_support(state, true, cx));
        cx.run_until_parked();
        let google = IntegrationId::Google;
        let attempt = view.update(cx, |this, cx| {
            this.advanced = true;
            this.sign_in(
                Card::provider(google),
                |attempt| IntegrationsModel::login_args(google, attempt),
                cx,
            );
            let attempt = this.model.operations[&google].attempt.clone().unwrap();
            this.deactivate(cx);
            assert!(this.model.operations[&google].cancelling);
            // A second deactivation sends nothing more.
            this.deactivate(cx);
            attempt
        });
        cx.run_until_parked();
        let argv = std::fs::read_to_string(&log).unwrap();
        let cancel = format!("cancel-login google --attempt {}", attempt.as_str());
        assert_eq!(argv.matches(&cancel).count(), 1, "{argv}");
        let login = format!("login google --attempt {}", attempt.as_str());
        assert!(
            argv.lines()
                .any(|line| !line.contains("cancel-login") && line.contains(&login)),
            "{argv}"
        );
        view.read_with(cx, |this, _| {
            assert!(!this.model.busy(google));
            assert!(this.advanced, "the card with the outcome stays expanded");
            let message = this.model.message(&Card::provider(google)).unwrap();
            assert!(message.error && message.text.contains("Nothing from it was saved"));
        });
    }

    fn google_secret_status(status: &str, stored: Option<bool>) -> serde_json::Value {
        let mut google = serde_json::json!({
            "integration":"google","configured":true,
            "state":{"generation":2,"status":status}
        });
        if let Some(stored) = stored {
            google["client_secret_stored"] = serde_json::json!(stored);
        }
        serde_json::json!({ "integrations": [google] })
    }

    #[test]
    fn the_client_secret_control_follows_the_stored_secret_and_sign_in() {
        let control = |status: &str, stored: Option<bool>| {
            let mut model = IntegrationsModel::default();
            model
                .apply_statuses(google_secret_status(status, stored))
                .unwrap();
            model.statuses[&IntegrationId::Google].secret_control()
        };
        assert_eq!(
            control("signed_out", Some(false)),
            Some(SecretControl::Save)
        );
        assert_eq!(control("active", Some(false)), Some(SecretControl::Save));
        assert_eq!(
            control("signed_out", Some(true)),
            Some(SecretControl::Remove)
        );
        for status in ["active", "reauth_required"] {
            assert_eq!(
                control(status, Some(true)),
                Some(SecretControl::SignOutFirst)
            );
        }
        // An older CLI without the field offers nothing rather than guessing.
        assert_eq!(control("signed_out", None), None);

        let secret = "synthetic-client-secret";
        assert!(IntegrationsModel::save_secret_command(" ").is_err());
        let (args, stdin) = IntegrationsModel::save_secret_command(secret).unwrap();
        assert_eq!(
            args,
            ["integrations", "setup", "google", "--client-secret-stdin"]
                .map(OsString::from)
                .to_vec()
        );
        assert_eq!(stdin, secret.as_bytes());
        assert_eq!(
            IntegrationsModel::clear_secret_args(),
            ["integrations", "setup", "google", "--clear-client-secret"]
                .map(OsString::from)
                .to_vec()
        );
    }

    #[test]
    fn a_revoking_sign_out_needs_confirmation_and_reports_what_google_answered() {
        let mut model = IntegrationsModel::default();
        model
            .apply_statuses(google_secret_status("signed_out", Some(true)))
            .unwrap();
        assert!(model.prepare_google_revoke().is_err());
        assert!(model.confirm_google_revoke_args().is_err());
        model
            .apply_statuses(google_secret_status("reauth_required", Some(true)))
            .unwrap();
        model.prepare_google_revoke().unwrap();
        model.cancel_google_revoke();
        assert!(model.confirm_google_revoke_args().is_err());
        model.prepare_google_revoke().unwrap();
        assert_eq!(
            model.confirm_google_revoke_args().unwrap(),
            ["integrations", "logout", "google", "--revoke-at-google"]
                .map(OsString::from)
                .to_vec()
        );
        // Single use.
        assert!(model.confirm_google_revoke_args().is_err());
        // A status that reads signed out closes an open confirmation.
        model.prepare_google_revoke().unwrap();
        model
            .apply_statuses(google_secret_status("signed_out", Some(true)))
            .unwrap();
        assert!(!model.google_revoke);

        let operation =
            Operation::new(Card::provider(IntegrationId::Google), Action::SignOutRevoke);
        for (outcome, error, text) in [
            (
                json!({"outcome":"revocation_requested"}),
                false,
                "may take a while",
            ),
            (
                json!({"outcome":"no_stored_grant"}),
                false,
                "nothing was sent",
            ),
            (
                json!({"outcome":"failed","code":"transient"}),
                true,
                "failed (transient)",
            ),
        ] {
            let mut model = IntegrationsModel::default();
            model.apply_result(
                &operation,
                Ok(json!({"integration":"google","signed_out":true,"remote_revocation":outcome})),
            );
            let message = model.message(&operation.card).unwrap();
            assert_eq!(message.error, error, "{}", message.text);
            assert!(message.text.contains(text), "{}", message.text);
            if error {
                assert!(message.text.contains(GOOGLE_PERMISSIONS));
            }
        }
        let removed = Operation::new(Card::provider(IntegrationId::Google), Action::RemoveSecret);
        assert!(
            IntegrationsModel::notice_for(removed.action, &json!({"client_secret_removed":true}))
                .starts_with("Client secret removed")
        );
        assert!(
            IntegrationsModel::notice_for(removed.action, &json!({"client_secret_removed":false}))
                .contains("nothing changed")
        );
    }

    #[gpui::test]
    fn google_secret_and_revocation_controls_run_only_their_confirmed_commands(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_support::{ps_path, ps_str, write_cli};
        use broscribe_core::Library;
        use broscribe_platform::{fakes::FakePaths, resolve_paths};

        let dir = tempfile::tempdir().unwrap();
        let status_path = dir.path().join("status.json");
        let write_status = |status: &str, stored: bool| {
            std::fs::write(
                &status_path,
                serde_json::to_vec(&google_secret_status(status, Some(stored))).unwrap(),
            )
            .unwrap();
        };
        write_status("active", true);
        let marker = dir.path().join("mutations");
        let revoked = r#"{"integration":"google","signed_out":true,"remote_revocation":{"outcome":"failed","code":"transient"}}"#;
        let cleared = r#"{"integration":"google","client_secret_removed":true}"#;
        let unexpected = r#"{"error":{"code":"unexpected_action","message":"Wrong command"}}"#;
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "case \"$*\" in\n*\"logout google --revoke-at-google\"*) printf '%s\\n' revoke >> '{marker}'; printf '%s\\n' '{revoked}';;\n*\"setup google --clear-client-secret\"*) printf '%s\\n' clear >> '{marker}'; printf '%s\\n' '{cleared}';;\n*\"integrations status\"*) cat '{status}';;\n*) printf '%s\\n' unexpected >> '{marker}'; printf '%s\\n' '{unexpected}'; exit 1;;\nesac\n",
                marker = marker.display(),
                status = status_path.display(),
            ),
            &format!(
                "if($all -clike '*logout google --revoke-at-google*'){{fx_append {marker} ('revoke'+[char]10);fx_echo {revoked}}}\
                 elseif($all -clike '*setup google --clear-client-secret*'){{fx_append {marker} ('clear'+[char]10);fx_echo {cleared}}}\
                 elseif($all -clike '*integrations status*'){{fx_cat {status}}}\
                 else{{fx_append {marker} ('unexpected'+[char]10);fx_echo {unexpected};exit 1}}",
                marker = ps_path(&marker),
                status = ps_path(&status_path),
                revoked = ps_str(revoked),
                cleared = ps_str(cleared),
                unexpected = ps_str(unexpected),
            ),
        );
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Default::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    cli,
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let mut view = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let integration = cx.new(|cx| IntegrationsView::with_native_support(state, true, cx));
            view = Some(integration.clone());
            gpui::component::Root::new(integration, window, cx)
        });
        let view = view.unwrap();
        cx.run_until_parked();
        view.update(cx, |this, cx| {
            this.advanced = true;
            cx.notify();
        });
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(2400.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        // Signed in: the secret cannot be removed, nor saved again.
        assert!(
            cx.debug_bounds("integration-remove-secret-google")
                .is_none()
        );
        assert!(cx.debug_bounds("integration-save-secret-google").is_none());

        let open = cx.debug_bounds("integration-revoke-open-google").unwrap();
        cx.simulate_click(open.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let cancel = cx.debug_bounds("integration-revoke-cancel-google").unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(!marker.exists(), "Cancel sends nothing");
        view.read_with(cx, |this, _| assert!(!this.model.google_revoke));

        cx.update(|window, cx| window.draw(cx).clear(cx));
        let open = cx.debug_bounds("integration-revoke-open-google").unwrap();
        cx.simulate_click(open.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        // The reload after the sign-out reads it signed out.
        write_status("signed_out", true);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let confirm = cx
            .debug_bounds("integration-revoke-confirm-google")
            .unwrap();
        cx.simulate_click(confirm.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "revoke\n");
        view.read_with(cx, |this, _| {
            let message = this
                .model
                .message(&Card::provider(IntegrationId::Google))
                .unwrap();
            assert!(message.error && message.text.contains("failed (transient)"));
            assert_eq!(
                this.model.statuses[&IntegrationId::Google].secret_control(),
                Some(SecretControl::Remove)
            );
        });

        write_status("signed_out", false);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let remove = cx.debug_bounds("integration-remove-secret-google").unwrap();
        cx.simulate_click(remove.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "revoke\nclear\n");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            cx.debug_bounds("integration-remove-secret-google")
                .is_none()
        );
        // Missing: it can be saved again, and an empty secret runs nothing.
        let save = cx.debug_bounds("integration-save-secret-google").unwrap();
        cx.simulate_click(save.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "revoke\nclear\n");
        view.read_with(cx, |this, _| {
            let message = this
                .model
                .message(&Card::provider(IntegrationId::Google))
                .unwrap();
            assert!(message.error && message.text.contains("client secret"));
        });
    }

    /// SET-05 and SET-06: every Calendars text is its own named node with a stable ID.
    /// A card's wait is a Pending line and its outcome Done (a status) or Failed (an
    /// alert) on `calendar-status-<integration>[-<account>]`, in words, never red alone;
    /// each card shows only the outcome of the action it started.
    #[gpui::test]
    fn calendar_texts_are_named_and_outcomes_carry_a_role(cx: &mut gpui::TestAppContext) {
        use crate::ui::spoken_log::{self, assert_distinct, has, node};
        use broscribe_core::Library;
        use broscribe_platform::{fakes::FakePaths, resolve_paths};

        let dir = tempfile::tempdir().unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Default::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let (view, cx) =
            cx.add_window_view(move |_, cx| IntegrationsView::with_native_support(state, true, cx));
        cx.run_until_parked();
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(6000.)));
        let named = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                view.update(cx, |this, cx| {
                    spoken_log::while_building(|| Render::render(this, window, cx)).1
                })
            })
        };

        view.update(cx, |this, cx| {
            this.native_supported = false;
            cx.notify();
        });
        let direct_only = named(cx);
        assert!(
            !direct_only
                .iter()
                .any(|(id, _, _)| id.starts_with("calendar-system-calendar-"))
        );
        assert!(has(&direct_only, "calendars-heading"));
        view.update(cx, |this, cx| {
            this.native_supported = true;
            cx.notify();
        });

        // The one field that repeats word for word across cards.
        let rows = ["calendar-*-capabilities"];

        // No CLI answered the status read: the page says so in one alert.
        let unread = named(cx);
        assert_distinct(&unread, &rows);
        let (role, text) = node(&unread, "calendars-status-error");
        assert_eq!(role, Role::Alert);
        assert!(
            text.starts_with("Couldn't read the calendar status. "),
            "{text}"
        );
        assert_eq!(
            node(&unread, "calendars-heading"),
            (Role::Heading, "Calendars")
        );
        assert_eq!(node(&unread, "calendars-intro").0, Role::Label);
        assert_eq!(
            node(&unread, "calendar-system-calendar-heading"),
            (Role::Heading, "Calendars on this Mac")
        );
        assert_eq!(
            node(&unread, "calendar-system-calendar-disclosure"),
            (Role::Label, NATIVE_DISCLOSURE)
        );
        assert_eq!(
            node(&unread, "calendar-system-calendar-state"),
            (Role::Status, "Status unavailable")
        );

        // System calendars connected with one calendar macOS no longer offers, Google's
        // settings invalid, one Microsoft account, the direct cards open.
        view.update(cx, |this, cx| {
            this.model.status_error = None;
            this.advanced = true;
            let mut status = native("active", 4, &["cal-a", "cal-gone"], granted(&["cal-a"]));
            let list = status["integrations"].as_array_mut().unwrap();
            list.push(json!({"integration":"google","configured":false,
                "state":{"status":"signed_out"},
                "config_error":"invalid [integration.google] settings: synthetic"}));
            list.push(json!({"integration":"microsoft-graph","configured":true,
                "state":{"status":"signed_out"},"microsoft_accounts":[graph_account(3, 2)]}));
            this.model.apply_statuses(status).unwrap();
            cx.notify();
        });
        let loaded = named(cx);
        assert_distinct(&loaded, &rows);
        assert!(!has(&loaded, "calendars-status-error"));
        assert_eq!(
            node(&loaded, "calendar-system-calendar-state"),
            (Role::Status, "Connected")
        );
        assert_eq!(
            node(&loaded, "calendar-system-calendar-choose"),
            (Role::Label, "Choose the work calendars BroScribe may read:")
        );
        assert_eq!(
            node(&loaded, "calendar-system-calendar-lost").0,
            Role::Status
        );
        assert_eq!(
            node(&loaded, "calendar-google-heading"),
            (Role::Heading, "Google")
        );
        assert_eq!(
            node(&loaded, "calendar-google-config-error"),
            (
                Role::Alert,
                "Google is off because its saved settings are invalid. Invalid \
                 [integration.google] settings: synthetic. Other calendars keep working."
            )
        );
        assert_eq!(
            node(&loaded, "calendar-google-not-configured").0,
            Role::Label
        );
        assert_eq!(
            node(&loaded, "calendar-microsoft-graph-heading"),
            (Role::Heading, "Microsoft 365")
        );
        assert_eq!(
            node(&loaded, "calendar-microsoft-graph-state").0,
            Role::Status
        );
        let account = |part: &str| format!("calendar-microsoft-graph-{HANDLE}-{part}");
        for part in ["identity", "routes"] {
            assert_eq!(node(&loaded, &account(part)).0, Role::Label, "{part}");
        }

        // Waits: one Pending line per card, Cancel beside the sign-in.
        view.update(cx, |this, cx| {
            let attempt = LoginAttemptId::generate();
            this.model
                .begin(
                    Operation::new(Card::provider(IntegrationId::Google), Action::SignIn)
                        .signing_in(attempt),
                )
                .unwrap();
            this.model
                .begin(Operation::new(
                    Card::account(GRAPH, account_ref()),
                    Action::Sync,
                ))
                .unwrap();
            this.model
                .begin(Operation::new(Card::provider(NATIVE), Action::Connect))
                .unwrap();
            cx.notify();
        });
        let waiting = named(cx);
        assert_distinct(&waiting, &rows);
        assert_eq!(
            node(&waiting, "calendar-status-google"),
            (
                Role::Status,
                "Waiting for sign-in… Finish it in your browser."
            )
        );
        assert_eq!(
            node(
                &waiting,
                &format!("calendar-status-microsoft-graph-{HANDLE}")
            ),
            (Role::Status, "Refreshing calendars…")
        );
        assert_eq!(
            node(&waiting, "calendar-status-system-calendar"),
            (
                Role::Status,
                "Waiting for macOS… Answer its permission prompt; it closes after two minutes."
            )
        );
        assert!(!has(&waiting, "calendar-status-microsoft-graph"));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("cancel-sign-in-google").is_some());

        // A cancel that failed is told beside the wait it could not end.
        view.update(cx, |this, cx| {
            let (_, ticket) = this.model.cancel_request(IntegrationId::Google).unwrap();
            this.model
                .apply_cancel_result(IntegrationId::Google, ticket, refused("cancel_failed"));
            cx.notify();
        });
        let cancel_failed = named(cx);
        assert_distinct(&cancel_failed, &rows);
        assert_eq!(
            node(&cancel_failed, "calendar-status-google").0,
            Role::Status
        );
        let (role, text) = node(&cancel_failed, "calendar-status-google-message");
        assert_eq!(role, Role::Alert);
        assert!(text.starts_with("Couldn't cancel the sign-in. "), "{text}");

        // Outcomes replace each wait on the same line: done is a status, a failure an
        // alert with words; no card shows another's outcome.
        view.update(cx, |this, cx| {
            for id in [IntegrationId::Google, GRAPH, NATIVE] {
                let operation = this.model.operations.remove(&id).unwrap();
                let result = match id {
                    IntegrationId::Google => refused("access_denied"),
                    IntegrationId::MicrosoftGraph => Ok(json!({})),
                    IntegrationId::SystemCalendar => refused("calendar_denied"),
                };
                this.model.apply_result(&operation, result);
            }
            cx.notify();
        });
        let done = named(cx);
        assert_distinct(&done, &rows);
        assert_eq!(
            node(&done, "calendar-status-google"),
            (Role::Alert, "Synthetic access_denied.")
        );
        assert!(!has(&done, "calendar-status-google-message"));
        assert_eq!(
            node(&done, &format!("calendar-status-microsoft-graph-{HANDLE}")),
            (Role::Status, "Calendars refreshed.")
        );
        let (role, text) = node(&done, "calendar-status-system-calendar");
        assert_eq!(role, Role::Alert);
        assert!(
            text.starts_with("Calendar access for BroScribe Agent is off."),
            "{text}"
        );
        assert!(!has(&done, "calendar-status-microsoft-graph"));
    }
    #[gpui::test]
    fn calendar_panes_follow_capability_and_label_oauth_fields(cx: &mut gpui::TestAppContext) {
        use crate::ui::a11y_tree::draw_and_read;
        use broscribe_platform::{FakeSystemCalendar, SystemCalendar, UnsupportedSystemCalendar};

        cx.update(gpui::init);
        for host in [
            HostPlatform::MacOs,
            HostPlatform::Windows,
            HostPlatform::Linux,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let state = crate::views::agents::tests::agents_state(cx, dir.path());
            let native: Box<dyn SystemCalendar> = if host == HostPlatform::MacOs {
                Box::new(FakeSystemCalendar::default())
            } else {
                Box::new(UnsupportedSystemCalendar)
            };
            let supported = native.is_supported();
            let (view, visual) = cx.add_window_view(move |_, cx| {
                IntegrationsView::with_native_support(state, supported, cx)
            });
            visual.run_until_parked();
            visual.simulate_resize(gpui::size(gpui::px(830.), gpui::px(1300.)));
            let tree = visual.update(draw_and_read);
            let has = |label| tree.nodes.iter().any(|node| node.name == label);
            assert_eq!(has("Calendars on this Mac"), supported);
            assert_eq!(has("Sign in with Microsoft…"), !supported);
            if supported {
                assert!(has(NATIVE_DISCLOSURE));
                let open = visual.debug_bounds("integrations-advanced").unwrap();
                visual.simulate_click(open.center(), gpui::Modifiers::none());
            }
            let tree = visual.update(draw_and_read);
            for label in [
                "Sign in with Microsoft…",
                "Sign in with Google…",
                "Client ID",
                "Client secret",
                "Setup instructions",
            ] {
                assert!(
                    tree.nodes.iter().any(|node| node.name == label),
                    "{host:?}: {label}"
                );
            }
            assert_eq!(
                tree.nodes.iter().filter(|node| node.role == "Code").count(),
                0
            );
            assert!(view.read_with(visual, |this, _| this.direct_open()));
        }
    }

    #[gpui::test]
    fn account_card_keeps_ids_in_details_and_removal_is_modal(cx: &mut gpui::TestAppContext) {
        use crate::ui::a11y_tree::draw_and_read;
        cx.update(gpui::init);
        let dir = tempfile::tempdir().unwrap();
        let state = crate::views::agents::tests::agents_state(cx, dir.path());
        let mut integrations = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| IntegrationsView::with_native_support(state, false, cx));
            integrations = Some(view.clone());
            gpui::component::Root::new(view, window, cx)
        });
        let view = integrations.unwrap();
        visual.run_until_parked();
        visual.simulate_resize(gpui::size(gpui::px(950.), gpui::px(1800.)));
        let handle = account_ref();
        let id = IntegrationId::MicrosoftGraph;
        view.update(visual, |this, cx| {
            this.model
                .apply_statuses(graph_status(graph_account(4, 7)))
                .unwrap();
            this.set_window_target(Some(format!("{id}/{}", handle.account_id)), cx);
        });
        let tree = visual.update(draw_and_read);
        assert!(
            tree.nodes
                .iter()
                .all(|node| !node.name.contains(handle.account_id.as_str()))
        );
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.name == "Choose calendars…")
        );
        assert!(tree.nodes.iter().any(|node| node.name == "Sync now"));
        let name = tree.nodes[tree.find("calendars-target")].name.clone();
        assert!(
            name.starts_with("Microsoft 365 needs your attention:"),
            "{name}"
        );
        let more = visual
            .debug_bounds("calendar-more-11111111111111111111111111111111")
            .unwrap();
        visual.simulate_click(more.center(), gpui::Modifiers::none());
        visual.run_until_parked();
        let menu = visual.update(draw_and_read);
        assert!(
            menu.nodes
                .iter()
                .any(|node| node.name.starts_with("Allow directory search…"))
        );
        assert!(menu.nodes.iter().any(|node| node.name == "Remove account"));
        visual.simulate_keystrokes("escape");
        visual.update(|window, cx| {
            view.update(cx, |this, cx| {
                let account = this.current_account(&handle).unwrap();
                let opener = this.action_focus[&account_node_id(id, &handle, "more")].clone();
                this.remove_account_confirmation(id, &account, opener, window, cx);
            });
        });
        let dialog = visual.update(draw_and_read);
        let node = dialog
            .nodes
            .iter()
            .find(|node| node.role == "AlertDialog")
            .unwrap();
        assert_eq!(node.name, "Remove this Microsoft account?");
        assert!(node.description.contains("deletes its cached events"));
        assert!(
            node.description
                .contains("Meetings, people and recordings stay.")
        );
        visual.simulate_keystrokes("escape");
        assert!(
            crate::views::agents::tests::commands(dir.path())
                .iter()
                .all(|command| !command.contains("remove-account"))
        );
    }
    #[gpui::test]
    fn native_calendar_controls_and_disconnect_confirmation_follow_permission(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::ui::a11y_tree::draw_and_read;
        cx.update(gpui::init);
        let dir = tempfile::tempdir().unwrap();
        let state = crate::views::agents::tests::agents_state(cx, dir.path());
        let mut integrations = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| IntegrationsView::with_native_support(state, true, cx));
            integrations = Some(view.clone());
            gpui::component::Root::new(view, window, cx)
        });
        let view = integrations.unwrap();
        visual.run_until_parked();
        visual.simulate_resize(gpui::size(gpui::px(830.), gpui::px(1000.)));
        let set = |value, cx: &mut gpui::VisualTestContext| {
            view.update(cx, |this, cx| {
                this.model.apply_statuses(value).unwrap();
                cx.notify();
            })
        };
        let controls = |tree: &crate::ui::a11y_tree::Tree| {
            tree.nodes
                .iter()
                .filter(|node| matches!(node.name.as_str(), "Connect…" | "Sync now" | "Disconnect"))
                .map(|node| node.name.clone())
                .collect::<Vec<_>>()
        };
        set(native("signed_out", 3, &[], granted(&["cal-a"])), visual);
        let tree = visual.update(draw_and_read);
        assert_eq!(controls(&tree), ["Connect…"]);
        assert!(tree.nodes.iter().any(|node| node.name == NATIVE_DISCLOSURE));
        set(native("active", 4, &["cal-a"], granted(&["cal-a"])), visual);
        let tree = visual.update(draw_and_read);
        assert_eq!(controls(&tree), ["Sync now", "Disconnect"]);
        assert!(tree.nodes.iter().all(|node| node.name != NATIVE_DISCLOSURE));
        let button = visual.debug_bounds("native-disconnect").unwrap();
        visual.simulate_click(button.center(), gpui::Modifiers::none());
        visual.run_until_parked();
        let dialog = visual.update(draw_and_read);
        let node = dialog
            .nodes
            .iter()
            .find(|node| node.role == "AlertDialog")
            .unwrap();
        assert_eq!(node.name, "Disconnect calendars on this Mac?");
        assert!(node.description.contains("clears its cached events"));
        assert!(
            node.description
                .contains("Meetings, people and recordings stay.")
        );
        visual.simulate_keystrokes("escape");
        visual.run_until_parked();
        visual.update(|window, cx| {
            assert!(view.read(cx).action_focus["native-disconnect"].is_focused(window))
        });
        assert!(
            crate::views::agents::tests::commands(dir.path())
                .iter()
                .all(|command| !command.contains("logout"))
        );
    }
}
