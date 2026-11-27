//! Helper-owned system-calendar access. No credentials or native objects cross this boundary.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarAuthorization {
    #[default]
    NotDetermined,
    Denied,
    Restricted,
    WriteOnly,
    Granted,
    Unsupported,
}

/// Eligible calendars only: birthday and subscription sources are not offered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemCalendarInfo {
    pub id: String,
    pub title: String,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemCalendarAttendee {
    pub name: Option<String>,
    pub email: Option<String>,
    pub is_self: bool,
}

/// UTC seconds, extracted links and eligible meeting participants; never event bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemCalendarEvent {
    pub id: String,
    pub calendar_id: String,
    /// Published iCalendar UID, when available; never inferred from a local identifier.
    pub ical_uid: Option<String>,
    /// UTC recurrence identity before an exception moves the occurrence.
    pub original_start_s: i64,
    pub title: Option<String>,
    pub start_s: i64,
    pub end_s: i64,
    pub links: Vec<String>,
    pub attendees: Vec<SystemCalendarAttendee>,
}

/// Only the installed helper receives the real native implementation.
pub trait SystemCalendar: Send + Sync {
    /// Whether this platform offers a native calendar source, independent of permission.
    fn is_supported(&self) -> bool {
        true
    }

    fn authorization(&self) -> Result<CalendarAuthorization>;
    /// Explicit user action; never call from status, startup or maintenance.
    fn request_access(&self) -> Result<CalendarAuthorization>;
    fn calendars(&self) -> Result<Vec<SystemCalendarInfo>>;
    /// Explicit selected IDs. An empty/resolved-empty set must never query all calendars.
    fn events(
        &self,
        from_s: i64,
        to_s: i64,
        calendar_ids: &[String],
    ) -> Result<Vec<SystemCalendarEvent>>;
}

/// Which helper-owned local calendar source a consent, a generation or an event belongs to.
/// Each provider has its own opt-in, permission, selection and generation: equal numbers
/// of different providers are unrelated consents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarProvider {
    /// macOS EventKit (calendars synced by the system).
    EventKit,
}

impl CalendarProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EventKit => "event_kit",
        }
    }
}

/// Consumers of durable calendar snapshots ask the helper for the current permission/opt-in
/// of the provider whose events they hold, never another provider's.
pub trait CalendarAuthority: Send + Sync {
    fn active_generation(&self, provider: CalendarProvider) -> Result<Option<u64>>;
    /// Read-only current authority of exactly the requested remote account.
    fn remote_authority(
        &self,
        account: &broscribe_core::calendar::AccountRef,
    ) -> Result<Option<broscribe_core::calendar::AccountAuthority>>;
    /// Read-only live Google calendar authority the agent holds: the account generation
    /// and the calendar-selection generation, both of which a prior must carry. `None`
    /// when Google has no active authority. Errors never mean "unchanged": callers fail
    /// closed and keep Google priors out of inference and publication.
    fn google_authority(&self) -> Result<Option<broscribe_core::calendar::GoogleCalendarStamp>>;
}

/// The EventKit feature is macOS-only; other platform factories expose this honestly.
pub struct UnsupportedSystemCalendar;
impl SystemCalendar for UnsupportedSystemCalendar {
    fn is_supported(&self) -> bool {
        false
    }

