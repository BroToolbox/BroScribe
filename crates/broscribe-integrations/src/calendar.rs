//! Provider-independent calendar data and non-exclusive identity priors.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::IntegrationId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attendee {
    pub name: Option<String>,
    pub email: Option<String>,
    #[serde(default)]
    pub is_self: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub integration: IntegrationId,
    pub title: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    #[serde(default)]
    pub join_links: Vec<String>,
    pub app: Option<String>,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
    /// Native EventKit events carry the opt-in generation they were read under; consumers
    /// accept them only while the helper reports that same live generation.
    /// Remote providers leave this native stamp empty; Microsoft requires `remote` and
    /// Google `google` below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_generation: Option<u64>,
    /// Verified account, selected route and both authority generations. Old unstamped
    /// Microsoft events are not eligible recording priors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<crate::RemoteCalendarStamp>,
    /// Google account and calendar-selection generations the event was read under. Older
    /// Google copies stamped only with `account_generation` are not eligible priors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub google: Option<crate::GoogleCalendarStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ical_uid: Option<String>,
    /// Canonical UTC RFC 3339 original occurrence start, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarCache {
    pub events: Vec<CalendarEvent>,
}

impl CalendarCache {
    pub fn prune(&mut self, now: DateTime<Utc>) {
        self.events.retain(|e| e.end >= now - Duration::days(30));
    }

    /// Atomically writes a copy (a recording's calendar prior) readable only by
    /// the user: mode 0600 for the temporary and published file on Unix.
    pub fn save_private(&self, path: &std::path::Path) -> crate::Result<()> {
        crate::store::write_json_private(path, self)
    }

    /// Calendar meetings overlapping the recording `[start, end]`, with 15 minutes of
    /// tolerance on each side. Ranks a matching join link, then the meeting app, then the
    /// overlap with the recording, then the distance between the starts. Only events with
    /// provenance are eligible: Microsoft events need their verified account stamp, Google
    /// events their account/selection pair and native events their generation stamp.
    /// Equally ranked events come back together
    /// only as verified copies of one occurrence (same published iCalendar UID, occurrence
    /// and start) with identical hints; anything else is `calendar_match_ambiguous`. A
    /// title alone never merges events.
    pub fn match_recording(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        join_link: Option<&str>,
        app: Option<&str>,
    ) -> crate::Result<Option<Vec<&CalendarEvent>>> {
        if end < start {
            return Err(crate::IntegrationError::Core(
                broscribe_core::Error::InvalidInput(
                    "a recording cannot end before it starts".into(),
                ),
            ));
        }
        let tolerance = Duration::minutes(15);
        let rank = |e: &CalendarEvent| {
            (
                join_link.is_some_and(|link| {
                    e.join_links
                        .iter()
                        .any(|j| j.trim_end_matches('/') == link.trim_end_matches('/'))
                }),
                app.zip(e.app.as_deref())
                    .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b)),
                e.end
                    .min(end)
                    .signed_duration_since(e.start.max(start))
                    .num_seconds()
                    .max(0),
                -start.signed_duration_since(e.start).num_seconds().abs(),
            )
        };
        let eligible: Vec<_> = self
            .events
            .iter()
            .filter(|e| {
                e.has_provenance() && e.start - tolerance <= end && e.end + tolerance >= start
            })
            .collect();
        let Some(best) = eligible.iter().map(|e| rank(e)).max() else {
            return Ok(None);
        };
        let tied: Vec<_> = eligible.into_iter().filter(|e| rank(e) == best).collect();
        let first = tied[0];
        let hints = |e: &CalendarEvent| {
            let mut attendees: Vec<_> = e
                .attendees
                .iter()
                .map(|a| {
                    (
                        a.name.clone(),
                        a.email.as_ref().map(|s| s.to_lowercase()),
                        a.is_self,
                    )
                })
                .collect();
            attendees.sort();
            let mut links = e.join_links.clone();
            links.sort();
            (e.title.clone(), attendees, links, e.app.clone(), e.end)
        };
        if tied.iter().skip(1).any(|event| {
            let same_meeting = first
                .ical_uid
                .as_ref()
                .zip(event.ical_uid.as_ref())
                .is_some_and(|(a, b)| a == b)
                && first
                    .occurrence_id
                    .as_ref()
                    .zip(event.occurrence_id.as_ref())
                    .is_some_and(|(a, b)| a == b)
                && first.start == event.start;
            !same_meeting || hints(first) != hints(event)
        }) {
            return Err(crate::service::service_error(
                "calendar_match_ambiguous",
                "equally ranked calendar meetings or copies have conflicting hints",
            ));
        }
        Ok(Some(tied))
    }
}

