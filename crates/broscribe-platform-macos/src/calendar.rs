//! The system calendar through EventKit (macOS 14+), for the signed recording
//! agent only: its bundle carries the calendar usage description and owns the
//! grant. Every call builds its own `EKEventStore` inside an autorelease pool on
//! the calling thread and returns plain data; no native object outlives the call
//! or crosses a thread, except the store kept by a pending access request's
//! completion block until the system answers. Nothing here writes to a calendar.

use std::cell::OnceCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use block2::RcBlock;
use broscribe_platform::{
    CalendarAuthorization, Error, Result, SystemCalendar, SystemCalendarAttendee,
    SystemCalendarEvent, SystemCalendarInfo,
};
use objc2::msg_send;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::Bool;
use objc2_event_kit::{
    EKAuthorizationStatus, EKCalendar, EKCalendarType, EKEntityMask, EKEntityType, EKEvent,
    EKEventStatus, EKEventStore, EKParticipant, EKParticipantStatus, EKParticipantType,
    EKSourceType,
};
use objc2_foundation::{NSArray, NSBundle, NSDate, NSError, NSString, NSURL};

/// How long an explicit access request waits for the user's answer.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// The longest event window: the calendar sync's one day back to 30 days ahead.
pub const MAX_WINDOW_S: i64 = 31 * 86_400;
/// Without this Info.plist key the system terminates a process that asks for access.
const USAGE_KEY: &str = "NSCalendarsFullAccessUsageDescription";
const MAX_LINKS: usize = 10;

/// One access request per process at a time; held until the system's answer
/// even after [`PROMPT_TIMEOUT`], so repeated clicks never open another request.
static PROMPT: LazyLock<PromptGate> = LazyLock::new(PromptGate::default);

/// EventKit-backed [`SystemCalendar`]; holds no native state between calls.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacSystemCalendar;

impl SystemCalendar for MacSystemCalendar {
    fn authorization(&self) -> Result<CalendarAuthorization> {
        Ok(native_authorization())
    }

    fn request_access(&self) -> Result<CalendarAuthorization> {
        let current = native_authorization();
        // Full access can be asked for only while unanswered or add-only; the
        // system never prompts again after a denial or restriction.
        if !matches!(
            current,
            CalendarAuthorization::NotDetermined | CalendarAuthorization::WriteOnly
        ) {
            return Ok(current);
        }
        if !usage_description_present() {
            return Err(service(
                "helper_required",
                "calendar access must be requested by the installed BroScribe Agent",
            ));
        }
        let Some(ticket) = PROMPT.try_begin() else {
            return Err(service(
                "busy",
                "a calendar permission request is already waiting for an answer",
            ));
        };
        let (send, receive) = mpsc::sync_channel(1);
        autoreleasepool(|_| {
            // SAFETY: a plain store; EventKit allows one per request.
            let store = unsafe { EKEventStore::new() };
            let owner = store.clone();
            let completion: RcBlock<dyn Fn(Bool, *mut NSError)> =
                RcBlock::new(move |granted: Bool, _error: *mut NSError| {
                    // The store lives as long as the system holds this block.
                    let _owner = &owner;
                    ticket.finish();
                    let _ = send.try_send(granted.as_bool());
                });
            // SAFETY: a valid block; EventKit copies it and calls it once.
            unsafe { store.requestFullAccessToEventsWithCompletion(RcBlock::as_ptr(&completion)) };
        });
        prompt_answer(receive.recv_timeout(PROMPT_TIMEOUT), native_authorization)
    }

    fn calendars(&self) -> Result<Vec<SystemCalendarInfo>> {
        require_read(native_authorization())?;
        autoreleasepool(|_| {
            // SAFETY: a plain store, used and released on this thread.
            let store = unsafe { EKEventStore::new() };
            // SAFETY: the event entity type is valid on every supported system.
            let calendars = unsafe { store.calendarsForEntityType(EKEntityType::Event) };
            Ok(eligible_calendars(
                calendars.iter().map(|calendar| raw_calendar(&calendar)),
            ))
        })
    }

    fn events(
        &self,
        from_s: i64,
        to_s: i64,
        calendar_ids: &[String],
    ) -> Result<Vec<SystemCalendarEvent>> {
        autoreleasepool(|_| selected_events(&NativeStore::default(), from_s, to_s, calendar_ids))
    }
}

// ---- policy seam ----

/// Calendar type as EventKit reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalendarKind {
    Local,
    CalDav,
    Exchange,
    Subscription,
    Birthday,
    Other,
}

/// Account type of a calendar's source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Local,
    Exchange,
    CalDav,
    MobileMe,
    Subscribed,
    Birthdays,
    Other,
}

/// What EventKit says about one calendar, before eligibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawCalendar {
    pub id: String,
    pub title: String,
    pub source: String,
    pub kind: CalendarKind,
    pub source_kind: Option<SourceKind>,
    pub subscribed: bool,
    pub allows_events: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParticipantKind {
    Person,
    Group,
    Room,
    Resource,
    Unknown,
}

