//! Integration identities, user settings (`[integration.<id>]` in
//! `config.toml`), provider endpoints and the read-only scopes requested.
//!
//! OAuth client IDs are supplied by the user; no provider identity ships in BroScribe.

use std::collections::BTreeMap;
use std::path::Path;

use broscribe_platform::CalendarProvider;
use serde::{Deserialize, Serialize};

use crate::{IntegrationError, Result};

/// The built-in integrations (design §10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntegrationId {
    /// Calendars already synced by macOS, authorized in the installed helper.
    SystemCalendar,
    /// Microsoft 365 through Microsoft Graph and the user's own Entra app registration.
    MicrosoftGraph,
    /// Google Calendar (and the Workspace directory) through the user's desktop OAuth client.
    Google,
}

/// How an integration signs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInFlow {
    /// Authorization code + PKCE with a loopback redirect (RFC 8252).
    LoopbackPkce,
    /// Native OS permission; no OAuth application registration or client secret.
    SystemPermission,
}

impl IntegrationId {
    pub const ALL: [IntegrationId; 3] = [Self::SystemCalendar, Self::MicrosoftGraph, Self::Google];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SystemCalendar => "system-calendar",
            Self::MicrosoftGraph => "microsoft-graph",
            Self::Google => "google",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|id| id.as_str() == s)
            .ok_or_else(|| {
                IntegrationError::Core(broscribe_core::Error::InvalidInput(format!(
                    "unknown integration {s:?} (expected system-calendar, microsoft-graph or google)"
                )))
            })
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::SystemCalendar => "System calendars (macOS)",
            Self::MicrosoftGraph => "Microsoft 365",
            Self::Google => "Google",
        }
    }

    /// The provider named in user-facing text ("Sign in to Microsoft…").
    pub fn provider_name(self) -> &'static str {
        match self {
            Self::SystemCalendar => "macOS",
            Self::MicrosoftGraph => "Microsoft",
            Self::Google => "Google",
        }
    }

    pub fn flow(self) -> SignInFlow {
        match self {
            Self::SystemCalendar => SignInFlow::SystemPermission,
            Self::MicrosoftGraph | Self::Google => SignInFlow::LoopbackPkce,
        }
    }

    /// Which API family serves calendar and directory data.
    pub(crate) fn is_graph(self) -> bool {
        self == Self::MicrosoftGraph
    }

    /// The helper-owned local calendar source behind a native integration.
    pub fn calendar_provider(self) -> Option<CalendarProvider> {
        match self {
            Self::SystemCalendar => Some(CalendarProvider::EventKit),
            Self::MicrosoftGraph | Self::Google => None,
        }
    }

    /// The integration of a native calendar provider.
    pub fn of_provider(provider: CalendarProvider) -> Self {
        match provider {
            CalendarProvider::EventKit => Self::SystemCalendar,
        }
    }

    /// Read by the installed helper under a macOS permission: no OAuth, tokens or directory.
    pub fn is_native_calendar(self) -> bool {
        self.calendar_provider().is_some()
    }
}

impl std::fmt::Display for IntegrationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Directory permission requested from Microsoft Graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GraphDirectory {
    /// `User.ReadBasic.All`: search the organisation's users and read photos.
    #[default]
    UserReadBasicAll,
    /// `People.Read`: the user's relevant people only (no photos).
    PeopleRead,
    /// No directory access; calendar only.
    None,
}

/// `[integration.<id>]` in `config.toml`. Secrets never live here: a Google
/// client secret goes to the `SecretStore` ([`crate::Integrations::set_client_secret`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IntegrationSettings {
    /// OAuth client ID set by the user. Graph and Google are not configured without it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Microsoft tenant (`common`, `organizations` or a tenant ID or domain).
    /// Defaults to `common`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Microsoft only: which directory permission to request.
    pub directory: GraphDirectory,
    /// Google only: request `directory.readonly` (Google Workspace accounts).
    pub workspace: bool,
}

impl IntegrationSettings {
    /// The trimmed client ID override, if one is set.
    pub fn client_id(&self) -> Option<&str> {
        self.client_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// The configured client ID `id` signs in with. Native calendars use none.
    pub fn effective_client_id(&self, id: IntegrationId) -> Option<&str> {
        if id.is_native_calendar() {
            None
        } else {
            self.client_id()
        }
    }
}

/// All `[integration.*]` tables.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IntegrationsConfig {
    pub integration: BTreeMap<String, IntegrationSettings>,
}