impl CalendarEvent {
    /// Stamped by the source it was read from: Microsoft with its verified account route,
    /// Google with its account and selection generations, native calendars with the
    /// generation they were read under.
    fn has_provenance(&self) -> bool {
        match self.integration {
            IntegrationId::Google => self.google.is_some(),
            id if id.is_graph() => self.remote.is_some(),
            _ => self.account_generation.is_some(),
        }
    }
}

pub use broscribe_core::schema::TitleSource;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarParticipant {
    pub name: Option<String>,
    pub email: Option<String>,
    pub is_self: bool,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarPrior {
    pub participants: Vec<CalendarParticipant>,
    pub hotwords: Vec<String>,
    pub title: Option<String>,
    pub title_source: TitleSource,
}

impl CalendarEvent {
    /// A native-calendar event of `integration` read under its opt-in `generation`. Only
    /// extracted links and eligible participants arrive from the platform; bodies never do.
    pub(crate) fn from_native(
        integration: IntegrationId,
        event: broscribe_platform::SystemCalendarEvent,
        generation: u64,
    ) -> Option<Self> {
        let start = DateTime::from_timestamp(event.start_s, 0)?;
        let end = DateTime::from_timestamp(event.end_s, 0)?;
        if end < start {
            return None;
        }
        Some(Self {
            id: event.id,
            integration,
            title: event.title.filter(|title| !title.trim().is_empty()),
            start,
            end,
            app: meeting_app(&event.links),
            join_links: event.links,
            attendees: event
                .attendees
                .into_iter()
                .map(|a| Attendee {
                    name: a.name,
                    email: a.email,
                    is_self: a.is_self,
                })
                .collect(),
            account_generation: Some(generation),
            remote: None,
            google: None,
            ical_uid: event.ical_uid,
            occurrence_id: Some(DateTime::from_timestamp(event.original_start_s, 0)?.to_rfc3339()),
        })
    }

    /// Returns additions, never a participant allow-list. A caller merges them with its roster.
    /// Only titles derived from a filename can be replaced, never user or generated titles.
    pub fn prior(&self, current_title: Option<&str>, source: TitleSource) -> CalendarPrior {
        let title = if source == TitleSource::File {
            self.title
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
        } else {
            None
        };
        let title_source = if title.is_some() {
            TitleSource::Calendar
        } else {
            source
        };
        CalendarPrior {
            participants: self
                .attendees
                .iter()
                .map(|a| CalendarParticipant {
                    name: a.name.clone(),
                    email: a.email.clone(),
                    is_self: a.is_self,
                    evidence: vec!["calendar".into()],
                })
                .collect(),
            hotwords: self
                .attendees
                .iter()
                .filter_map(|a| a.name.clone())
                .collect(),
            title: title.or_else(|| current_title.map(str::to_owned)),
            title_source,
        }
    }

    /// Applies an additive prior to a meeting already locked by the caller. No network or I/O.
    /// Existing participants, assignments, person IDs and user titles are never removed.
    pub fn apply_to(&self, meeting: &mut broscribe_core::schema::Meeting) {
        use broscribe_core::schema::Participant;
        let prior = self.prior(
            meeting.title.as_deref(),
            meeting.title_source.unwrap_or_default(),
        );
        meeting.title = prior.title;
        meeting.title_source = Some(prior.title_source);
        for attendee in prior.participants {
            let existing = meeting.participants.iter_mut().find(|p| {
                match (p.email.as_deref(), attendee.email.as_deref()) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    (None, None) => p
                        .name
                        .as_deref()
                        .zip(attendee.name.as_deref())
                        .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b)),
                    _ => false,
                }
            });
            if let Some(p) = existing {
                if !p.evidence.iter().any(|e| e == "calendar") {
                    p.evidence.push("calendar".into());
                }
                if p.name.is_none() {
                    p.name = attendee.name;
                }
            } else {
                meeting.participants.push(Participant {
                    name: attendee.name,
                    email: attendee.email,
                    is_self: attendee.is_self,
                    evidence: attendee.evidence,
                    ..Participant::default()
                });
            }
        }
        for name in prior.hotwords {
            if !meeting
                .hotwords
                .iter()
                .any(|w| w.eq_ignore_ascii_case(&name))
            {
                meeting.hotwords.push(name);
            }
        }
    }
}