/// One attendee or organizer; `url` is usually `mailto:`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawParticipant {
    pub name: Option<String>,
    pub url: Option<String>,
    pub is_current_user: bool,
    pub declined: bool,
    pub kind: ParticipantKind,
}

/// One event occurrence as EventKit returns it. `notes` and `location` are
/// read only to extract links and never leave this module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawCalendarEvent {
    /// `eventIdentifier`, shared by every occurrence of a recurring event.
    pub id: String,
    pub calendar_id: String,
    pub title: Option<String>,
    pub start_s: i64,
    pub end_s: i64,
    /// The occurrence's original start (`occurrenceDate`), stable when one
    /// occurrence is moved.
    pub occurrence_s: Option<i64>,
    pub all_day: bool,
    pub cancelled: bool,
    pub url: Option<String>,
    pub location: Option<String>,
    pub notes: Option<String>,
    pub organizer: Option<RawParticipant>,
    pub attendees: Vec<RawParticipant>,
}

/// The native lookups [`selected_events`] needs, so its selection rules run
/// against EventKit and synthetic stores alike.
pub trait EventStoreAccess {
    type Calendar;
    fn authorization(&self) -> Result<CalendarAuthorization>;
    /// The calendar with exactly this ID, if the store has one.
    fn calendar(&self, id: &str) -> Option<(RawCalendar, Self::Calendar)>;
    /// Occurrences overlapping the window in `calendars` (never empty).
    fn events(
        &self,
        from_s: i64,
        to_s: i64,
        calendars: Vec<Self::Calendar>,
    ) -> Result<Vec<RawCalendarEvent>>;
}

/// Calendars a user may select: event calendars that are neither birthdays
/// nor subscriptions, ordered by source and title.
pub fn eligible_calendars(
    calendars: impl IntoIterator<Item = RawCalendar>,
) -> Vec<SystemCalendarInfo> {
    let mut eligible: Vec<SystemCalendarInfo> = calendars
        .into_iter()
        .filter(is_eligible)
        .map(|calendar| SystemCalendarInfo {
            id: calendar.id,
            title: calendar.title,
            source: calendar.source,
        })
        .collect();
    eligible.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.id.cmp(&b.id))
    });
    eligible.dedup_by(|a, b| a.id == b.id);
    eligible
}

