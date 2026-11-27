//! OS-free remote calendar identities and authority stamps. Display labels are not identities.
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteIntegration {
    MicrosoftGraph,
}

impl RemoteIntegration {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MicrosoftGraph => "microsoft-graph",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct AccountId(String);

impl AccountId {
    pub fn parse(value: &str) -> crate::Result<Self> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(crate::Error::InvalidInput("invalid account handle".into()));
        }
        Ok(Self(value.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}
impl<'de> Deserialize<'de> for AccountId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AccountRef {
    pub integration: RemoteIntegration,
    pub account_id: AccountId,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CalendarTarget {
    Own {
        calendar_id: String,
    },
    RecipientShared {
        calendar_id: String,
    },
    Delegated {
        mailbox: String,
        calendar_id: Option<String>,
    },
    Group {
        group_id: String,
    },
}

impl CalendarTarget {
    pub fn validate(&self) -> crate::Result<()> {
        let valid = |id: &str| {
            !id.trim().is_empty()
                && id != "."
                && id != ".."
                && id.len() <= 2048
                && !id.chars().any(char::is_control)
        };
        let accepted = match self {
            Self::Own { calendar_id } | Self::RecipientShared { calendar_id } => valid(calendar_id),
            Self::Delegated {
                mailbox,
                calendar_id,
            } => valid(mailbox) && calendar_id.as_deref().is_none_or(valid),
            Self::Group { group_id } => valid(group_id),
        };
        if accepted {
            Ok(())
        } else {
            Err(crate::Error::Service { code: "invalid_calendar_target", message: "calendar routing identifiers must be nonempty, bounded, not dot segments, and contain no control characters".into() })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RemoteCalendarStamp {
    pub account: AccountRef,
    pub generation: u64,
    pub selection_generation: u64,
    pub target: CalendarTarget,
}

/// Live authority for an account and its currently usable selected routes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountAuthority {
    pub account: AccountRef,
    pub generation: u64,
    pub selection_generation: u64,
    pub selected_calendars: Vec<CalendarTarget>,
}

/// Google calendar authority: selection changes never change account or
/// directory/photo identity. Both counters must match before using a prior.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleCalendarStamp {
    pub generation: u64,
    pub selection_generation: u64,
}