/// `[integration.<name>]` tables whose settings did not parse, by table name, each with a
/// message naming only the table and the TOML type error (never the document text).
pub type InvalidTables = BTreeMap<String, String>;

impl IntegrationsConfig {
    fn read(data_dir: &Path) -> Result<Option<String>> {
        let path = broscribe_core::config::Config::path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(IntegrationError::io(&path, e)),
        }
    }

    fn invalid_document(data_dir: &Path, e: &toml::de::Error) -> IntegrationError {
        IntegrationError::Core(broscribe_core::Error::InvalidInput(format!(
            "{}: {}",
            broscribe_core::config::Config::path(data_dir).display(),
            e.message()
        )))
    }

    /// Reads the `[integration.*]` tables of `<data_dir>/config.toml`; a
    /// missing file gives no settings. Other tables are ignored. Any malformed
    /// integration table fails the whole read; see [`Self::load_isolated`].
    pub fn load(data_dir: &Path) -> Result<Self> {
        let Some(text) = Self::read(data_dir)? else {
            return Ok(Self::default());
        };
        Self::parse(&text).map_err(|e| Self::invalid_document(data_dir, &e))
    }

    pub fn parse(text: &str) -> std::result::Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Like [`Self::load`], but one malformed `[integration.<name>]` table disables only
    /// that integration (it reads as unconfigured and is reported in the second value);
    /// healthy integrations and native calendars keep their settings. Only a document
    /// that is not TOML at all fails every integration.
    pub fn load_isolated(data_dir: &Path) -> Result<(Self, InvalidTables)> {
        let Some(text) = Self::read(data_dir)? else {
            return Ok((Self::default(), InvalidTables::new()));
        };
        Self::parse_isolated(&text).map_err(|e| Self::invalid_document(data_dir, &e))
    }

    /// Parses each `[integration.<name>]` table on its own; see [`Self::load_isolated`].
    pub fn parse_isolated(
        text: &str,
    ) -> std::result::Result<(Self, InvalidTables), toml::de::Error> {
        let mut document: toml::Table = text.parse()?;
        let mut config = Self::default();
        let mut invalid = InvalidTables::new();
        match document.remove("integration") {
            None => {}
            Some(toml::Value::Table(tables)) => {
                for (name, value) in tables {
                    match value.try_into::<IntegrationSettings>() {
                        Ok(settings) => {
                            config.integration.insert(name, settings);
                        }
                        Err(e) => {
                            let message =
                                format!("invalid [integration.{name}] settings: {}", e.message());
                            invalid.insert(name, message);
                        }
                    }
                }
            }
            Some(_) => {
                invalid.insert(
                    "integration".into(),
                    "[integration] must contain one table per integration".into(),
                );
            }
        }
        Ok((config, invalid))
    }

    /// The authoritative settings of `id` alone: another integration's malformed table
    /// never affects it, while its own malformed table fails closed as `not_configured`.
    pub fn load_settings(data_dir: &Path, id: IntegrationId) -> Result<IntegrationSettings> {
        let (config, invalid) = Self::load_isolated(data_dir)?;
        if let Some(message) = invalid
            .get(id.as_str())
            .or_else(|| invalid.get("integration"))
        {
            return Err(IntegrationError::NotConfigured(
                id.to_string(),
                message.clone(),
            ));
        }
        Ok(config.settings(id))
    }

    pub fn settings(&self, id: IntegrationId) -> IntegrationSettings {
        self.integration
            .get(id.as_str())
            .cloned()
            .unwrap_or_default()
    }
}

/// Where an integration's OAuth and data requests go. Production values come
/// from [`Endpoints::for_integration`]; tests point them at a local mock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    /// Calendar API base (`…/v1.0` for Graph, `…/calendar/v3` for Google).
    pub api: String,
    /// Directory API base (Graph itself, or the Google People API).
    pub directory_api: String,
    /// Token revocation (RFC 7009), used only by an explicit revoking sign-out.
    /// Google only; `None` where BroScribe offers no provider revocation.
    pub revoke: Option<String>,
}