    fn authorization(&self) -> Result<CalendarAuthorization> {
        Ok(CalendarAuthorization::Unsupported)
    }
    fn request_access(&self) -> Result<CalendarAuthorization> {
        Err(unsupported())
    }
    fn calendars(&self) -> Result<Vec<SystemCalendarInfo>> {
        Err(unsupported())
    }
    fn events(&self, _: i64, _: i64, _: &[String]) -> Result<Vec<SystemCalendarEvent>> {
        Err(unsupported())
    }
}
pub struct UnsupportedCalendarAuthority;
impl CalendarAuthority for UnsupportedCalendarAuthority {
    fn active_generation(&self, provider: CalendarProvider) -> Result<Option<u64>> {
        Err(match provider {
            CalendarProvider::EventKit => unsupported(),
        })
    }
    fn remote_authority(
        &self,
        _: &broscribe_core::calendar::AccountRef,
    ) -> Result<Option<broscribe_core::calendar::AccountAuthority>> {
        Err(Error::Unsupported(
            "remote calendar authority is unavailable on this platform".into(),
        ))
    }
    fn google_authority(&self) -> Result<Option<broscribe_core::calendar::GoogleCalendarStamp>> {
        Err(Error::Unsupported(
            "Google calendar authority is unavailable on this platform".into(),
        ))
    }
}
fn unsupported() -> Error {
    Error::Unsupported("macOS system-calendar access is unavailable on this platform".into())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarQuery {
    pub from_s: i64,
    pub to_s: i64,
    pub calendar_ids: Vec<String>,
}

/// Deterministic calendar permission/data seam; defaults to no permission or data.
#[derive(Default)]
pub struct FakeSystemCalendar {
    pub authorization: Mutex<CalendarAuthorization>,
    pub request_result: Mutex<CalendarAuthorization>,
    pub calendars: Mutex<Vec<SystemCalendarInfo>>,
    pub events: Mutex<Vec<SystemCalendarEvent>>,
    pub access_requests: AtomicUsize,
    pub calendar_reads: AtomicUsize,
    pub event_queries: Mutex<Vec<CalendarQuery>>,
}
fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}
impl SystemCalendar for FakeSystemCalendar {
    fn authorization(&self) -> Result<CalendarAuthorization> {
        Ok(*lock(&self.authorization))
    }
    fn request_access(&self) -> Result<CalendarAuthorization> {
        self.access_requests.fetch_add(1, Ordering::SeqCst);
        let result = *lock(&self.request_result);
        *lock(&self.authorization) = result;
        Ok(result)
    }
    fn calendars(&self) -> Result<Vec<SystemCalendarInfo>> {
        self.calendar_reads.fetch_add(1, Ordering::SeqCst);
        Ok(lock(&self.calendars).clone())
    }
    fn events(
        &self,
        from_s: i64,
        to_s: i64,
        calendar_ids: &[String],
    ) -> Result<Vec<SystemCalendarEvent>> {
        lock(&self.event_queries).push(CalendarQuery {
            from_s,
            to_s,
            calendar_ids: calendar_ids.to_vec(),
        });
        Ok(lock(&self.events)
            .iter()
            .filter(|event| {
                calendar_ids.contains(&event.calendar_id)
                    && event.start_s < to_s
                    && event.end_s > from_s
            })
            .cloned()
            .collect())
    }
}

/// Live native-calendar generation; defaults to no opt-in.
#[derive(Default)]
pub struct FakeCalendarAuthority {
    /// EventKit (system calendar).
    pub generation: Mutex<Option<u64>>,
    /// Every provider asked, in order.
    pub queries: Mutex<Vec<CalendarProvider>>,
    pub remote: Mutex<
        std::collections::BTreeMap<
            broscribe_core::calendar::AccountRef,
            broscribe_core::calendar::AccountAuthority,
        >,
    >,
    pub remote_queries: Mutex<Vec<broscribe_core::calendar::AccountRef>>,
    /// Live Google calendar authority (account and selection generations).
    pub google_authority: Mutex<Option<broscribe_core::calendar::GoogleCalendarStamp>>,
    /// Times the Google authority was asked.
    pub google_queries: AtomicUsize,
    /// Service code returned instead of the Google authority (agent down, outdated…).
    pub google_error: Mutex<Option<&'static str>>,
}
impl CalendarAuthority for FakeCalendarAuthority {
    fn active_generation(&self, provider: CalendarProvider) -> Result<Option<u64>> {
        lock(&self.queries).push(provider);
        Ok(*lock(match provider {
            CalendarProvider::EventKit => &self.generation,
        }))
    }
    fn remote_authority(
        &self,
        account: &broscribe_core::calendar::AccountRef,
    ) -> Result<Option<broscribe_core::calendar::AccountAuthority>> {
        lock(&self.remote_queries).push(account.clone());
        Ok(lock(&self.remote).get(account).cloned())
    }
    fn google_authority(&self) -> Result<Option<broscribe_core::calendar::GoogleCalendarStamp>> {
        self.google_queries.fetch_add(1, Ordering::SeqCst);
        if let Some(code) = *lock(&self.google_error) {
            return Err(Error::Service {
                code,
                message: "synthetic Google calendar authority failure".into(),
            });
        }
        Ok(*lock(&self.google_authority))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_authority_seams_are_explicit() {
        let fake = FakeCalendarAuthority::default();
        assert_eq!(fake.google_authority().unwrap(), None);
        let stamp = broscribe_core::calendar::GoogleCalendarStamp {
            generation: 4,
            selection_generation: 9,
        };
        *lock(&fake.google_authority) = Some(stamp);
        assert_eq!(fake.google_authority().unwrap(), Some(stamp));
        *lock(&fake.google_error) = Some("helper_required");
        assert_eq!(
            fake.google_authority().unwrap_err().code(),
            "helper_required"
        );
        assert_eq!(fake.google_queries.load(Ordering::SeqCst), 3);
        // Unsupported platforms fail closed instead of claiming "no authority".
        assert_eq!(
            UnsupportedCalendarAuthority
                .google_authority()
                .unwrap_err()
                .code(),
            "unsupported"
        );
    }
}