pub(crate) fn meeting_app(links: &[String]) -> Option<String> {
    links.iter().find_map(|l| {
        let host = l
            .split_once("://")?
            .1
            .split('/')
            .next()?
            .split(':')
            .next()?;
        if host == "teams.microsoft.com" || host == "teams.live.com" {
            Some("teams")
        } else if host == "meet.google.com" {
            Some("meet")
        } else if host == "zoom.us" || host.ends_with(".zoom.us") {
            Some("zoom")
        } else if host == "webex.com" || host.ends_with(".webex.com") {
            Some("webex")
        } else {
            None
        }
        .map(str::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(id: &str, start: DateTime<Utc>, link: &str) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            integration: IntegrationId::Google,
            title: Some("Synthetic planning".into()),
            start,
            end: start + Duration::hours(1),
            join_links: vec![link.into()],
            app: Some("meet".into()),
            attendees: vec![Attendee {
                name: Some("Mira Example".into()),
                email: Some("mira@example.invalid".into()),
                is_self: false,
            }],
            account_generation: None,
            remote: None,
            google: Some(crate::GoogleCalendarStamp {
                generation: 1,
                selection_generation: 1,
            }),
            ical_uid: None,
            occurrence_id: None,
        }
    }
    #[test]
    fn matching_prefers_links_and_prior_never_replaces_user_title() {
        let now = Utc::now();
        let cache = CalendarCache {
            events: vec![
                event("near", now, "https://meet.google.com/aaa"),
                event(
                    "linked",
                    now - Duration::minutes(10),
                    "https://meet.google.com/bbb",
                ),
            ],
        };
        let found = cache
            .match_recording(
                now,
                now + Duration::minutes(40),
                Some("https://meet.google.com/bbb"),
                Some("meet"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(found.len(), 1);
        let found = found[0];
        assert_eq!(found.id, "linked");
        let p = found.prior(Some("My chosen title"), TitleSource::User);
        assert_eq!(p.title.as_deref(), Some("My chosen title"));
        assert_eq!(p.participants[0].evidence, ["calendar"]);
        assert_eq!(
            found
                .prior(Some("recording.wav"), TitleSource::File)
                .title_source,
            TitleSource::Calendar
        );
        assert!(
            cache
                .match_recording(
                    now - Duration::hours(3),
                    now - Duration::hours(2),
                    None,
                    None
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(
            cache
                .match_recording(now, now - Duration::seconds(1), None, None)
                .unwrap_err()
                .code(),
            "invalid_input"
        );
    }
    #[test]
    fn interval_matching_picks_the_meeting_covering_the_recording_not_the_one_ending_at_its_start()
    {
        let ten = Utc::now();
        let mut earlier = event("earlier", ten - Duration::hours(1), "");
        earlier.title = Some("Earlier meeting".into());
        let mut later = event("later", ten, "");
        later.title = Some("Later meeting".into());
        let cache = CalendarCache {
            events: vec![earlier.clone(), later],
        };
        // Started just before the boundary, ran through most of the later meeting.
        let found = cache
            .match_recording(
                ten - Duration::minutes(2),
                ten + Duration::minutes(55),
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            found.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["later"]
        );
        // Same title, distinct meetings without verified identity: never merged.
        let mut twin = earlier.clone();
        twin.id = "twin".into();
        twin.integration = IntegrationId::SystemCalendar;
        twin.google = None;
        twin.account_generation = Some(1);
        let cache = CalendarCache {
            events: vec![earlier.clone(), twin],
        };
        assert_eq!(
            cache
                .match_recording(earlier.start, earlier.end, None, None)
                .unwrap_err()
                .code(),
            "calendar_match_ambiguous"
        );
        // Events without their source's provenance never match: a Google copy stamped only
        // with a pre-selection-generation scalar fails closed, as does an unstamped native one.
        let mut legacy = earlier.clone();
        legacy.google = None;
        legacy.account_generation = Some(1);
        let mut native = earlier.clone();
        native.integration = IntegrationId::SystemCalendar;
        native.google = None;
        for unstamped in [legacy, native] {
            assert!(
                CalendarCache {
                    events: vec![unstamped]
                }
                .match_recording(earlier.start, earlier.end, None, None)
                .unwrap()
                .is_none()
            );
        }
    }
    #[test]
    fn applying_prior_preserves_roster_and_user_title_and_is_idempotent() {
        use broscribe_core::{
            MeetingId,
            schema::{Meeting, Participant, Source},
        };
        let now = Utc::now();
        let mut meeting = Meeting::new(
            MeetingId::parse("m-abc123").unwrap(),
            Source::Import,
            now.fixed_offset(),
        );
        meeting.title = Some("A user title".into());
        meeting.title_source = Some(TitleSource::User);
        meeting.participants.push(Participant {
            name: Some("Unlisted guest".into()),
            evidence: vec!["roster".into()],
            ..Default::default()
        });
        let event = event("synthetic", now, "https://meet.google.com/fake");
        event.apply_to(&mut meeting);
        event.apply_to(&mut meeting);
        assert_eq!(meeting.title.as_deref(), Some("A user title"));
        assert_eq!(meeting.participants.len(), 2);
        assert_eq!(meeting.participants[0].evidence, ["roster"]);
        assert_eq!(meeting.participants[1].evidence, ["calendar"]);
        assert_eq!(meeting.hotwords, ["Mira Example"]);
        meeting.title_source = Some(TitleSource::File);
        event.apply_to(&mut meeting);
        assert_eq!(meeting.title.as_deref(), Some("Synthetic planning"));
        assert_eq!(meeting.title_source, Some(TitleSource::Calendar));
    }
    #[test]
    fn cache_retains_only_thirty_days() {
        let now = Utc::now();
        let mut c = CalendarCache {
            events: vec![
                event("old", now - Duration::days(32), ""),
                event("recent", now, ""),
            ],
        };
        c.prune(now);
        assert_eq!(c.events.len(), 1);
        assert_eq!(c.events[0].id, "recent");
    }
    #[test]
    fn matching_retains_copy_provenance_and_rejects_distinct_or_conflicting_ties_in_any_order() {
        let now = Utc::now();
        let mut a = event("same-event", now, "https://meet.google.com/synthetic");
        a.integration = IntegrationId::MicrosoftGraph;
        a.google = None;
        a.ical_uid = Some("meeting-uid".into());
        a.occurrence_id = Some(now.to_rfc3339());
        a.remote = Some(crate::RemoteCalendarStamp {
            account: crate::AccountRef {
                integration: crate::RemoteIntegration::MicrosoftGraph,
                account_id: crate::AccountId::parse("11111111111111111111111111111111").unwrap(),
            },
            generation: 1,
            selection_generation: 1,
            target: crate::CalendarTarget::Own {
                calendar_id: "work".into(),
            },
        });
        let mut b = a.clone();
        b.remote.as_mut().unwrap().account.account_id =
            crate::AccountId::parse("22222222222222222222222222222222").unwrap();
        for reverse in [false, true] {
            let mut events = vec![a.clone(), b.clone()];
            if reverse {
                events.reverse();
            }
            let cache = CalendarCache { events };
            let copies = cache
                .match_recording(now, now, None, None)
                .unwrap()
                .unwrap();
            assert_eq!(copies.len(), 2);
            assert_ne!(copies[0].remote, copies[1].remote);
            for conflict in [false, true] {
                let mut other = b.clone();
                if conflict {
                    other.title = Some("Conflicting hint".into());
                } else {
                    other.ical_uid = Some("another-meeting".into());
                }
                let mut events = vec![a.clone(), other];
                if reverse {
                    events.reverse();
                }
                let cache = CalendarCache { events };
                assert_eq!(
                    cache
                        .match_recording(now, now, None, None)
                        .unwrap_err()
                        .code(),
                    "calendar_match_ambiguous"
                );
            }
        }
        let mut legacy = a;
        legacy.remote = None;
        assert!(
            CalendarCache {
                events: vec![legacy]
            }
            .match_recording(now, now, None, None)
            .unwrap()
            .is_none()
        );
    }
}