impl Endpoints {
    /// Native calendars have no OAuth endpoints: macOS grants access to the helper.
    pub fn for_integration(id: IntegrationId, settings: &IntegrationSettings) -> Result<Self> {
        match id {
            IntegrationId::SystemCalendar => Err(IntegrationError::NotConfigured(
                id.to_string(),
                "native calendars use macOS permission, not OAuth endpoints".into(),
            )),
            IntegrationId::Google => Ok(Self {
                authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
                token: "https://oauth2.googleapis.com/token".into(),
                api: "https://www.googleapis.com/calendar/v3".into(),
                directory_api: "https://people.googleapis.com/v1".into(),
                revoke: Some("https://oauth2.googleapis.com/revoke".into()),
            }),
            IntegrationId::MicrosoftGraph => {
                let tenant = settings
                    .tenant
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .unwrap_or("common");
                if !tenant
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
                {
                    return Err(IntegrationError::NotConfigured(
                        id.to_string(),
                        format!("invalid tenant {tenant:?}"),
                    ));
                }
                let base = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0");
                Ok(Self {
                    authorize: format!("{base}/authorize"),
                    token: format!("{base}/token"),
                    api: "https://graph.microsoft.com/v1.0".into(),
                    directory_api: "https://graph.microsoft.com/v1.0".into(),
                    revoke: None,
                })
            }
        }
    }
}

pub const GOOGLE_CALENDAR_READONLY: &str = "https://www.googleapis.com/auth/calendar.readonly";
pub const GOOGLE_DIRECTORY_READONLY: &str = "https://www.googleapis.com/auth/directory.readonly";

/// The minimal read-only scopes requested (plan item 14). Never write scopes.
pub fn scopes(id: IntegrationId, settings: &IntegrationSettings) -> Vec<String> {
    let mut out: Vec<&str> = Vec::new();
    match id {
        IntegrationId::MicrosoftGraph => {
            out.extend(["offline_access", "Calendars.Read", "User.Read"]);
            match settings.directory {
                GraphDirectory::UserReadBasicAll => out.push("User.ReadBasic.All"),
                GraphDirectory::PeopleRead => out.push("People.Read"),
                GraphDirectory::None => {}
            }
        }
        IntegrationId::SystemCalendar => {}
        IntegrationId::Google => {
            out.push(GOOGLE_CALENDAR_READONLY);
            if settings.workspace {
                out.push(GOOGLE_DIRECTORY_READONLY);
            }
        }
    }
    out.into_iter().map(String::from).collect()
}

/// Requested scopes whose grant an administrator may control, flagged in
/// `integrations status`. None of the Graph scopes above needs admin consent
/// by default, but a tenant can require it for every app; Workspace admins
/// decide whether directory data is shared.
pub fn admin_controlled_scopes(id: IntegrationId, settings: &IntegrationSettings) -> Vec<String> {
    match id {
        IntegrationId::Google if settings.workspace => vec![GOOGLE_DIRECTORY_READONLY.into()],
        _ => Vec::new(),
    }
}

