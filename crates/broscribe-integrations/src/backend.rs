//! Read-only Graph and Google adapters. Pagination never sends credentials to another origin.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::calendar::{Attendee, CalendarEvent, meeting_app};
use crate::config::{Endpoints, GraphDirectory, IntegrationId, IntegrationSettings};
use crate::encode::{build_query, pct_encode};
use crate::http::{Http, Reply};
use crate::{GoogleCalendarStamp, IntegrationError, Result};

#[path = "microsoft.rs"]
mod microsoft;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryPerson {
    pub remote_id: String,
    pub name: Option<String>,
    pub email: Option<String>,
    /// Provider-supplied photo location; not persisted in the library.
    pub photo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::AccountDirectorySource>,
}

/// One Google calendar the signed-in account may read events of (role reader or above).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleCalendar {
    pub id: String,
    pub name: Option<String>,
    pub primary: bool,
    pub access_role: String,
}

/// The Google event fields BroScribe reads; descriptions, locations and attachments are
/// never requested.
const GOOGLE_EVENT_FIELDS: &str = "items(id,status,summary,start,end,visibility,hangoutLink,conferenceData/entryPoints(entryPointType,uri),attendees(email,displayName,self,resource,responseStatus),iCalUID,originalStartTime),nextPageToken";

pub(crate) struct Backend<'a> {
    pub id: IntegrationId,
    pub settings: &'a IntegrationSettings,
    pub endpoints: &'a Endpoints,
    pub http: &'a Http,
}

/// One read of the selected Google calendars.
#[derive(Debug, Default)]
pub(crate) struct GoogleCalendarRead {
    pub events: Vec<CalendarEvent>,
    /// Selected calendars this account can no longer read (deleted, unshared or refused),
    /// kept even when another calendar's read then failed or its credential was rejected.
    pub lost: Vec<String>,
    /// The read stopped at this failure (a credential rejection, a transient or an
    /// unexpected error, not a loss): `events` are then incomplete and must never be
    /// published as the selection's copy.
    pub failure: Option<IntegrationError>,
}

impl GoogleCalendarRead {
    /// The access token was rejected (API 401): the read stopped, the losses stand.
    pub fn rejected(&self) -> bool {
        matches!(self.failure, Some(IntegrationError::ReauthRequired(_)))
    }
}

fn text(v: &Value, field: &str) -> Option<String> {
    v.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
fn date(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(t) = v.get("dateTime").and_then(Value::as_str) {
        DateTime::parse_from_rfc3339(t)
            .map(|t| t.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f")
                    .ok()
                    .map(|t| t.and_utc())
            })
    } else {
        NaiveDate::parse_from_str(v.get("date")?.as_str()?, "%Y-%m-%d")
            .ok()?
            .and_hms_opt(0, 0, 0)
            .map(|t| t.and_utc())
    }
}
fn same_origin(a: &str, b: &str) -> bool {
    let parse = |s: &str| s.parse::<ureq::http::Uri>().ok();
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a.scheme_str() == b.scheme_str() && a.authority() == b.authority(),
        _ => false,
    }
}

/// Graph People search for `query` as one literal: Graph's search language
/// needs quotes around names with spaces and e-mail addresses (`@`), and URL
/// encoding alone does not supply them.
fn graph_people_search_url(base: &str, query: &str) -> String {
    let search = graph_search_literal(query);
    format!(
        "{base}/me/people?{}",
        build_query(&[("$search", &search), ("$top", "100")])
    )
}

/// `query` as a double-quoted search literal, inner quotes and backslashes
/// escaped with a backslash.
fn graph_search_literal(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 2);
    out.push('"');
    for c in query.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