/// Meetings in the explicitly selected eligible calendars. An empty selection,
/// or one that resolves to no eligible calendar, returns nothing without
/// querying events; the store is never asked for all calendars.
pub fn selected_events<S: EventStoreAccess>(
    store: &S,
    from_s: i64,
    to_s: i64,
    calendar_ids: &[String],
) -> Result<Vec<SystemCalendarEvent>> {
    validate_window(from_s, to_s)?;
    let mut ids: Vec<&str> = Vec::new();
    for id in calendar_ids.iter().map(|id| id.trim()) {
        if !id.is_empty() && !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    require_read(store.authorization()?)?;
    let mut selected = HashSet::new();
    let mut handles = Vec::new();
    for id in ids {
        if let Some((calendar, handle)) = store.calendar(id)
            && calendar.id == id
            && is_eligible(&calendar)
        {
            selected.insert(calendar.id);
            handles.push(handle);
        }
    }
    if handles.is_empty() {
        return Ok(Vec::new());
    }
    let events = store.events(from_s, to_s, handles)?;
    Ok(meeting_events(from_s, to_s, &selected, events))
}

fn is_eligible(calendar: &RawCalendar) -> bool {
    calendar.allows_events
        && !calendar.subscribed
        && !matches!(
            calendar.kind,
            CalendarKind::Birthday | CalendarKind::Subscription
        )
        && !matches!(
            calendar.source_kind,
            Some(SourceKind::Birthdays | SourceKind::Subscribed)
        )
}

fn validate_window(from_s: i64, to_s: i64) -> Result<()> {
    match to_s.checked_sub(from_s) {
        Some(span) if (1..=MAX_WINDOW_S).contains(&span) => Ok(()),
        _ => Err(Error::InvalidInput(format!(
            "calendar window must be positive and at most {} days",
            MAX_WINDOW_S / 86_400
        ))),
    }
}

fn require_read(authorization: CalendarAuthorization) -> Result<()> {
    match authorization {
        CalendarAuthorization::Granted => Ok(()),
        CalendarAuthorization::Denied => Err(service(
            "calendar_denied",
            "calendar access is denied for BroScribe Agent; allow it in System Settings › Privacy & Security › Calendars",
        )),
        CalendarAuthorization::Restricted => Err(service(
            "calendar_restricted",
            "calendar access is restricted on this Mac (device management or Screen Time)",
        )),
        CalendarAuthorization::WriteOnly => Err(service(
            "calendar_write_only",
            "BroScribe Agent can only add calendar events; reading meetings needs full access in System Settings › Privacy & Security › Calendars",
        )),
        CalendarAuthorization::NotDetermined => Err(service(
            "calendar_pending",
            "calendar access has not been granted yet; connect the system calendar first",
        )),
        CalendarAuthorization::Unsupported => Err(Error::Unsupported(
            "this macOS version reports an unknown calendar authorization".into(),
        )),
    }
}

fn meeting_events(
    from_s: i64,
    to_s: i64,
    selected: &HashSet<String>,
    events: Vec<RawCalendarEvent>,
) -> Vec<SystemCalendarEvent> {
    let mut meetings: Vec<SystemCalendarEvent> = events
        .into_iter()
        .filter(|event| {
            selected.contains(&event.calendar_id)
                && !event.all_day
                && !event.cancelled
                && event.end_s >= event.start_s
                && event.start_s < to_s
                && event.end_s > from_s
        })
        .filter_map(meeting)
        .collect();
    meetings.sort_by(|a, b| a.start_s.cmp(&b.start_s).then_with(|| a.id.cmp(&b.id)));
    meetings.dedup_by(|a, b| a.id == b.id);
    meetings
}

/// `None` when the user declined or, for an event with attendees, takes no part.
fn meeting(event: RawCalendarEvent) -> Option<SystemCalendarEvent> {
    if event
        .attendees
        .iter()
        .any(|attendee| attendee.is_current_user && attendee.declined)
    {
        return None;
    }
    let involved = event
        .attendees
        .iter()
        .chain(&event.organizer)
        .any(|participant| participant.is_current_user);
    if !event.attendees.is_empty() && !involved {
        return None;
    }
    let mut links = Vec::new();
    for text in [&event.url, &event.location, &event.notes]
        .into_iter()
        .flatten()
    {
        extract_links(text, &mut links);
    }
    let occurrence_s = event.occurrence_s.unwrap_or(event.start_s);
    Some(SystemCalendarEvent {
        id: format!("{}@{occurrence_s}", event.id),
        calendar_id: event.calendar_id,
        ical_uid: None,
        original_start_s: occurrence_s,
        title: event
            .title
            .map(|title| title.trim().to_owned())
            .filter(|title| !title.is_empty()),
        start_s: event.start_s,
        end_s: event.end_s,
        links,
        attendees: attendees(event.organizer.as_ref(), &event.attendees),
    })
}

/// People only (no rooms, resources or attendees who declined), the
/// organizer first, merged by e-mail, otherwise by name.
fn attendees(
    organizer: Option<&RawParticipant>,
    attendees: &[RawParticipant],
) -> Vec<SystemCalendarAttendee> {
    let mut people: Vec<(String, SystemCalendarAttendee)> = Vec::new();
    for participant in organizer.into_iter().chain(attendees) {
        if matches!(
            participant.kind,
            ParticipantKind::Room | ParticipantKind::Resource
        ) || (participant.declined && !participant.is_current_user)
        {
            continue;
        }
        let name = participant
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let email = participant.url.as_deref().and_then(mailto_email);
        let key = match (&email, &name) {
            (Some(email), _) => email.to_lowercase(),
            (None, Some(name)) => format!("name:{}", name.to_lowercase()),
            (None, None) => continue,
        };
        if let Some((_, known)) = people.iter_mut().find(|(known, _)| *known == key) {
            known.is_self |= participant.is_current_user;
            if known.name.is_none() {
                known.name = name;
            }
            continue;
        }
        people.push((
            key,
            SystemCalendarAttendee {
                name,
                email,
                is_self: participant.is_current_user,
            },
        ));
    }
    people.into_iter().map(|(_, person)| person).collect()
}

/// The single address of a `mailto:` URL, percent-decoded, without headers.
fn mailto_email(url: &str) -> Option<String> {
    let url = url.trim();
    let scheme = url.get(..7)?;
    if !scheme.eq_ignore_ascii_case("mailto:") {
        return None;
    }
    let address = url[7..].split('?').next()?;
    let address = percent_decode(address)?;
    let address = address.trim();
    let (local, domain) = address.rsplit_once('@')?;
    let valid = !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !address
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, ',' | ';' | '<' | '>'));
    valid.then(|| address.to_owned())
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Appends the `http`/`https` links found in `text`, nothing else from it.
fn extract_links(text: &str, links: &mut Vec<String>) {
    let separators = |c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' | '^' | '`'
            )
    };
    for token in text.split(separators) {
        if links.len() >= MAX_LINKS {
            return;
        }
        let lower = token.to_ascii_lowercase();
        let Some(start) = [lower.find("https://"), lower.find("http://")]
            .into_iter()
            .flatten()
            .min()
        else {
            continue;
        };
        let link = token[start..].trim_end_matches(['.', ',', ';', ':', '!', '?', '*']);
        let host = link.split_once("://").map_or("", |(_, rest)| rest);
        if host.starts_with(|c: char| c.is_alphanumeric()) && !links.iter().any(|l| l == link) {
            links.push(link.to_owned());
        }
    }
}

fn service(code: &'static str, message: &str) -> Error {
    Error::Service {
        code,
        message: message.to_owned(),
    }
}

/// Single-flight flag for the system access prompt.
#[derive(Default)]
struct PromptGate(Arc<AtomicBool>);

impl PromptGate {
    fn try_begin(&self) -> Option<PromptTicket> {
        self.0
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(PromptTicket {
            gate: Arc::clone(&self.0),
            finished: AtomicBool::new(false),
        })
    }
}

/// Releases the gate once: on the system's answer, or when the system drops
/// the completion without calling it.
struct PromptTicket {
    gate: Arc<AtomicBool>,
    finished: AtomicBool,
}

impl PromptTicket {
    fn finish(&self) {
        if !self.finished.swap(true, Ordering::SeqCst) {
            self.gate.store(false, Ordering::SeqCst);
        }
    }
}

impl Drop for PromptTicket {
    fn drop(&mut self) {
        self.finish();
    }
}

/// A grant is reported only from the system's own answer; a timeout leaves
/// it unknown.
fn prompt_answer(
    answer: std::result::Result<bool, RecvTimeoutError>,
    current: impl FnOnce() -> CalendarAuthorization,
) -> Result<CalendarAuthorization> {
    match answer {
        Ok(true) => Ok(CalendarAuthorization::Granted),
        Ok(false) | Err(RecvTimeoutError::Disconnected) => Ok(current()),
        Err(RecvTimeoutError::Timeout) => Err(service(
            "calendar_pending",
            "the calendar permission prompt is still waiting for an answer; connect again after answering it",
        )),
    }
}

// ---- EventKit ----

fn native_authorization() -> CalendarAuthorization {
    // SAFETY: a class query with a valid entity type; it never prompts.
    let status = unsafe { EKEventStore::authorizationStatusForEntityType(EKEntityType::Event) };
    match status {
        EKAuthorizationStatus::NotDetermined => CalendarAuthorization::NotDetermined,
        EKAuthorizationStatus::Restricted => CalendarAuthorization::Restricted,
        EKAuthorizationStatus::Denied => CalendarAuthorization::Denied,
        EKAuthorizationStatus::FullAccess => CalendarAuthorization::Granted,
        EKAuthorizationStatus::WriteOnly => CalendarAuthorization::WriteOnly,
        // A status newer than this SDK: never guess a grant.
        _ => CalendarAuthorization::Unsupported,
    }
}

fn usage_description_present() -> bool {
    autoreleasepool(|_| {
        NSBundle::mainBundle()
            .objectForInfoDictionaryKey(&NSString::from_str(USAGE_KEY))
            .and_then(|value| value.downcast::<NSString>().ok())
            .is_some_and(|value| !value.to_string().trim().is_empty())
    })
}

/// One query's store, created only once a selected calendar is looked up.
#[derive(Default)]
struct NativeStore {
    store: OnceCell<Retained<EKEventStore>>,
}

impl NativeStore {
    fn store(&self) -> &EKEventStore {
        // SAFETY: a plain store, used and released on this thread.
        self.store.get_or_init(|| unsafe { EKEventStore::new() })
    }
}

impl EventStoreAccess for NativeStore {
    type Calendar = Retained<EKCalendar>;

    fn authorization(&self) -> Result<CalendarAuthorization> {
        Ok(native_authorization())
    }

    fn calendar(&self, id: &str) -> Option<(RawCalendar, Retained<EKCalendar>)> {
        // SAFETY: a valid identifier string; unknown identifiers return nil.
        let calendar = unsafe { self.store().calendarWithIdentifier(&NSString::from_str(id)) }?;
        Some((raw_calendar(&calendar), calendar))
    }

    fn events(
        &self,
        from_s: i64,
        to_s: i64,
        calendars: Vec<Retained<EKCalendar>>,
    ) -> Result<Vec<RawCalendarEvent>> {
        let start = NSDate::dateWithTimeIntervalSince1970(from_s as f64);
        let end = NSDate::dateWithTimeIntervalSince1970(to_s as f64);
        let calendars = NSArray::from_retained_slice(&calendars);
        let store = self.store();
        // SAFETY: valid dates and a non-empty array of this store's calendars,
        // so the predicate never spans all calendars.
        let predicate = unsafe {
            store.predicateForEventsWithStartDate_endDate_calendars(&start, &end, Some(&*calendars))
        };
        // SAFETY: the predicate comes from this store; the result may be nil.
        let events: Option<Retained<NSArray<EKEvent>>> =
            unsafe { msg_send![store, eventsMatchingPredicate: &*predicate] };
        Ok(events
            .map(|events| {
                events
                    .iter()
                    .filter_map(|event| raw_event(&event))
                    .collect()
            })
            .unwrap_or_default())
    }
}

fn raw_calendar(calendar: &EKCalendar) -> RawCalendar {
    // SAFETY: property reads on a live calendar of this call's store.
    unsafe {
        let source = calendar.source();
        RawCalendar {
            id: calendar.calendarIdentifier().to_string(),
            title: calendar.title().to_string(),
            source: source
                .as_ref()
                .map(|source| source.title().to_string())
                .unwrap_or_default(),
            kind: match calendar.r#type() {
                EKCalendarType::Local => CalendarKind::Local,
                EKCalendarType::CalDAV => CalendarKind::CalDav,
                EKCalendarType::Exchange => CalendarKind::Exchange,
                EKCalendarType::Subscription => CalendarKind::Subscription,
                EKCalendarType::Birthday => CalendarKind::Birthday,
                _ => CalendarKind::Other,
            },
            source_kind: source.map(|source| match source.sourceType() {
                EKSourceType::Local => SourceKind::Local,
                EKSourceType::Exchange => SourceKind::Exchange,
                EKSourceType::CalDAV => SourceKind::CalDav,
                EKSourceType::MobileMe => SourceKind::MobileMe,
                EKSourceType::Subscribed => SourceKind::Subscribed,
                EKSourceType::Birthdays => SourceKind::Birthdays,
                _ => SourceKind::Other,
            }),
            subscribed: calendar.isSubscribed(),
            allows_events: calendar.allowedEntityTypes().0 & EKEntityMask::Event.0 != 0,
        }
    }
}

fn raw_event(event: &EKEvent) -> Option<RawCalendarEvent> {
    // SAFETY: property reads on a live event of this call's store; the
    // nullable ones are read as optional.
    unsafe {
        let calendar_id = event.calendar()?.calendarIdentifier().to_string();
        let start: Option<Retained<NSDate>> = msg_send![event, startDate];
        let end: Option<Retained<NSDate>> = msg_send![event, endDate];
        let start = start?;
        let end = end?;
        let title: Option<Retained<NSString>> = msg_send![event, title];
        let id = event.eventIdentifier().map_or_else(
            || event.calendarItemIdentifier().to_string(),
            |id| id.to_string(),
        );
        Some(RawCalendarEvent {
            id,
            calendar_id,
            title: title.map(|title| title.to_string()),
            start_s: seconds(&start),
            end_s: seconds(&end),
            occurrence_s: event.occurrenceDate().map(|date| seconds(&date)),
            all_day: event.isAllDay(),
            cancelled: event.status() == EKEventStatus::Canceled,
            url: event.URL().as_deref().and_then(url_string),
            location: event.location().map(|text| text.to_string()),
            notes: event.notes().map(|text| text.to_string()),
            organizer: event
                .organizer()
                .map(|organizer| raw_participant(&organizer)),
            attendees: event
                .attendees()
                .map(|attendees| {
                    attendees
                        .iter()
                        .map(|attendee| raw_participant(&attendee))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

fn raw_participant(participant: &EKParticipant) -> RawParticipant {
    // SAFETY: property reads on a live participant of this call's store.
    unsafe {
        let url: Option<Retained<NSURL>> = msg_send![participant, URL];
        RawParticipant {
            name: participant.name().map(|name| name.to_string()),
            url: url.as_deref().and_then(url_string),
            is_current_user: participant.isCurrentUser(),
            declined: participant.participantStatus() == EKParticipantStatus::Declined,
            kind: match participant.participantType() {
                EKParticipantType::Person => ParticipantKind::Person,
                EKParticipantType::Group => ParticipantKind::Group,
                EKParticipantType::Room => ParticipantKind::Room,
                EKParticipantType::Resource => ParticipantKind::Resource,
                _ => ParticipantKind::Unknown,
            },
        }
    }
}

fn url_string(url: &NSURL) -> Option<String> {
    url.absoluteString().map(|text| text.to_string())
}

fn seconds(date: &NSDate) -> i64 {
    date.timeIntervalSince1970().floor() as i64
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    const FROM: i64 = 1_800_000_000;
    const TO: i64 = FROM + 2 * 86_400;
    const HOUR: i64 = 3_600;

    fn calendar(id: &str, kind: CalendarKind) -> RawCalendar {
        RawCalendar {
            id: id.into(),
            title: format!("Title {id}"),
            source: "Source".into(),
            kind,
            source_kind: Some(SourceKind::CalDav),
            subscribed: false,
            allows_events: true,
        }
    }

    fn person(email: &str, current: bool) -> RawParticipant {
        RawParticipant {
            name: Some(format!("Name {email}")),
            url: Some(format!("mailto:{email}")),
            is_current_user: current,
            declined: false,
            kind: ParticipantKind::Person,
        }
    }

    fn event(id: &str, calendar_id: &str, start_s: i64) -> RawCalendarEvent {
        RawCalendarEvent {
            id: id.into(),
            calendar_id: calendar_id.into(),
            title: Some(format!(" Meeting {id} ")),
            start_s,
            end_s: start_s + HOUR,
            occurrence_s: None,
            all_day: false,
            cancelled: false,
            url: None,
            location: None,
            notes: None,
            organizer: None,
            attendees: Vec::new(),
        }
    }

    /// Synthetic EventKit: records every lookup and query it receives.
    struct FakeStore {
        authorization: CalendarAuthorization,
        calendars: Vec<RawCalendar>,
        events: Vec<RawCalendarEvent>,
        authorization_reads: Cell<usize>,
        lookups: RefCell<Vec<String>>,
        queries: RefCell<Vec<(i64, i64, Vec<String>)>>,
    }

    impl FakeStore {
        fn new(calendars: Vec<RawCalendar>, events: Vec<RawCalendarEvent>) -> Self {
            Self {
                authorization: CalendarAuthorization::Granted,
                calendars,
                events,
                authorization_reads: Cell::new(0),
                lookups: RefCell::default(),
                queries: RefCell::default(),
            }
        }
    }

    impl EventStoreAccess for FakeStore {
        type Calendar = String;

        fn authorization(&self) -> Result<CalendarAuthorization> {
            self.authorization_reads
                .set(self.authorization_reads.get() + 1);
            Ok(self.authorization)
        }

        fn calendar(&self, id: &str) -> Option<(RawCalendar, String)> {
            self.lookups.borrow_mut().push(id.into());
            let calendar = self.calendars.iter().find(|c| c.id == id)?.clone();
            let handle = calendar.id.clone();
            Some((calendar, handle))
        }

        fn events(
            &self,
            from_s: i64,
            to_s: i64,
            calendars: Vec<String>,
        ) -> Result<Vec<RawCalendarEvent>> {
            assert!(!calendars.is_empty(), "queried without calendars");
            self.queries
                .borrow_mut()
                .push((from_s, to_s, calendars.clone()));
            // Like EventKit, return whatever the store has; the policy must filter.
            Ok(self.events.clone())
        }
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn an_empty_selection_never_touches_the_store() {
        let store = FakeStore::new(
            vec![calendar("work", CalendarKind::CalDav)],
            vec![event("e", "work", FROM)],
        );
        for selection in [ids(&[]), ids(&["", "   "])] {
            assert!(
                selected_events(&store, FROM, TO, &selection)
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(store.authorization_reads.get(), 0);
        assert!(store.lookups.borrow().is_empty());
        assert!(store.queries.borrow().is_empty());
    }

    #[test]
    fn a_selection_resolving_to_no_eligible_calendar_never_queries_events() {
        let mut subscribed = calendar("subscribed", CalendarKind::CalDav);
        subscribed.subscribed = true;
        let mut holidays = calendar("holidays", CalendarKind::Local);
        holidays.source_kind = Some(SourceKind::Subscribed);
        let mut reminders = calendar("reminders", CalendarKind::CalDav);
        reminders.allows_events = false;
        let store = FakeStore::new(
            vec![
                calendar("birthdays", CalendarKind::Birthday),
                calendar("feed", CalendarKind::Subscription),
                subscribed,
                holidays,
                reminders,
            ],
            vec![event("e", "birthdays", FROM)],
        );
        let selection = ids(&[
            "missing",
            "birthdays",
            "feed",
            "subscribed",
            "holidays",
            "reminders",
        ]);
        assert!(
            selected_events(&store, FROM, TO, &selection)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.lookups.borrow().len(), 6);
        assert!(store.queries.borrow().is_empty());
    }

    #[test]
    fn only_selected_eligible_calendars_are_queried_and_returned() {
        let store = FakeStore::new(
            vec![
                calendar("work", CalendarKind::Exchange),
                calendar("team", CalendarKind::CalDav),
                calendar("birthdays", CalendarKind::Birthday),
            ],
            vec![
                event("a", "work", FROM + HOUR),
                event("b", "team", FROM + HOUR),
                event("c", "birthdays", FROM + HOUR),
            ],
        );
        let found =
            selected_events(&store, FROM, TO, &ids(&["work", " work ", "birthdays"])).unwrap();
        assert_eq!(*store.queries.borrow(), vec![(FROM, TO, ids(&["work"]))]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].calendar_id, "work");
        assert_eq!(found[0].title.as_deref(), Some("Meeting a"));
    }

    #[test]
    fn windows_must_match_the_sync_range() {
        let store = FakeStore::new(vec![calendar("work", CalendarKind::Local)], Vec::new());
        let work = ids(&["work"]);
        for (from, to) in [
            (FROM, FROM),
            (FROM, FROM - 1),
            (FROM, FROM + MAX_WINDOW_S + 1),
            (i64::MIN, i64::MAX),
        ] {
            let error = selected_events(&store, from, to, &work).unwrap_err();
            assert_eq!(error.code(), "invalid_input");
        }
        assert!(store.queries.borrow().is_empty());
        selected_events(&store, FROM, FROM + MAX_WINDOW_S, &work).unwrap();
        assert_eq!(store.queries.borrow().len(), 1);
    }

    #[test]
    fn missing_read_access_reports_its_code_without_querying() {
        for (authorization, code) in [
            (CalendarAuthorization::Denied, "calendar_denied"),
            (CalendarAuthorization::Restricted, "calendar_restricted"),
            (CalendarAuthorization::WriteOnly, "calendar_write_only"),
            (CalendarAuthorization::NotDetermined, "calendar_pending"),
            (CalendarAuthorization::Unsupported, "unsupported"),
        ] {
            let mut store = FakeStore::new(vec![calendar("work", CalendarKind::Local)], Vec::new());
            store.authorization = authorization;
            let error = selected_events(&store, FROM, TO, &ids(&["work"])).unwrap_err();
            assert_eq!(error.code(), code);
            assert!(store.lookups.borrow().is_empty());
            assert!(store.queries.borrow().is_empty());
        }
    }

    #[test]
    fn non_meetings_and_events_without_the_user_are_left_out() {
        let mut all_day = event("all-day", "work", FROM);
        all_day.all_day = true;
        let mut cancelled = event("cancelled", "work", FROM);
        cancelled.cancelled = true;
        let mut declined = event("declined", "work", FROM);
        let mut me = person("me@example.com", true);
        me.declined = true;
        declined.attendees = vec![me, person("a@example.com", false)];
        let mut others = event("others", "work", FROM);
        others.attendees = vec![
            person("a@example.com", false),
            person("b@example.com", false),
        ];
        let mut organized = event("organized", "work", FROM);
        organized.organizer = Some(person("me@example.com", true));
        organized.attendees = vec![person("a@example.com", false)];
        let mut invited = event("invited", "work", FROM);
        invited.attendees = vec![person("me@example.com", true)];
        let personal = event("personal", "work", FROM);
        let outside = event("outside", "work", TO);
        let mut backwards = event("backwards", "work", FROM);
        backwards.end_s = FROM - 1;
        let store = FakeStore::new(
            vec![calendar("work", CalendarKind::CalDav)],
            vec![
                all_day, cancelled, declined, others, organized, invited, personal, outside,
                backwards,
            ],
        );
        let found = selected_events(&store, FROM, TO, &ids(&["work"])).unwrap();
        let kept: Vec<&str> = found
            .iter()
            .map(|e| e.id.split('@').next().unwrap())
            .collect();
        assert_eq!(kept, ["invited", "organized", "personal"]);
    }

    #[test]
    fn attendees_merge_the_organizer_and_parse_mailto() {
        let mut meeting = event("m", "work", FROM);
        let mut organizer = person("Lead@Example.com", false);
        organizer.name = None;
        meeting.organizer = Some(organizer);
        let mut lead = person("lead@example.com", false);
        lead.name = Some("Lead Person".into());
        let mut encoded = person("x", false);
        encoded.url = Some("MAILTO:first.last%2Btag%40example.org?subject=hi".into());
        encoded.name = Some("  ".into());
        let mut principal = person("x", false);
        principal.url = Some("urn:uuid:0000".into());
        principal.name = Some("No Mail".into());
        let mut room = person("room@example.com", false);
        room.kind = ParticipantKind::Room;
        let mut resource = person("projector@example.com", false);
        resource.kind = ParticipantKind::Resource;
        let mut absent = person("absent@example.com", false);
        absent.declined = true;
        let mut list = person("x", false);
        list.url = Some("mailto:a@example.com,b@example.com".into());
        list.name = None;
        meeting.attendees = vec![
            person("me@example.com", true),
            lead,
            encoded,
            principal,
            room,
            resource,
            absent,
            list,
        ];
        let store = FakeStore::new(vec![calendar("work", CalendarKind::CalDav)], vec![meeting]);
        let found = selected_events(&store, FROM, TO, &ids(&["work"])).unwrap();
        let people: Vec<(Option<&str>, Option<&str>, bool)> = found[0]
            .attendees
            .iter()
            .map(|a| (a.name.as_deref(), a.email.as_deref(), a.is_self))
            .collect();
        assert_eq!(
            people,
            [
                (Some("Lead Person"), Some("Lead@Example.com"), false),
                (Some("Name me@example.com"), Some("me@example.com"), true),
                (None, Some("first.last+tag@example.org"), false),
                (Some("No Mail"), None, false),
            ]
        );
    }

    #[test]
    fn only_links_are_kept_from_url_location_and_notes() {
        let mut meeting = event("m", "work", FROM);
        meeting.url = Some("https://meet.example.com/abc-defg".into());
        meeting.location = Some("Room 4 (https://zoom.example/j/123?pwd=x).".into());
        meeting.notes = Some(
            "Private agenda text.\nJoin: <https://teams.example.com/l/meetup-join/1>, docs \
             HTTPS://docs.example.org/d/2; again https://meet.example.com/abc-defg \
             ftp://files.example.com mailto:x@example.com https:// nothing"
                .into(),
        );
        let store = FakeStore::new(vec![calendar("work", CalendarKind::CalDav)], vec![meeting]);
        let found = selected_events(&store, FROM, TO, &ids(&["work"])).unwrap();
        assert_eq!(
            found[0].links,
            [
                "https://meet.example.com/abc-defg",
                "https://zoom.example/j/123?pwd=x",
                "https://teams.example.com/l/meetup-join/1",
                "HTTPS://docs.example.org/d/2",
            ]
        );
        assert_eq!(found[0].title.as_deref(), Some("Meeting m"));
    }

    #[test]
    fn link_extraction_is_bounded() {
        let mut meeting = event("m", "work", FROM);
        meeting.notes = Some(
            (0..50)
                .map(|i| format!("https://example.com/{i}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
        let store = FakeStore::new(vec![calendar("work", CalendarKind::CalDav)], vec![meeting]);
        let found = selected_events(&store, FROM, TO, &ids(&["work"])).unwrap();
        assert_eq!(found[0].links.len(), MAX_LINKS);
    }

    #[test]
    fn recurring_occurrences_get_distinct_stable_ids() {
        let first = event("series", "work", FROM + HOUR);
        let mut moved = event("series", "work", FROM + 30 * HOUR);
        moved.occurrence_s = Some(FROM + 25 * HOUR);
        let duplicate = first.clone();
        let store = FakeStore::new(
            vec![calendar("work", CalendarKind::CalDav)],
            vec![moved, first, duplicate],
        );
        let found = selected_events(&store, FROM, TO, &ids(&["work"])).unwrap();
        let ids: Vec<&str> = found.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                format!("series@{}", FROM + HOUR),
                format!("series@{}", FROM + 25 * HOUR)
            ]
        );
        assert_eq!(found[1].start_s, FROM + 30 * HOUR);
    }

    #[test]
    fn local_or_external_identifiers_are_never_published_as_ical_uids() {
        // EventKit identifiers (even ones shaped like an iCalendar UID, as an Exchange or
        // CalDAV external identifier may be) are local; only a published UID may merge
        // copies across providers, so matching stays conservative and ambiguous instead.
        for kind in [
            CalendarKind::Exchange,
            CalendarKind::CalDav,
            CalendarKind::Local,
        ] {
            let store = FakeStore::new(
                vec![calendar("work", kind)],
                vec![event(
                    "040000008200E00074C5B7101A82E008@example.invalid",
                    "work",
                    FROM + HOUR,
                )],
            );
            let found = selected_events(&store, FROM, TO, &ids(&["work"])).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].ical_uid, None);
            assert_eq!(found[0].original_start_s, FROM + HOUR);
        }
    }

    #[test]
    fn eligible_calendars_exclude_birthdays_subscriptions_and_reminders() {
        let mut birthdays_source = calendar("contacts", CalendarKind::Local);
        birthdays_source.source_kind = Some(SourceKind::Birthdays);
        let mut subscribed = calendar("subscribed", CalendarKind::CalDav);
        subscribed.subscribed = true;
        let mut reminders = calendar("reminders", CalendarKind::CalDav);
        reminders.allows_events = false;
        let mut home = calendar("home", CalendarKind::Local);
        home.source = "A".into();
        let eligible = eligible_calendars(vec![
            calendar("work", CalendarKind::Exchange),
            calendar("birthdays", CalendarKind::Birthday),
            calendar("feed", CalendarKind::Subscription),
            birthdays_source,
            subscribed,
            reminders,
            home,
        ]);
        let found: Vec<(&str, &str)> = eligible
            .iter()
            .map(|c| (c.id.as_str(), c.source.as_str()))
            .collect();
        assert_eq!(found, [("home", "A"), ("work", "Source")]);
    }

    #[test]
    fn one_prompt_at_a_time_until_the_system_answers() {
        let gate = PromptGate::default();
        let first = gate.try_begin().unwrap();
        assert!(gate.try_begin().is_none());
        first.finish();
        let second = gate.try_begin().unwrap();
        // The first completion is released later; it must not free the second.
        drop(first);
        assert!(gate.try_begin().is_none());
        drop(second);
        assert!(gate.try_begin().is_some());
    }

    #[test]
    fn prompt_answers_never_guess_a_grant() {
        let unexpected = || -> CalendarAuthorization { panic!("status read after a timeout") };
        let error = prompt_answer(Err(RecvTimeoutError::Timeout), unexpected).unwrap_err();
        assert_eq!(error.code(), "calendar_pending");
        assert_eq!(
            prompt_answer(Ok(true), unexpected).unwrap(),
            CalendarAuthorization::Granted
        );
        for answer in [Ok(false), Err(RecvTimeoutError::Disconnected)] {
            assert_eq!(
                prompt_answer(answer, || CalendarAuthorization::WriteOnly).unwrap(),
                CalendarAuthorization::WriteOnly
            );
        }
    }
}