/// Whether a directory capability is requested at all.
pub(crate) fn directory_requested(id: IntegrationId, settings: &IntegrationSettings) -> bool {
    match id {
        IntegrationId::SystemCalendar => false,
        IntegrationId::Google => settings.workspace,
        IntegrationId::MicrosoftGraph => settings.directory != GraphDirectory::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_integration_tables_only() {
        let c = IntegrationsConfig::parse(
            r#"
library = "/x"
[integration.microsoft-graph]
client_id = " abc "
tenant = "contoso.example"
directory = "people-read"
[integration.google]
client_id = "g"
workspace = true
"#,
        )
        .unwrap();
        let g = c.settings(IntegrationId::MicrosoftGraph);
        assert_eq!(g.client_id(), Some("abc"));
        assert_eq!(
            scopes(IntegrationId::MicrosoftGraph, &g),
            [
                "offline_access",
                "Calendars.Read",
                "User.Read",
                "People.Read"
            ]
        );
        let e = Endpoints::for_integration(IntegrationId::MicrosoftGraph, &g).unwrap();
        assert!(e.token.contains("/contoso.example/"));
        let google = c.settings(IntegrationId::Google);
        assert_eq!(
            scopes(IntegrationId::Google, &google),
            [GOOGLE_CALENDAR_READONLY, GOOGLE_DIRECTORY_READONLY]
        );
    }

    #[test]
    fn rejects_odd_tenant() {
        let s = IntegrationSettings {
            tenant: Some("a/b".into()),
            ..Default::default()
        };
        assert!(Endpoints::for_integration(IntegrationId::MicrosoftGraph, &s).is_err());
    }

    #[test]
    fn ids_round_trip() {
        for id in IntegrationId::ALL {
            assert_eq!(IntegrationId::parse(id.as_str()).unwrap(), id);
        }
        assert!(IntegrationId::parse("outlook").is_err());
    }

    #[test]
    fn system_calendar_has_no_oauth_endpoints_scopes_or_directory() {
        let settings = IntegrationSettings {
            client_id: Some("ignored".into()),
            workspace: true,
            ..Default::default()
        };
        let id = IntegrationId::SystemCalendar;
        assert_eq!(id.flow(), SignInFlow::SystemPermission);
        assert_eq!(IntegrationId::ALL[0], id);
        assert!(Endpoints::for_integration(id, &settings).is_err());
        assert!(scopes(id, &settings).is_empty());
        assert!(admin_controlled_scopes(id, &settings).is_empty());
        assert!(!directory_requested(id, &settings));
    }

    #[test]
    fn configured_clients_only_and_retired_tables_are_inert() {
        let config = IntegrationsConfig::parse(
            r#"
[integration.teams-session]
client_id = "retired-client"
[integration.outlook-calendar]
client_id = "retired-native"
"#,
        )
        .unwrap();
        for id in IntegrationId::ALL {
            assert_eq!(config.settings(id), IntegrationSettings::default());
            assert_eq!(config.settings(id).effective_client_id(id), None);
        }
        assert!(IntegrationId::parse("teams-session").is_err());
        assert!(IntegrationId::parse("outlook-calendar").is_err());
        let configured = IntegrationSettings {
            client_id: Some(" synthetic-client ".into()),
            ..Default::default()
        };
        for id in [IntegrationId::MicrosoftGraph, IntegrationId::Google] {
            assert_eq!(configured.effective_client_id(id), Some("synthetic-client"));
            assert_eq!(id.flow(), SignInFlow::LoopbackPkce);
        }
        assert_eq!(
            configured.effective_client_id(IntegrationId::SystemCalendar),
            None
        );
        assert_eq!(
            IntegrationId::of_provider(CalendarProvider::EventKit),
            IntegrationId::SystemCalendar
        );
    }

    #[test]
    fn malformed_integration_table_disables_only_that_integration() {
        let text = r#"
library = "/synthetic"
[integration.google]
client_id = 123
[integration.microsoft-graph]
client_id = "synthetic-graph"
"#;
        assert!(IntegrationsConfig::parse(text).is_err());
        let (config, invalid) = IntegrationsConfig::parse_isolated(text).unwrap();
        assert_eq!(
            config.settings(IntegrationId::MicrosoftGraph).client_id(),
            Some("synthetic-graph")
        );
        assert_eq!(
            config.settings(IntegrationId::Google),
            IntegrationSettings::default()
        );
        assert_eq!(invalid.keys().collect::<Vec<_>>(), ["google"]);
        assert!(invalid["google"].starts_with("invalid [integration.google] settings"));
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(broscribe_core::config::Config::path(tmp.path()), text).unwrap();
        assert_eq!(
            IntegrationsConfig::load_settings(tmp.path(), IntegrationId::MicrosoftGraph)
                .unwrap()
                .client_id(),
            Some("synthetic-graph")
        );
        assert_eq!(
            IntegrationsConfig::load_settings(tmp.path(), IntegrationId::Google)
                .unwrap_err()
                .code(),
            "not_configured"
        );
        // Native calendars read no OAuth settings and stay usable.
        assert_eq!(
            IntegrationsConfig::load_settings(tmp.path(), IntegrationId::SystemCalendar).unwrap(),
            IntegrationSettings::default()
        );
        assert!(IntegrationsConfig::parse_isolated("not = [toml").is_err());
    }
}