impl Backend<'_> {
    fn get_json(&self, url: &str, token: &str) -> Result<Value> {
        let reply = self.http.get(
            url,
            Some(token),
            &[],
            &[("Prefer", "outlook.timezone=\"UTC\"")],
        )?;
        match reply.status {
            200..=299 => reply.json("provider API"),
            _ => Err(self.response_error(&reply)),
        }
    }

    fn response_error(&self, reply: &Reply) -> IntegrationError {
        let rate_limited = self.id == IntegrationId::Google
            && reply.status == 403
            && serde_json::from_slice::<Value>(&reply.body)
                .ok()
                .is_some_and(|body| {
                    body.pointer("/error/errors")
                        .and_then(Value::as_array)
                        .is_some_and(|errors| {
                            errors.iter().any(|error| {
                                matches!(
                                    error.get("reason").and_then(Value::as_str),
                                    Some("rateLimitExceeded" | "userRateLimitExceeded")
                                )
                            })
                        })
                });
        match reply.status {
            401 => IntegrationError::ReauthRequired(self.id.to_string()),
            403 if !rate_limited => IntegrationError::Forbidden(self.id.to_string()),
            404 if self.id == IntegrationId::Google => {
                IntegrationError::Core(broscribe_core::Error::NotFound("Google resource".into()))
            }
            403 | 429 | 500..=599 => {
                IntegrationError::Transient(format!("provider API HTTP {}", reply.status))
            }
            _ => IntegrationError::Service(format!("provider API HTTP {}", reply.status)),
        }
    }

    fn pages(
        &self,
        initial: String,
        base: &str,
        token: &str,
        collection: &str,
    ) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut url = initial.clone();
        let mut seen = std::collections::HashSet::new();
        loop {
            if !same_origin(&url, base) || !seen.insert(url.clone()) {
                return Err(crate::service::service_error(
                    "invalid_pagination",
                    "invalid provider pagination URL",
                ));
            }
            if seen.len() > 100 || out.len() > 10_000 {
                return Err(crate::service::service_error(
                    "pagination_limit",
                    "provider pagination exceeded its bound",
                ));
            }
            let page = self.get_json(&url, token)?;
            if self.id.is_graph() && !page.get(collection).is_some_and(Value::is_array) {
                return Err(crate::service::service_error(
                    "invalid_provider_response",
                    "provider collection is missing or invalid",
                ));
            }
            if let Some(items) = page.get(collection).and_then(Value::as_array) {
                out.extend(items.iter().cloned());
                if out.len() > 10_000 {
                    return Err(crate::service::service_error(
                        "pagination_limit",
                        "provider collection exceeded its bound",
                    ));
                }
            }
            if self.id.is_graph() {
                let Some(next) = text(&page, "@odata.nextLink") else {
                    break;
                };
                url = next;
            } else {
                let Some(next) = text(&page, "nextPageToken") else {
                    break;
                };
                url = format!("{initial}&{}", build_query(&[("pageToken", &next)]));
            }
        }
        Ok(out)
    }

    fn google_only(&self) -> Result<&str> {
        if self.id != IntegrationId::Google {
            return Err(crate::service::service_error(
                "account_required",
                "Microsoft calendar reads require an explicit account and route",
            ));
        }
        Ok(self.endpoints.api.trim_end_matches('/'))
    }

    /// The signed-in Google account as the calendar read scope reports it: the primary
    /// calendar's identifier is the account address. No profile or e-mail scope is used.
    pub fn google_identity(&self, token: &str) -> Result<crate::AccountInfo> {
        let base = self.google_only()?;
        let primary = self.get_json(
            &format!(
                "{base}/calendars/primary?{}",
                build_query(&[("fields", "id,summary")])
            ),
            token,
        )?;
        let email = text(&primary, "id")
            .filter(|id| id.contains('@'))
            .map(|id| id.to_lowercase())
            .ok_or_else(|| {
                crate::service::service_error(
                    "account_identity_unavailable",
                    "Google did not report the signed-in account",
                )
            })?;
        let name = text(&primary, "summary").filter(|s| !s.eq_ignore_ascii_case(&email));
        Ok(crate::AccountInfo {
            name,
            email: Some(email),
        })
    }

    /// Calendars of the signed-in Google account whose events it may read.
    pub fn google_calendars(&self, token: &str) -> Result<Vec<GoogleCalendar>> {
        let base = self.google_only()?;
        let url = format!(
            "{base}/users/me/calendarList?{}",
            build_query(&[
                ("minAccessRole", "reader"),
                ("maxResults", "250"),
                (
                    "fields",
                    "items(id,summary,summaryOverride,primary,accessRole),nextPageToken"
                ),
            ])
        );
        Ok(self
            .pages(url, &self.endpoints.api, token, "items")?
            .into_iter()
            .filter_map(|v| {
                let access_role = text(&v, "accessRole")?;
                if !matches!(access_role.as_str(), "reader" | "writer" | "owner") {
                    return None;
                }
                Some(GoogleCalendar {
                    id: text(&v, "id")?,
                    name: text(&v, "summaryOverride").or_else(|| text(&v, "summary")),
                    primary: v.get("primary").and_then(Value::as_bool) == Some(true),
                    access_role,
                })
            })
            .collect())
    }

    /// Events of the explicitly selected Google calendars, stamped with the account and
    /// selection generations they are read under. Only the fields used below are requested.
    /// A selected calendar Google no longer has for this account (404) or refuses (403) is
    /// reported lost while the others are still read. Any other failure, a rejected access
    /// token included, stops the read and is returned in [`GoogleCalendarRead::failure`]
    /// beside the losses already observed.
    /// When no calendar was read and one was refused under an accepted token, the account's
    /// calendar list decides: refused there too means the account's calendar access itself
    /// was withdrawn (`Forbidden`); a list that cannot be read is a failure that keeps the
    /// losses, and a rejected token there replaces an earlier failure.
    /// Only that withdrawal is an `Err`.
    pub fn calendar(
        &self,
        token: &str,
        calendar_ids: &[String],
        stamp: GoogleCalendarStamp,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<GoogleCalendarRead> {
        let base = self.google_only()?;
        let from = start.to_rfc3339();
        let to = end.to_rfc3339();
        let mut read = GoogleCalendarRead::default();
        let mut any_read = false;
        let mut refused = false;
        for calendar_id in calendar_ids {
            let url = format!(
                "{base}/calendars/{}/events?{}",
                pct_encode(calendar_id),
                build_query(&[
                    ("timeMin", &from),
                    ("timeMax", &to),
                    ("singleEvents", "true"),
                    ("maxResults", "250"),
                    ("fields", GOOGLE_EVENT_FIELDS),
                ])
            );
            match self.pages(url, &self.endpoints.api, token, "items") {
                Ok(items) => {
                    any_read = true;
                    read.events
                        .extend(items.iter().filter_map(|v| self.google_event(v, stamp)));
                }
                Err(IntegrationError::Forbidden(_)) => {
                    refused = true;
                    read.lost.push(calendar_id.clone());
                }
                Err(IntegrationError::Core(broscribe_core::Error::NotFound(_))) => {
                    read.lost.push(calendar_id.clone());
                }
                // A rejected token (401) included: the losses stay with the read.
                Err(error) => {
                    read.failure = Some(error);
                    break;
                }
            }
        }
        if refused && !any_read && !read.rejected() {
            match self.google_calendars(token) {
                Ok(_) => {}
                Err(error @ IntegrationError::Forbidden(_)) => return Err(error),
                // A rejected token is retried with fresh credentials whatever failed before.
                Err(error @ IntegrationError::ReauthRequired(_)) => read.failure = Some(error),
                // A failure seen earlier stays the one reported.
                Err(error) => {
                    if read.failure.is_none() {
                        read.failure = Some(error);
                    }
                }
            }
        }
        Ok(read)
    }

    fn google_event(&self, v: &Value, stamp: GoogleCalendarStamp) -> Option<CalendarEvent> {
        if v.get("status").and_then(Value::as_str) == Some("cancelled")
            || v.pointer("/start/date").is_some()
            || matches!(
                v.get("visibility").and_then(Value::as_str),
                Some("private" | "confidential")
            )
            || v.get("attendees")
                .and_then(Value::as_array)
                .is_some_and(|attendees| {
                    attendees.iter().any(|a| {
                        a.get("self").and_then(Value::as_bool) == Some(true)
                            && a.get("responseStatus").and_then(Value::as_str) == Some("declined")
                    })
                })
        {
            return None;
        }
        let id = text(v, "id")?;
        let start = v.get("start").and_then(date)?;
        let end = v.get("end").and_then(date)?;
        let mut links: Vec<String> = text(v, "hangoutLink").into_iter().collect();
        for entry in v
            .pointer("/conferenceData/entryPoints")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if entry.get("entryPointType").and_then(Value::as_str) == Some("video")
                && let Some(l) = text(entry, "uri")
                && !links.contains(&l)
            {
                links.push(l);
            }
        }
        let attendees = v
            .get("attendees")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|a| {
                a.get("resource").and_then(Value::as_bool) != Some(true)
                    && a.get("responseStatus").and_then(Value::as_str) != Some("declined")
            })
            .map(|a| Attendee {
                name: text(a, "displayName"),
                email: text(a, "email"),
                is_self: a.get("self").and_then(Value::as_bool).unwrap_or(false),
            })
            .collect();
        Some(CalendarEvent {
            id,
            integration: self.id,
            title: text(v, "summary"),
            start,
            end,
            app: meeting_app(&links),
            join_links: links,
            attendees,
            account_generation: None,
            remote: None,
            google: Some(stamp),
            ical_uid: text(v, "iCalUID"),
            occurrence_id: match v.get("originalStartTime") {
                Some(original) => date(original),
                None => Some(start),
            }
            .map(|original| original.to_rfc3339()),
        })
    }

    pub fn directory(&self, token: &str, query: &str) -> Result<Vec<DirectoryPerson>> {
        let base = self.endpoints.directory_api.trim_end_matches('/');
        let url = if self.id.is_graph() {
            if self.settings.directory == GraphDirectory::PeopleRead {
                graph_people_search_url(base, query)
            } else {
                let escaped = query.replace('\'', "''");
                let filter =
                    format!("startswith(displayName,'{escaped}') or startswith(mail,'{escaped}')");
                format!(
                    "{base}/users?{}",
                    build_query(&[
                        ("$filter", &filter),
                        ("$select", "id,displayName,mail,userPrincipalName"),
                        ("$top", "100")
                    ])
                )
            }
        } else {
            format!(
                "{base}/people:searchDirectoryPeople?{}",
                build_query(&[
                    ("query", query),
                    ("readMask", "names,emailAddresses,photos"),
                    ("sources", "DIRECTORY_SOURCE_TYPE_DOMAIN_PROFILE"),
                    ("pageSize", "100")
                ])
            )
        };
        let items = self.pages(
            url,
            base,
            token,
            if self.id.is_graph() {
                "value"
            } else {
                "people"
            },
        )?;
        Ok(items
            .into_iter()
            .filter_map(|v| {
                if self.id.is_graph() {
                    let remote_id = text(&v, "id")?;
                    let email = text(&v, "mail")
                        .or_else(|| text(&v, "userPrincipalName"))
                        .or_else(|| {
                            v.pointer("/scoredEmailAddresses/0/address")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        });
                    let photo_url = (self.settings.directory == GraphDirectory::UserReadBasicAll)
                        .then(|| format!("{base}/users/{}/photo/$value", pct_encode(&remote_id)));
                    Some(DirectoryPerson {
                        remote_id,
                        name: text(&v, "displayName"),
                        email,
                        photo_url,
                        source: None,
                    })
                } else {
                    Some(DirectoryPerson {
                        source: None,
                        remote_id: text(&v, "resourceName")?,
                        name: v
                            .pointer("/names/0/displayName")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        email: v
                            .pointer("/emailAddresses/0/value")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        photo_url: v
                            .pointer("/photos/0/url")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    })
                }
            })
            .collect())
    }

    pub fn photo(&self, token: &str, person: &DirectoryPerson) -> Result<Option<Vec<u8>>> {
        let Some(url) = person.photo_url.as_deref() else {
            return Ok(None);
        };
        let own_origin = same_origin(url, &self.endpoints.directory_api);
        // Google serves profile images from a separate public image CDN. Never send its bearer token there.
        let google_cdn = !self.id.is_graph()
            && url.parse::<ureq::http::Uri>().ok().is_some_and(|u| {
                u.scheme_str() == Some("https")
                    && u.host().is_some_and(|h| {
                        h == "googleusercontent.com" || h.ends_with(".googleusercontent.com")
                    })
            });
        if !own_origin && !google_cdn {
            return Err(IntegrationError::Service("unexpected photo origin".into()));
        }
        let r = self.http.get(url, own_origin.then_some(token), &[], &[])?;
        match r.status {
            200 => Ok(Some(r.body)),
            404 => Ok(None),
            _ => Err(self.response_error(&r)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::pct_decode;

    fn search_of(url: &str) -> String {
        let (path, query) = url.split_once('?').unwrap();
        assert_eq!(path, "https://graph.example.invalid/v1.0/me/people");
        let (key, value) = query.split('&').next().unwrap().split_once('=').unwrap();
        assert_eq!(key, "%24search");
        pct_decode(value).unwrap()
    }

    #[test]
    fn graph_people_search_sends_quoted_escaped_literals() {
        let base = "https://graph.example.invalid/v1.0";
        let name = graph_people_search_url(base, "Mira Example");
        assert_eq!(
            name,
            format!("{base}/me/people?%24search=%22Mira%20Example%22&%24top=100")
        );
        assert_eq!(search_of(&name), "\"Mira Example\"");
        let email = graph_people_search_url(base, "mira@example.invalid");
        assert_eq!(search_of(&email), "\"mira@example.invalid\"");
        assert!(email.contains("%22mira%40example.invalid%22"));
        let odd = graph_people_search_url(base, r#"Mi"ra \ Ex"#);
        assert_eq!(search_of(&odd), r#""Mi\"ra \\ Ex""#);
    }
}
